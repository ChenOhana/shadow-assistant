//! Tier 2 plumbing: snapshots, the background worker, and the results path.
//!
//! When a VS Code session opens, [`VsCodeSnapshots`] copies the edited file
//! (identified from the window title) into the data directory: its initial
//! content. When the session closes as tier 0/1 text, it is marked
//! `pending`, and [`Tier2Worker`] works out its real text, one job at a time:
//!
//! 1. If the file on disk was written after the session's last keystroke
//!    (autosave, or Ctrl+S at the end), it holds exactly what VS Code made of
//!    the session: the text is a git-style unified diff from the initial
//!    content to the saved file, with 3 lines of context (status
//!    `done: saved file`). VS Code is not started.
//! 2. Otherwise the session's own keystrokes are replayed from the log into a
//!    sandboxed VS Code ([`super::vscode`]), as a best effort that nothing
//!    verifies (status `done: replayed (best effort, unverified)`).
//!
//! A session that only shortcuts touched (Ctrl+D, Alt+Up, ... leave the tier
//! 0/1 text as it was; status `pending: saved file only`) gets step 1 only.
//! When the saved file shows no change, or a shortcut-only session was not
//! saved, the session changed nothing and is removed.
//!
//! The text goes back to the ingest thread, which writes it over the tier
//! 0/1 text. A failure or timeout leaves the tier 0/1 text in place and
//! records why. Pending jobs survive restarts (they are rows in the
//! database) and are retried.

use super::apps::normalize_window;
use super::vscode::{self, ReplayEvent, ReplayInput, VsCodeReplayer};
use crate::event::EventKind;
use crate::eventlog::LogReader;
use crate::sessions::{Tier2Snapshotter, Tier2Start, Tier2Status};
use crate::store;
use crate::timefmt::{Millis, now_ms};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

/// How long after a session starts its file is still looked for. VS Code
/// records a newly opened file in its window state with a delay, so the
/// lookup is retried on each ingest step until then. A late snapshot is safe
/// because the file must not have changed on disk since the session started
/// (checked below); the bound also skips sessions while ingest catches up on
/// an old log.
const MAX_SNAPSHOT_DELAY_MS: Millis = 120_000;
/// Files larger than this are not replayed.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Shut the warm instance down after this long without jobs.
const IDLE_SHUTDOWN: Duration = Duration::from_secs(600);
/// A focus event is copied from only while it is this fresh (not while
/// ingest catches up on an old log, when the file has changed since).
const LIVE_FOCUS_MS: Millis = 2_000;
/// A copy taken on focus can stand in for a session's snapshot only if the
/// session starts within this long after the focus.
const FOCUS_COPY_MS: Millis = 10_000;

fn snapshot_path(dir: &Path, id: i64) -> PathBuf {
    dir.join(format!("snapshot-{id}"))
}

/// A file copied when its VS Code window was focused, before any key.
struct FocusCopy {
    /// Where the copy is (`focus-<n>` in the snapshot directory).
    path: PathBuf,
    /// The file's modification time when it was copied.
    modified: Millis,
    /// When the window was focused.
    focused_at: Millis,
}

fn modified_ms(meta: &std::fs::Metadata) -> Option<Millis> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as Millis)
}

/// Takes VS Code file snapshots into `dir` (`<data-dir>/vscode/snapshots`).
pub struct VsCodeSnapshots {
    dir: PathBuf,
    /// VS Code's config directory to read window state from; `None` means
    /// the user's (`~/.config/Code`). Tests give a fake one.
    vscode_config: Option<PathBuf>,
    /// Files copied on focus, by file, for sessions whose file is saved
    /// before their own snapshot is taken (Ctrl+D then Ctrl+S right away).
    focus_copies: std::sync::Mutex<std::collections::HashMap<PathBuf, FocusCopy>>,
    next_copy: std::sync::atomic::AtomicU64,
}

