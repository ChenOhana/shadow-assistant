//! The capture log: an append-only JSONL file and the ground truth for everything.
//!
//! Crash safety rests on three rules:
//!
//! 1. **One writer.** `brain run` holds an exclusive lock on the data directory
//!    (see [`crate::datadir`]), so only one process ever appends.
//! 2. **Whole lines or nothing.** Each event is serialized in memory and appended
//!    with a single `write(2)` call. After a `kill -9` the kernel still holds
//!    everything already written (the page cache belongs to the kernel, not to
//!    the process). The worst case is a partial last line from a write that was
//!    interrupted. [`LogWriter::open`] truncates such a torn tail before
//!    appending anything new, so the file is always a sequence of complete lines.
//! 3. **Readers only trust newline-terminated lines.** [`LogReader`] never
//!    returns a line without its trailing `\n`. Concurrent readers therefore
//!    never see a line that is still being written.
//!
//! `fdatasync` runs in batches (see [`LogWriter::sync`]). It only matters for
//! power loss or a kernel crash, not for a process being killed.

use crate::event::Event;
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The only writer of a capture log (see the module docs for the crash-safety rules).
pub struct LogWriter {
    file: File,
    path: PathBuf,
    buf: Vec<u8>,
    len: u64,
    dirty: bool,
}

impl LogWriter {
    /// Opens (creating if needed) the log for appending, repairing a torn tail.
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("opening capture log {}", path.display()))?;
        let len = repair_torn_tail(&mut file)
            .with_context(|| format!("repairing capture log {}", path.display()))?;
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file,
            path: path.to_owned(),
            buf: Vec::with_capacity(512),
            len,
            dirty: false,
        })
    }

    /// Appends one event as one line with a single `write(2)` call.
    pub fn append(&mut self, event: &Event) -> Result<()> {
        event.to_line(&mut self.buf);
        self.file
            .write_all(&self.buf)
            .with_context(|| format!("appending to {}", self.path.display()))?;
        self.len += self.buf.len() as u64;
        self.dirty = true;
        Ok(())
    }

    /// Flushes appended data to stable storage, if anything changed since the last sync.
    pub fn sync(&mut self) -> Result<()> {
        if self.dirty {
            self.file.sync_data().context("fdatasync on capture log")?;
            self.dirty = false;
        }
        Ok(())
    }

    /// Current length of the log in bytes, always at a line boundary.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the log has no events yet.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Truncates anything after the last `\n` and returns the new file length.
fn repair_torn_tail(file: &mut File) -> Result<u64> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(0);
    }
    // Scan backwards in chunks for the last newline.
    const CHUNK: u64 = 64 * 1024;
    let mut end = len;
    let mut chunk = vec![0u8; CHUNK as usize];
    loop {
        let start = end.saturating_sub(CHUNK);
        let n = (end - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk[..n])?;
        if let Some(pos) = chunk[..n].iter().rposition(|&b| b == b'\n') {
            let good_len = start + pos as u64 + 1;
            if good_len != len {
                file.set_len(good_len)?;
                file.sync_all()?;
            }
            return Ok(good_len);
        }
        if start == 0 {
            // No newline anywhere: the file holds a single torn line.
            file.set_len(0)?;
            file.sync_all()?;
            return Ok(0);
        }
        end = start;
    }
}

/// One complete line read from the log.
#[derive(Debug)]
pub struct LogLine {
    /// Byte offset of the start of this line.
    pub offset: u64,
    /// The parsed event, or `None` if the line is not a valid event. That cannot
    /// happen for logs written by `brain run`, but a reader must not choke on it.
    pub event: Option<Event>,
}

/// Sequential reader over complete lines, starting from any line boundary.
pub struct LogReader {
    reader: BufReader<File>,
    offset: u64,
    line: Vec<u8>,
}

