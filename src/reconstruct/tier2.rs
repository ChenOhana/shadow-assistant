//! Tier 2 plumbing: snapshots, the background worker, and the results path.
//!
//! When a VS Code session opens, [`VsCodeSnapshots`] copies the edited file
//! (identified from the window title) into the data directory. When the
//! session closes as tier 0/1 text, it is marked `pending`; [`Tier2Worker`]
//! then replays the session's own keystrokes from the log into a sandboxed
//! VS Code ([`super::vscode`]), one job at a time, and hands the text back to
//! the ingest thread, which writes it over the tier 0/1 text. A failure or
//! timeout leaves the tier 0/1 text in place and records why. Pending jobs
//! survive restarts (they are rows in the database) and are retried.

use super::apps::normalize_window;
use super::vscode::{self, ReplayEvent, ReplayInput, VsCodeReplayer};
use crate::event::EventKind;
use crate::eventlog::LogReader;
use crate::sessions::{Tier2Snapshotter, Tier2Start};
use crate::store;
use crate::timefmt::{Millis, now_ms};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

/// A snapshot taken this long after the session started may already include
/// the user's edits (a save in between), so it is not attempted. This also
/// skips snapshots while ingest catches up on an old log.
const MAX_SNAPSHOT_DELAY_MS: Millis = 10_000;
/// Files larger than this are not replayed.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Shut the warm instance down after this long without jobs.
const IDLE_SHUTDOWN: Duration = Duration::from_secs(600);

fn snapshot_path(dir: &Path, id: i64) -> PathBuf {
    dir.join(format!("snapshot-{id}"))
}

/// Takes VS Code file snapshots into `dir` (`<data-dir>/vscode/snapshots`).
pub struct VsCodeSnapshots {
    dir: PathBuf,
}

impl VsCodeSnapshots {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self { dir })
    }

    /// Removes snapshots no session needs any more (left behind when `brain
    /// run` died between taking a snapshot and committing its session).
    pub fn prune(&self, conn: &rusqlite::Connection) -> Result<()> {
        let mut st =
            conn.prepare("SELECT id FROM sessions WHERE tier2_status IN ('snapshot', 'pending')")?;
        let keep = st
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<std::collections::HashSet<i64>>>()?;
        for entry in std::fs::read_dir(&self.dir)?.flatten() {
            let name = entry.file_name();
            let id = name
                .to_str()
                .and_then(|n| n.strip_prefix("snapshot-"))
                .and_then(|n| n.parse::<i64>().ok());
            if id.is_some_and(|id| !keep.contains(&id)) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(())
    }
}

impl Tier2Snapshotter for VsCodeSnapshots {
    fn snapshot(&self, app: &str, raw_title: &str, started_at: Millis, id: i64) -> Tier2Start {
        if !vscode::is_vscode(app) {
            return Tier2Start::NotApplicable;
        }
        let skip = |why: &str| Tier2Start::Skipped(why.to_string());
        if now_ms() - started_at > MAX_SNAPSHOT_DELAY_MS {
            return skip("session ingested too late to snapshot its file");
        }
        if vscode::title_is_dirty(raw_title) {
            return skip("the file had unsaved changes when the session started");
        }
        let Some(file) = vscode::resolve_file(raw_title) else {
            return skip("could not identify the file from the window title");
        };
        let meta = match std::fs::metadata(&file) {
            Ok(m) if m.is_file() => m,
            _ => return skip("the file is not readable"),
        };
        if meta.len() > MAX_FILE_BYTES {
            return skip("the file is too large to replay");
        }
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as Millis);
        if modified_ms.is_some_and(|m| m > started_at) {
            return skip("the file changed on disk after the session started");
        }
        match std::fs::copy(&file, snapshot_path(&self.dir, id)) {
            Ok(_) => Tier2Start::Snapshot { file },
            Err(_) => skip("could not copy the file"),
        }
    }

    fn discard(&self, id: i64) {
        let _ = std::fs::remove_file(snapshot_path(&self.dir, id));
    }
}

/// A finished replay: the session id and its text, or why it failed.
pub type Tier2Result = (i64, std::result::Result<String, String>);

/// The background worker running tier 2 replays.
pub struct Tier2Worker {
    jobs: Option<Sender<i64>>,
    kill: vscode::KillHandle,
    thread: Option<JoinHandle<()>>,
}