impl VsCodeSnapshots {
    /// Snapshots stored in `dir` (created if missing).
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir,
            vscode_config: None,
            focus_copies: Default::default(),
            next_copy: Default::default(),
        })
    }

    fn resolve(&self, raw_title: &str) -> Option<PathBuf> {
        match &self.vscode_config {
            Some(config) => vscode::resolve_file_in(raw_title, Some(config)),
            None => vscode::resolve_file(raw_title),
        }
    }

    fn focus_copies(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<PathBuf, FocusCopy>> {
        self.focus_copies.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Takes the focus copy of `file`, if it still shows the file as it was
    /// when a session starting at `started_at` began: not modified after
    /// that, and the focus recent enough that nothing else saved it between.
    fn take_focus_copy(&self, file: &Path, started_at: Millis) -> Option<PathBuf> {
        let copy = self.focus_copies().remove(file)?;
        let usable = copy.modified <= started_at
            && copy.focused_at <= started_at
            && started_at - copy.focused_at <= FOCUS_COPY_MS;
        if usable {
            Some(copy.path)
        } else {
            let _ = std::fs::remove_file(&copy.path);
            None
        }
    }

    /// Reads VS Code's window state from `config` instead of the user's.
    pub fn with_vscode_config(mut self, config: PathBuf) -> Self {
        self.vscode_config = Some(config);
        self
    }

    /// Removes snapshots no session needs any more (left behind when `brain
    /// run` died between taking a snapshot and committing its session).
    pub fn prune(&self, conn: &rusqlite::Connection) -> Result<()> {
        let mut st = conn.prepare("SELECT id FROM sessions WHERE tier2_status IN (?1, ?2, ?3)")?;
        let keep = st
            .query_map(
                [
                    Tier2Status::Snapshot.as_str(),
                    Tier2Status::Pending.as_str(),
                    Tier2Status::PendingSavedOnly.as_str(),
                ],
                |r| r.get::<_, i64>(0),
            )?
            .collect::<rusqlite::Result<std::collections::HashSet<i64>>>()?;
        for entry in std::fs::read_dir(&self.dir)?.flatten() {
            let name = entry.file_name();
            if name.to_str().is_some_and(|n| n.starts_with("focus-")) {
                let _ = std::fs::remove_file(entry.path());
                continue;
            }
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
    fn counts_every_key(&self, app: &str) -> bool {
        vscode::is_vscode(app)
    }

    fn on_focus(&self, app: &str, raw_title: &str, ts: Millis) {
        if !vscode::is_vscode(app)
            || now_ms() - ts > LIVE_FOCUS_MS
            || vscode::title_is_untitled(raw_title)
            // Unsaved changes: the file on disk is not what the editor shows.
            || vscode::title_is_dirty(raw_title)
        {
            return;
        }
        let Some(file) = self.resolve(raw_title) else {
            return;
        };
        let Ok(meta) = std::fs::metadata(&file) else {
            return;
        };
        let Some(modified) = modified_ms(&meta) else {
            return;
        };
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            return;
        }
        let mut copies = self.focus_copies();
        // Drop copies too old to be used, and the previous copy of this file.
        copies.retain(|f, c| {
            let keep = f != &file && ts - c.focused_at <= FOCUS_COPY_MS;
            if !keep {
                let _ = std::fs::remove_file(&c.path);
            }
            keep
        });
        let n = self
            .next_copy
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = self.dir.join(format!("focus-{n}"));
        if std::fs::copy(&file, &path).is_ok() {
            copies.insert(
                file,
                FocusCopy {
                    path,
                    modified,
                    focused_at: ts,
                },
            );
        }
    }

    fn snapshot(&self, app: &str, raw_title: &str, started_at: Millis, id: i64) -> Tier2Start {
        if !vscode::is_vscode(app) {
            return Tier2Start::NotApplicable;
        }
        let skip = |why: &str| Tier2Start::Skipped(why.to_string());
        if vscode::title_is_untitled(raw_title) {
            return skip("unsaved new file (nothing on disk to replay from)");
        }
        if vscode::title_is_dirty(raw_title) {
            return skip("the file had unsaved changes when the session started");
        }
        let unidentified = "could not identify the file from the window title";
        let Some(file) = self.resolve(raw_title) else {
            return if now_ms() - started_at > MAX_SNAPSHOT_DELAY_MS {
                skip(unidentified)
            } else {
                Tier2Start::NotYet(unidentified.to_string())
            };
        };
        let meta = match std::fs::metadata(&file) {
            Ok(m) if m.is_file() => m,
            _ => return skip("the file is not readable"),
        };
        if meta.len() > MAX_FILE_BYTES {
            return skip("the file is too large to replay");
        }
        // A copy taken when the window was focused is used only when the
        // file was saved before this snapshot (it is consumed either way).
        let focus_copy = self.take_focus_copy(&file, started_at);
        let target = snapshot_path(&self.dir, id);
        if modified_ms(&meta).is_some_and(|m| m > started_at) {
            return match focus_copy.map(|c| std::fs::rename(c, &target)) {
                Some(Ok(())) => Tier2Start::Snapshot { file },
                _ => skip(
                    "the file changed on disk after the session started \
                     (saved before it could be snapshotted)",
                ),
            };
        }
        if let Some(c) = focus_copy {
            let _ = std::fs::remove_file(c);
        }
        match std::fs::copy(&file, target) {
            Ok(_) => Tier2Start::Snapshot { file },
            Err(_) => skip("could not copy the file"),
        }
    }

    fn discard(&self, id: i64) {
        let _ = std::fs::remove_file(snapshot_path(&self.dir, id));
    }
}

/// `tier2_status` of a session whose text is a diff of the saved file.
pub const STATUS_SAVED_FILE: &str = "done: saved file";
/// `tier2_status` of a session whose text comes from a replay in VS Code.
pub const STATUS_REPLAYED: &str = "done: replayed (best effort, unverified)";

/// A finished tier 2 job's text, and the status saying where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier2Text {
    /// The session's new text (a diff of the saved file, or the replay's changed lines).
    pub text: String,
    /// `done: saved file` or `done: replayed (best effort, unverified)`.
    pub status: &'static str,
}

/// What a finished tier 2 job found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tier2Outcome {
    /// The session's text, to replace the tier 0/1 text.
    Text(Tier2Text),
    /// The session did not change the file: remove it.
    Unchanged,
}

