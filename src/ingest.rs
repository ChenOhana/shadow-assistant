//! Ingest: tails the capture log into the SQLite index.
//!
//! Each [`Ingester::step`] reads a bounded batch of complete log lines starting at
//! the persisted offset. It writes everything it derives from them and the new
//! offset in **one transaction**. If the process dies before the commit, nothing
//! from the batch is visible and the next start re-reads the same bytes. If it
//! dies after the commit, the offset already covers the batch. Either way there
//! are no gaps and no duplicates.
//!
//! Stage 1 derives log checkpoints (seek points for `query text`). Session
//! reconstruction is added on top of the same step in a later stage.

use crate::eventlog::LogReader;
use crate::store::{self, GLOBAL_CHECKPOINT_APP};
use crate::timefmt::{self, Millis};
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Minimum spacing between two checkpoints for the same app, in log time.
pub const CHECKPOINT_INTERVAL_MS: Millis = 5_000;
/// Upper bound on lines per transaction, which bounds memory use and commit latency.
pub const MAX_LINES_PER_STEP: usize = 5_000;

pub struct Ingester {
    conn: Connection,
    log_path: PathBuf,
    offset: u64,
    last_checkpoint: HashMap<String, Millis>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StepStats {
    pub lines: usize,
    pub invalid_lines: usize,
    pub checkpoints: usize,
}

impl Ingester {
    pub fn new(conn: Connection, log_path: &Path) -> Result<Self> {
        let mut offset = store::ingest_offset(&conn)?;
        // After a power loss the log's un-fsync'd tail can vanish while the
        // index (committed earlier) still points past it. Resume from the end
        // of what survived instead of seeking beyond EOF.
        let log_len = std::fs::metadata(log_path).map(|m| m.len()).unwrap_or(0);
        if offset > log_len {
            eprintln!(
                "brain: ingest offset {offset} is past the end of the log ({log_len} bytes); \
                 resuming from the end"
            );
            offset = log_len;
            store::set_ingest_offset(&conn, offset)?;
        }
        let mut last_checkpoint = HashMap::new();
        for (app, ts) in store::latest_checkpoints(&conn)? {
            if let Ok(ms) = timefmt::parse_rfc3339_ms(&ts) {
                last_checkpoint.insert(app, ms);
            }
        }
        Ok(Self {
            conn,
            log_path: log_path.to_owned(),
            offset,
            last_checkpoint,
        })
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Ingests at most [`MAX_LINES_PER_STEP`] new complete lines.
    /// Returns `lines == 0` when caught up.
    pub fn step(&mut self) -> Result<StepStats> {
        if !self.log_path.exists() {
            return Ok(StepStats::default());
        }
        let mut reader = LogReader::open(&self.log_path, self.offset)?;
        let mut stats = StepStats::default();
        // Checkpoint decisions are staged and only become the in-memory state
        // once the transaction commits, so a failed commit cannot desync them.
        let mut staged: HashMap<String, Millis> = HashMap::new();

        let tx = self
            .conn
            .transaction()
            .context("starting ingest transaction")?;
        while stats.lines < MAX_LINES_PER_STEP {
            let Some(line) = reader.next_line()? else {
                break;
            };
            stats.lines += 1;
            let Some(event) = line.event else {
                stats.invalid_lines += 1;
                continue;
            };
            for app in [GLOBAL_CHECKPOINT_APP, event.app.as_str()] {
                let last = staged
                    .get(app)
                    .or_else(|| self.last_checkpoint.get(app))
                    .copied();
                if checkpoint_due(last, event.ts) {
                    store::insert_checkpoint(&tx, app, &timefmt::format_ms(event.ts), line.offset)?;
                    staged.insert(app.to_owned(), event.ts);
                    stats.checkpoints += 1;
                }
            }
        }
        if stats.lines == 0 {
            return Ok(stats); // nothing read: dropping `tx` is a no-op rollback
        }
        let new_offset = reader.offset();
        store::set_ingest_offset(&tx, new_offset)?;
        tx.commit().context("committing ingest transaction")?;

        self.offset = new_offset;
        self.last_checkpoint.extend(staged);
        Ok(stats)
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Steps until caught up with the log (or `max_steps` reached).
    pub fn catch_up(&mut self, max_steps: usize) -> Result<StepStats> {
        let mut total = StepStats::default();
        for _ in 0..max_steps {
            let s = self.step()?;
            total.lines += s.lines;
            total.invalid_lines += s.invalid_lines;
            total.checkpoints += s.checkpoints;
            if s.lines < MAX_LINES_PER_STEP {
                break;
            }
        }
        Ok(total)
    }
}

/// A new checkpoint is due when none exists yet for this app, or the last one is
/// at least [`CHECKPOINT_INTERVAL_MS`] away in log time. The distance is absolute
/// so that a backwards clock jump (NTP, manual change) cannot suppress
/// checkpoints until the clock catches up again.
fn checkpoint_due(last: Option<Millis>, ts: Millis) -> bool {
    match last {
        None => true,
        Some(prev) => (ts - prev).abs() >= CHECKPOINT_INTERVAL_MS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventKind};
    use crate::eventlog::LogWriter;
    use std::io::Write;

    fn ev(ts: Millis, app: &str) -> Event {
        Event {
            ts,
            app: app.into(),
            kind: EventKind::Key {
                key: "a".into(),
                mods: vec![],
                repeat: false,
                window: "w".into(),
            },
        }
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn ingests_and_checkpoints_per_app_and_globally() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");
        let mut w = LogWriter::open(&log).unwrap();
        // 20 s of events alternating between two apps, one per second.
        for i in 0..20 {
            w.append(&ev(
                1_000_000 + i * 1000,
                if i % 2 == 0 { "a" } else { "b" },
            ))
            .unwrap();
        }
        let mut ing =
            Ingester::new(store::open_writer(&tmp.path().join("b.db")).unwrap(), &log).unwrap();
        let s = ing.catch_up(10).unwrap();
        assert_eq!(s.lines, 20);
        assert_eq!(ing.offset(), w.len());
        let c = ing.connection();
        assert_eq!(
            count(c, "SELECT log_offset FROM ingest_state"),
            w.len() as i64
        );
        // Global: t=0,5,10,15 s. Per app (events every 2 s): a at 0,6,12,18; b at 1,7,13,19.
        assert_eq!(
            count(c, "SELECT count(*) FROM log_checkpoints WHERE app='*'"),
            4
        );
        assert_eq!(
            count(c, "SELECT count(*) FROM log_checkpoints WHERE app='a'"),
            4
        );
        assert_eq!(
            count(c, "SELECT count(*) FROM log_checkpoints WHERE app='b'"),
            4
        );
        // Nothing new: a no-op step.
        assert_eq!(ing.step().unwrap(), StepStats::default());
    }

    #[test]
    fn resumes_from_persisted_offset_without_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");
        let db = tmp.path().join("b.db");
        let mut w = LogWriter::open(&log).unwrap();
        for i in 0..10 {
            w.append(&ev(i * 6000, "a")).unwrap();
        }
        {
            let mut ing = Ingester::new(store::open_writer(&db).unwrap(), &log).unwrap();
            ing.catch_up(10).unwrap();
        } // "crash" after commit
        for i in 10..15 {
            w.append(&ev(i * 6000, "a")).unwrap();
        }
        let mut ing = Ingester::new(store::open_writer(&db).unwrap(), &log).unwrap();
        let s = ing.catch_up(10).unwrap();
        assert_eq!(s.lines, 5, "only the new lines are read");
        assert_eq!(
            count(
                ing.connection(),
                "SELECT count(*) FROM log_checkpoints WHERE app='a'"
            ),
            15
        );
    }

    #[test]
    fn partial_trailing_line_is_left_for_later() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");
        let mut w = LogWriter::open(&log).unwrap();
        w.append(&ev(0, "a")).unwrap();
        let committed = w.len();
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"{\"ts\":\"2026").unwrap();
        let mut ing =
            Ingester::new(store::open_writer(&tmp.path().join("b.db")).unwrap(), &log).unwrap();
        assert_eq!(ing.step().unwrap().lines, 1);
        assert_eq!(ing.offset(), committed);
    }

    #[test]
    fn checkpoint_spacing_rule() {
        assert!(checkpoint_due(None, 0));
        assert!(!checkpoint_due(Some(0), 4_999));
        assert!(checkpoint_due(Some(0), 5_000));
        assert!(checkpoint_due(Some(100_000), 0), "backwards clock jump");
    }
}