impl Tier2Worker {
    /// Starts the worker. It first picks up jobs left pending by a previous
    /// run, then takes new ones from [`Tier2Worker::submit`]. Results go to
    /// `results`, to be written by the ingest thread.
    pub fn spawn(
        db_path: PathBuf,
        log_path: PathBuf,
        work_dir: PathBuf,
        results: Sender<Tier2Result>,
    ) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<i64>();
        let replayer = VsCodeReplayer::new(work_dir.clone());
        let kill = replayer.kill_handle();
        let snapshots = work_dir.join("snapshots");
        let thread = std::thread::Builder::new()
            .name("tier2".into())
            .spawn(move || worker(replayer, rx, &db_path, &log_path, &snapshots, &results))?;
        Ok(Self {
            jobs: Some(tx),
            kill,
            thread: Some(thread),
        })
    }

    pub fn submit(&self, id: i64) {
        if let Some(tx) = &self.jobs {
            let _ = tx.send(id);
        }
    }

    /// Stops the worker, killing an in-flight replay (it stays pending and
    /// is retried on the next start).
    pub fn stop(mut self) {
        self.jobs = None; // closes the channel
        self.kill.kill();
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            eprintln!("brain: warning: tier 2 worker panicked");
        }
    }
}

fn worker(
    mut replayer: VsCodeReplayer,
    rx: Receiver<i64>,
    db_path: &Path,
    log_path: &Path,
    snapshots: &Path,
    results: &Sender<Tier2Result>,
) {
    let mut queue: std::collections::VecDeque<i64> = match pending_jobs(db_path) {
        Ok(ids) => ids.into(),
        Err(e) => {
            eprintln!("brain: warning: tier 2: could not list pending jobs: {e:#}");
            Default::default()
        }
    };
    loop {
        let id = match queue.pop_front() {
            Some(id) => id,
            None => match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(id) => id,
                Err(RecvTimeoutError::Timeout) => {
                    replayer.reap_idle(IDLE_SHUTDOWN);
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
        let outcome =
            run_job(&mut replayer, db_path, log_path, snapshots, id).map_err(|e| format!("{e:#}"));
        if let Err(why) = &outcome {
            eprintln!("brain: tier 2 replay of session {id} failed, keeping tier 0/1 text: {why}");
        }
        if results.send((id, outcome)).is_err() {
            break; // ingest is gone: shutting down
        }
    }
    replayer.shutdown();
}

fn pending_jobs(db_path: &Path) -> Result<Vec<i64>> {
    let conn = store::open_reader(db_path)?;
    let mut st =
        conn.prepare("SELECT id FROM sessions WHERE tier2_status = 'pending' ORDER BY id")?;
    let ids = st
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(ids)
}

fn run_job(
    replayer: &mut VsCodeReplayer,
    db_path: &Path,
    log_path: &Path,
    snapshots: &Path,
    id: i64,
) -> Result<String> {
    let conn = store::open_reader(db_path)?;
    let (app, window, start, end, file): (String, String, i64, Option<i64>, Option<String>) = conn
        .query_row(
            "SELECT app, window, start_offset, end_offset, tier2_file FROM sessions
             WHERE id = ?1 AND tier2_status = 'pending'",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .context("session is no longer pending")?;
    drop(conn);
    let (Some(end), Some(file)) = (end, file) else {
        bail!("session has no end offset or file");
    };
    let initial = std::fs::read(snapshot_path(snapshots, id)).context("reading the snapshot")?;
    let events = session_events(log_path, &app, &window, start as u64, end as u64)?;
    let file_name = Path::new(&file)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file.txt".into());
    // Generous: VS Code may need to start (cold start is several seconds).
    let timeout = Duration::from_secs(45) + Duration::from_millis(30 * events.len() as u64);
    let saved = replayer.replay(
        &ReplayInput {
            file_name: &file_name,
            initial: &initial,
            events: &events,
        },
        timeout,
    )?;
    Ok(vscode::changed_lines(
        &String::from_utf8_lossy(&initial),
        &saved,
    ))
}

/// The session's own input events: everything logged for its (app, window)
/// between its first and last event.
fn session_events(
    log_path: &Path,
    app: &str,
    window: &str,
    start: u64,
    end: u64,
) -> Result<Vec<ReplayEvent>> {
    let mut reader = LogReader::open(log_path, start)?;
    let mut out = Vec::new();
    while let Some(line) = reader.next_line()? {
        if line.offset >= end {
            break;
        }
        let Some(ev) = line.event else { continue };
        if ev.app != app || ev.window().is_none_or(|w| normalize_window(w) != window) {
            continue;
        }
        match ev.kind {
            EventKind::Key { key, mods, .. } => out.push(ReplayEvent::Key { key, mods }),
            EventKind::Paste {
                text: Some(text), ..
            } => out.push(ReplayEvent::Paste { text }),
            _ => {}
        }
    }
    Ok(out)
}