/// A finished job: the session id and its outcome, or why it failed.
pub type Tier2Result = (i64, std::result::Result<Tier2Outcome, String>);

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

    /// Queues session `id` (pending) for the worker.
    pub fn submit(&self, id: i64) {
        if let Some(tx) = &self.jobs {
            let _ = tx.send(id);
        }
    }

    /// Stops the worker, killing an in-flight replay (it stays pending and
    /// is retried on the next start). Dropping the worker does the same.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for Tier2Worker {
    fn drop(&mut self) {
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
        conn.prepare("SELECT id FROM sessions WHERE tier2_status IN (?1, ?2) ORDER BY id")?;
    let ids = st
        .query_map(
            [
                Tier2Status::Pending.as_str(),
                Tier2Status::PendingSavedOnly.as_str(),
            ],
            |r| r.get(0),
        )?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(ids)
}

/// A pending tier 2 job: the session's row, as the worker needs it.
struct Job {
    app: String,
    window: String,
    /// The session's events in the log: `[start, end)`.
    start: u64,
    end: u64,
    /// The edited file.
    file: String,
    /// The session's last keystroke.
    last_key: Millis,
    /// Shortcuts only ([`Tier2Status::PendingSavedOnly`]): never replayed.
    saved_only: bool,
}

fn run_job(
    replayer: &mut VsCodeReplayer,
    db_path: &Path,
    log_path: &Path,
    snapshots: &Path,
    id: i64,
) -> Result<Tier2Outcome> {
    let job = load_job(db_path, id)?;
    let initial = std::fs::read(snapshot_path(snapshots, id)).context("reading the snapshot")?;
    let initial_text = String::from_utf8_lossy(&initial);
    // 1. The file as VS Code saved it after the last keystroke, if it was.
    if let Some(outcome) = saved_file_outcome(&job, &initial_text) {
        return Ok(outcome);
    }
    // Only shortcuts and no save: nothing shows that the file changed.
    if job.saved_only {
        return Ok(Tier2Outcome::Unchanged);
    }
    // 2. Otherwise, a best-effort replay.
    replay_outcome(replayer, log_path, &job, &initial, &initial_text)
}