impl LogReader {
    /// Opens the log at `path` and positions the reader at `offset`, which must
    /// be a line boundary (0, or a value previously returned by [`Self::offset`]).
    pub fn open(path: &Path, offset: u64) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("opening capture log {}", path.display()))?;
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        reader.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            reader,
            offset,
            line: Vec::with_capacity(512),
        })
    }

    /// Returns the next complete line, or `None` at the end of complete data.
    /// A trailing partial line (one still being written) is left unread, and
    /// the next call tries it again.
    pub fn next_line(&mut self) -> Result<Option<LogLine>> {
        self.line.clear();
        let n = self.reader.read_until(b'\n', &mut self.line)?;
        if n == 0 {
            return Ok(None);
        }
        if self.line.last() != Some(&b'\n') {
            // Partial line: rewind so a later call re-reads it whole.
            self.reader.seek(SeekFrom::Start(self.offset))?;
            return Ok(None);
        }
        let offset = self.offset;
        self.offset += n as u64;
        let event = serde_json::from_slice::<Event>(&self.line[..n - 1]).ok();
        Ok(Some(LogLine { offset, event }))
    }

    /// Offset just past the last complete line returned.
    pub fn offset(&self) -> u64 {
        self.offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventKind, Mod};

    fn key(ts: i64, k: &str) -> Event {
        Event {
            ts,
            app: "app".into(),
            kind: EventKind::Key {
                key: k.into(),
                mods: vec![Mod::Shift],
                repeat: false,
                window: "w".into(),
            },
        }
    }

    fn read_all(path: &Path, from: u64) -> (Vec<Event>, u64) {
        let mut r = LogReader::open(path, from).unwrap();
        let mut out = Vec::new();
        while let Some(l) = r.next_line().unwrap() {
            out.push(l.event.unwrap());
        }
        (out, r.offset())
    }

    #[test]
    fn append_then_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        let mut w = LogWriter::open(&p).unwrap();
        for i in 0..100 {
            w.append(&key(i, "a")).unwrap();
        }
        w.sync().unwrap();
        let (evs, off) = read_all(&p, 0);
        assert_eq!(evs.len(), 100);
        assert_eq!(off, w.len());
        assert_eq!(evs[42].ts, 42);
    }

    #[test]
    fn torn_tail_is_repaired_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        {
            let mut w = LogWriter::open(&p).unwrap();
            w.append(&key(1, "a")).unwrap();
            w.append(&key(2, "b")).unwrap();
        }
        let good_len = std::fs::metadata(&p).unwrap().len();
        // Simulate kill -9 in the middle of a write.
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"{\"ts\":\"2026-11-08T14:0").unwrap();
        drop(f);

        let mut w = LogWriter::open(&p).unwrap();
        assert_eq!(w.len(), good_len);
        w.append(&key(3, "c")).unwrap();
        let (evs, _) = read_all(&p, 0);
        assert_eq!(evs.iter().map(|e| e.ts).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn file_with_only_a_torn_line_is_emptied() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        std::fs::write(&p, b"{\"ts\":").unwrap();
        let w = LogWriter::open(&p).unwrap();
        assert_eq!(w.len(), 0);
        assert!(w.is_empty());
    }

    #[test]
    fn repair_scans_across_chunk_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        let mut content = b"{\"x\":1}\n".to_vec();
        content.extend(std::iter::repeat_n(b'x', 200_000)); // torn line longer than a chunk
        std::fs::write(&p, &content).unwrap();
        let w = LogWriter::open(&p).unwrap();
        assert_eq!(w.len(), 8);
    }

    #[test]
    fn reader_does_not_consume_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        let mut w = LogWriter::open(&p).unwrap();
        w.append(&key(1, "a")).unwrap();
        // A writer that has produced only half a line so far.
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        let mut line = Vec::new();
        key(2, "b").to_line(&mut line);
        let (first, second) = line.split_at(10);
        f.write_all(first).unwrap();

        let mut r = LogReader::open(&p, 0).unwrap();
        assert_eq!(r.next_line().unwrap().unwrap().event.unwrap().ts, 1);
        assert!(r.next_line().unwrap().is_none());
        let resume = r.offset();

        // The rest of the line lands; the reader picks it up whole.
        f.write_all(second).unwrap();
        assert_eq!(r.next_line().unwrap().unwrap().event.unwrap().ts, 2);
        assert!(r.next_line().unwrap().is_none());

        // A fresh reader at the saved offset sees the same line.
        let (evs, _) = read_all(&p, resume);
        assert_eq!(evs.len(), 1);
    }

    #[test]
    fn invalid_lines_are_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        std::fs::write(&p, b"not json\n").unwrap();
        let mut r = LogReader::open(&p, 0).unwrap();
        let l = r.next_line().unwrap().unwrap();
        assert!(l.event.is_none());
        assert_eq!(r.offset(), 9);
    }
}