/// Reads session `id`, which must still be waiting for its tier 2 result.
fn load_job(db_path: &Path, id: i64) -> Result<Job> {
    let conn = store::open_reader(db_path)?;
    type Row = (
        String,
        String,
        i64,
        Option<i64>,
        Option<String>,
        Option<String>,
        String,
    );
    let (app, window, start, end, file, ended_at, status): Row = conn
        .query_row(
            "SELECT app, window, start_offset, end_offset, tier2_file, ended_at, tier2_status
             FROM sessions WHERE id = ?1 AND tier2_status IN (?2, ?3)",
            rusqlite::params![
                id,
                Tier2Status::Pending.as_str(),
                Tier2Status::PendingSavedOnly.as_str()
            ],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .context("session is no longer pending")?;
    let (Some(end), Some(file), Some(ended_at)) = (end, file, ended_at) else {
        bail!("session has no end offset, file or end time");
    };
    Ok(Job {
        app,
        window,
        start: start as u64,
        end: end as u64,
        file,
        last_key: crate::timefmt::parse_rfc3339_ms(&ended_at)?,
        saved_only: status == Tier2Status::PendingSavedOnly.as_str(),
    })
}

/// The diff of the file VS Code saved after the session's last keystroke
/// against its initial content (`Unchanged` if it is the same); `None` if
/// the file was not saved after the session.
fn saved_file_outcome(job: &Job, initial_text: &str) -> Option<Tier2Outcome> {
    let saved = saved_after(Path::new(&job.file), job.last_key)?;
    let name = Path::new(&job.file)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let diff = vscode::unified_diff(initial_text, &saved, &name, 3);
    Some(if diff.is_empty() {
        Tier2Outcome::Unchanged
    } else {
        Tier2Outcome::Text(Tier2Text {
            text: diff,
            status: STATUS_SAVED_FILE,
        })
    })
}

/// Replays the session's keystrokes from the log into VS Code and takes the
/// lines they changed (best effort, see [`super::vscode`]).
fn replay_outcome(
    replayer: &mut VsCodeReplayer,
    log_path: &Path,
    job: &Job,
    initial: &[u8],
    initial_text: &str,
) -> Result<Tier2Outcome> {
    let events = session_events(log_path, &job.app, &job.window, job.start, job.end)?;
    let file_name = Path::new(&job.file)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file.txt".into());
    // Generous: VS Code may need to start (cold start is several seconds).
    let timeout = Duration::from_secs(45) + Duration::from_millis(30 * events.len() as u64);
    let saved = replayer.replay(
        &ReplayInput {
            file_name: &file_name,
            initial,
            events: &events,
        },
        timeout,
    )?;
    Ok(Tier2Outcome::Text(Tier2Text {
        text: vscode::changed_lines(initial_text, &saved),
        status: STATUS_REPLAYED,
    }))
}

/// The content of `file` if it was written after `last_key` (so it includes
/// every edit of the session) and is a readable text file of reasonable size.
fn saved_after(file: &Path, last_key: Millis) -> Option<String> {
    let meta = std::fs::metadata(file).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as Millis;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES || modified <= last_key {
        return None;
    }
    let bytes = std::fs::read(file).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
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
