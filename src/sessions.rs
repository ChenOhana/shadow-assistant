//! Writing sessions: groups logged events into sessions and keeps each
//! session's reconstructed text, inside the ingest transaction.
//!
//! **Session rules** (from the design):
//! * A session is keyed by `(app, window)`; the window title is normalized
//!   ([`normalize_window`]) so an editor's unsaved-marker does not split it.
//! * It closes after [`IDLE_TIMEOUT_MS`] without activity *in that context*,
//!   or at once when a single-line field commits (Enter at a shell prompt, in
//!   a chat box or an address bar), is cancelled (Ctrl+C at a prompt), or
//!   focus moves to another field of the same window (Tab in a web form).
//! * Focus loss alone never closes it: switching away and back within the
//!   idle timeout resumes the same session.
//! * A window whose title changes *because of the typing* (VS Code names an
//!   unsaved file after its first line, a mail window after its subject)
//!   keeps its session: a title change within [`RENAME_WINDOW_MS`] of a
//!   keystroke in that window, with no shortcut in between, makes the new
//!   title an alias of the open session.
//! * `brain run` stopping or restarting closes all sessions: what was typed
//!   while it was not running is unknown.
//!
//! **Text.** In multi-line apps (editors and anything unknown) each
//! `(app, window)` has a modelled document ([`TextBuffer`]) that outlives its
//! sessions (persisted in `context_state`), so the next session continues
//! where the previous one left it. A session's text is that document when the
//! session closes. Single-line fields (shell prompts, address bars, chat
//! boxes) start every session empty: they are short-lived, and their window
//! titles are reused by unrelated fields (every new Chrome tab is "New tab",
//! every shell at home is "~"), so carrying text over would mix them.
//!
//! **Determinism.** Everything here is a function of the log, except two
//! wall-clock decisions that cannot change the result: closing an idle
//! session before the next event arrives ([`Sessions::close_idle`] waits a
//! grace period past the timeout, so the event-time rule would close it the
//! same way), and the VS Code file snapshot (tier 2, an additive upgrade).
//! After a crash, open sessions are rebuilt by replaying their events from
//! the log ([`Sessions::recover`]), so a rebuild from the log produces the
//! same rows as an uninterrupted run.

use crate::event::{Event, EventKind, SystemState};
use crate::eventlog::LogReader;
use crate::reconstruct::apps::{AppClass, classify, normalize_window};
use crate::reconstruct::buffer::TextBuffer;
use crate::reconstruct::editor::{Action, FieldEditor, Outcome, action, key_suppresses_paste};
use crate::timefmt::{self, Millis};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A session closes after this long without activity in its context.
pub const IDLE_TIMEOUT_MS: Millis = 30_000;
/// Before closing an idle session on the wall clock (no later event yet),
/// wait this much longer, so no event with an earlier timestamp can still be
/// on its way into the log.
const IDLE_GRACE_MS: Millis = 2_000;
/// A context's document is forgotten after this long without a session.
pub const CONTEXT_TTL_MS: Millis = 24 * 3600 * 1000;
/// A window title change this soon after a keystroke in that window is
/// taken as caused by the typing (VS Code updates it within ~25 ms).
pub const RENAME_WINDOW_MS: Millis = 1_000;

type Key = (String, String);

/// How a VS Code session can be replayed in the real editor (tier 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tier2Start {
    /// Not a VS Code session, or replay is off.
    NotApplicable,
    /// The edited file was copied to the snapshot path for this session.
    Snapshot { file: PathBuf },
    /// Replay is impossible; the reason is recorded.
    Skipped(String),
}

/// Takes the file snapshot a tier 2 replay starts from, when a session opens.
pub trait Tier2Snapshotter: Send {
    fn snapshot(
        &self,
        app: &str,
        raw_title: &str,
        started_at: Millis,
        session_id: i64,
    ) -> Tier2Start;
    /// Removes the snapshot of a session that will never be replayed.
    fn discard(&self, session_id: i64);
}

struct OpenSession {
    id: i64,
    editor: FieldEditor,
    last_activity: Millis,
    end_offset: u64,
    tier2_snapshot: bool,
    dirty: bool,
    /// Hash of the document when the session opened: a session that ends
    /// with the same text changed nothing.
    start_hash: u64,
    /// The window's latest title, when typing renamed it (see `aliases`).
    latest: Option<Key>,
}

fn text_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

pub struct Sessions {
    open: HashMap<Key, OpenSession>,
    /// A plain Ctrl+V in a terminal at this (app, window, ts): drop the paste.
    suppress_paste: Option<(String, String, Millis)>,
    /// Titles a window took on while typing in it, mapped to the key of the
    /// open session they belong to.
    aliases: HashMap<Key, Key>,
    /// The session the last applied keystroke went to, and when. Cleared by
    /// any ignored key (a shortcut such as Ctrl+Tab may switch tabs).
    last_key: Option<(Key, Millis)>,
    tier2: Option<Box<dyn Tier2Snapshotter>>,
    /// Effects to carry out only after the transaction commits.
    staged_jobs: Vec<i64>,
    staged_discards: Vec<i64>,
    /// Whether anything was written in the current transaction.
    wrote: bool,
}

impl Sessions {
    /// Rebuilds the open sessions from the database and the log: each open
    /// session's document is restored from `context_state` and its events
    /// (from `start_offset` up to `upto`, the ingested offset) are replayed.
    pub fn recover(
        conn: &Connection,
        log_path: &Path,
        upto: u64,
        tier2: Option<Box<dyn Tier2Snapshotter>>,
    ) -> Result<Self> {
        let mut s = Self {
            open: HashMap::new(),
            suppress_paste: None,
            aliases: HashMap::new(),
            last_key: None,
            tier2,
            staged_jobs: Vec::new(),
            staged_discards: Vec::new(),
            wrote: false,
        };
        let mut stmt = conn.prepare(
            "SELECT id, app, window, started_at, start_offset, tier2_status
             FROM sessions WHERE status = 'open' ORDER BY start_offset",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let mut starts: HashMap<Key, u64> = HashMap::new();
        for (id, app, window, started_at, start_offset, tier2) in rows {
            let started = timefmt::parse_rfc3339_ms(&started_at)
                .with_context(|| format!("session {id}: bad started_at"))?;
            let buf = load_context(conn, &app, &window, started, false)?;
            let start_hash = text_hash(&buf.text());
            let key = (app.clone(), window);
            starts.insert(key.clone(), start_offset as u64);
            s.open.insert(
                key,
                OpenSession {
                    id,
                    editor: FieldEditor::new(classify(&app), buf),
                    last_activity: started,
                    end_offset: start_offset as u64,
                    tier2_snapshot: tier2.as_deref() == Some("snapshot"),
                    dirty: false,
                    start_hash,
                    latest: None,
                },
            );
        }
        let Some(&from) = starts.values().min() else {
            return Ok(s);
        };
        let mut reader = LogReader::open(log_path, from)?;
        while let Some(line) = reader.next_line()? {
            if line.offset >= upto {
                break;
            }
            let next = reader.offset();
            let Some(ev) = line.event else { continue };
            let in_session = |k: &Key| starts.get(k).is_some_and(|&st| line.offset >= st);
            s.replay_event(conn, &ev, line.offset, next, &in_session)?;
        }
        Ok(s)
    }

    /// Applies one event to the open session it belongs to, without opening
    /// or closing sessions (used for recovery).
    fn replay_event(
        &mut self,
        conn: &Connection,
        ev: &Event,
        offset: u64,
        next: u64,
        in_session: &dyn Fn(&Key) -> bool,
    ) -> Result<()> {
        match &ev.kind {
            EventKind::Key {
                key, mods, window, ..
            } => {
                let k = self.resolve(&ev.app, window);
                let class = classify(&ev.app);
                let act = self.note_key(class, &k, key, mods, ev.ts);
                if act == Action::Ignore {
                    self.last_key = None;
                    return Ok(());
                }
                if !in_session(&k) {
                    return Ok(());
                }
                if let Some(s) = self.open.get_mut(&k) {
                    let mut hist = |n: usize| history_entry(conn, &ev.app, offset, n);
                    s.editor.apply(&act, &mut hist);
                    s.touch(ev.ts, next);
                    self.last_key = Some((k, ev.ts));
                }
            }
            EventKind::Paste { text, window } => {
                let k = self.resolve(&ev.app, window);
                if self.paste_suppressed(&k, ev.ts) || !in_session(&k) {
                    return Ok(());
                }
                if let (Some(t), Some(s)) = (text, self.open.get_mut(&k)) {
                    s.editor.paste(t);
                    s.touch(ev.ts, next);
                    self.last_key = Some((k, ev.ts));
                }
            }
            EventKind::Focus { window } => self.note_title(&ev.app, window, ev.ts, in_session),
            _ => {}
        }
        Ok(())
    }

    /// The session key for an event: the normalized title, or the open
    /// session it is an alias of.
    fn resolve(&self, app: &str, window: &str) -> Key {
        let k = (app.to_string(), normalize_window(window));
        self.aliases.get(&k).cloned().unwrap_or(k)
    }

    /// A focus event: if the window's title just changed because of the
    /// typing, record the new title as an alias of the open session.
    /// `is_open` says whether a session is open at this point in the log.
    fn note_title(&mut self, app: &str, window: &str, ts: Millis, is_open: &dyn Fn(&Key) -> bool) {
        let Some((last, at)) = &self.last_key else {
            return;
        };
        let k = (app.to_string(), normalize_window(window));
        let renamed = last.0 == app
            && ts - at <= RENAME_WINDOW_MS
            && self.resolve(app, window) != *last
            && is_open(last)
            && !is_open(&k);
        if renamed {
            let last = last.clone();
            if let Some(s) = self.open.get_mut(&last) {
                s.latest = Some(k.clone());
            }
            self.aliases.insert(k, last);
        }
    }

    /// Tracks the plain-Ctrl+V-in-a-terminal case and maps the key.
    fn note_key(
        &mut self,
        class: AppClass,
        k: &Key,
        key: &str,
        mods: &[crate::event::Mod],
        ts: Millis,
    ) -> Action {
        self.suppress_paste =
            key_suppresses_paste(class, key, mods).then(|| (k.0.clone(), k.1.clone(), ts));
        action(class, key, mods)
    }

    fn paste_suppressed(&mut self, k: &Key, ts: Millis) -> bool {
        let hit = self
            .suppress_paste
            .as_ref()
            .is_some_and(|(a, w, t)| *a == k.0 && *w == k.1 && *t == ts);
        self.suppress_paste = None;
        hit
    }

    /// Processes one log event inside the ingest transaction.
    pub fn on_event(&mut self, tx: &Connection, offset: u64, next: u64, ev: &Event) -> Result<()> {
        // Event-time idle rule: any session idle for the timeout before this
        // event closed before it.
        self.close_where(tx, "idle", |s| ev.ts - s.last_activity >= IDLE_TIMEOUT_MS)?;
        match &ev.kind {
            EventKind::Key {
                key, mods, window, ..
            } => {
                let k = self.resolve(&ev.app, window);
                let class = classify(&ev.app);
                let act = self.note_key(class, &k, key, mods, ev.ts);
                if act == Action::Ignore {
                    self.last_key = None;
                    return Ok(());
                }
                self.ensure_open(tx, &k, class, window, ev.ts, offset)?;
                let s = self.open.get_mut(&k).expect("just opened");
                let mut hist = |n: usize| history_entry(tx, &ev.app, offset, n);
                let outcome = s.editor.apply(&act, &mut hist);
                s.touch(ev.ts, next);
                self.last_key = Some((k.clone(), ev.ts));
                let reason = match outcome {
                    Outcome::Continue => return Ok(()),
                    Outcome::Commit => "commit",
                    Outcome::Cancel => "cancel",
                    Outcome::FieldChange => "field",
                };
                self.close(tx, &k, reason, true)?;
            }
            EventKind::Paste { text, window } => {
                let k = self.resolve(&ev.app, window);
                if self.paste_suppressed(&k, ev.ts) {
                    return Ok(());
                }
                // Unknown clipboard content (Klipper unavailable): nothing to insert.
                let Some(text) = text else { return Ok(()) };
                self.ensure_open(tx, &k, classify(&ev.app), window, ev.ts, offset)?;
                let s = self.open.get_mut(&k).expect("just opened");
                s.editor.paste(text);
                s.touch(ev.ts, next);
                self.last_key = Some((k, ev.ts));
            }
            EventKind::Focus { window } => {
                let open: Vec<Key> = self.open.keys().cloned().collect();
                self.note_title(&ev.app, window, ev.ts, &|k| open.contains(k));
            }
            EventKind::System { state } => match state {
                SystemState::Start => {
                    // A start without a preceding stop means a crash; either
                    // way, input while not running is unknown.
                    self.close_where(tx, "restart", |_| true)?;
                    tx.execute("UPDATE context_state SET cursor_unknown = 1", [])?;
                    tx.execute(
                        "DELETE FROM context_state WHERE updated_at < ?1",
                        [timefmt::format_ms(ev.ts - CONTEXT_TTL_MS)],
                    )?;
                    self.wrote = true;
                }
                SystemState::Stop => self.close_where(tx, "stop", |_| true)?,
                _ => {}
            },
        }
        Ok(())
    }

    /// Closes sessions idle on the wall clock. Call only when the log has
    /// been ingested up to its end.
    pub fn close_idle(&mut self, tx: &Connection, now: Millis) -> Result<()> {
        self.close_where(tx, "idle", |s| {
            now - s.last_activity >= IDLE_TIMEOUT_MS + IDLE_GRACE_MS
        })
    }

    /// Writes the current text of open sessions that changed, so live
    /// searches see what is being typed right now.
    pub fn flush(&mut self, tx: &Connection) -> Result<()> {
        for s in self.open.values_mut().filter(|s| s.dirty) {
            tx.prepare_cached("UPDATE sessions SET reconstructed_text = ?1 WHERE id = ?2")?
                .execute(params![s.editor.buf.text(), s.id])?;
            s.dirty = false;
            self.wrote = true;
        }
        Ok(())
    }

    fn ensure_open(
        &mut self,
        tx: &Connection,
        k: &Key,
        class: AppClass,
        raw_window: &str,
        ts: Millis,
        offset: u64,
    ) -> Result<()> {
        if self.open.contains_key(k) {
            return Ok(());
        }
        let buf = load_context(tx, &k.0, &k.1, ts, true)?;
        let start_hash = text_hash(&buf.text());
        tx.prepare_cached(
            "INSERT INTO sessions (app, window, started_at, status, tier_used,
                                   reconstructed_text, start_offset)
             VALUES (?1, ?2, ?3, 'open', ?4, ?5, ?6)",
        )?
        .execute(params![
            k.0,
            k.1,
            timefmt::format_ms(ts),
            class.tier(),
            buf.text(),
            offset as i64
        ])?;
        let id = tx.last_insert_rowid();
        let mut tier2_snapshot = false;
        if let Some(t2) = &self.tier2 {
            let (status, file) = match t2.snapshot(&k.0, raw_window, ts, id) {
                Tier2Start::NotApplicable => (None, None),
                Tier2Start::Snapshot { file } => {
                    tier2_snapshot = true;
                    (
                        Some("snapshot".to_string()),
                        Some(file.to_string_lossy().into_owned()),
                    )
                }
                Tier2Start::Skipped(why) => (Some(format!("skipped: {why}")), None),
            };
            if status.is_some() {
                tx.execute(
                    "UPDATE sessions SET tier2_status = ?1, tier2_file = ?2 WHERE id = ?3",
                    params![status, file, id],
                )?;
            }
        }
        self.wrote = true;
        self.open.insert(
            k.clone(),
            OpenSession {
                id,
                editor: FieldEditor::new(class, buf),
                last_activity: ts,
                end_offset: offset,
                tier2_snapshot,
                dirty: false,
                start_hash,
                latest: None,
            },
        );
        Ok(())
    }

    fn close_where(
        &mut self,
        tx: &Connection,
        reason: &str,
        pred: impl Fn(&OpenSession) -> bool,
    ) -> Result<()> {
        let mut keys: Vec<Key> = self
            .open
            .iter()
            .filter(|(_, s)| pred(s))
            .map(|(k, _)| k.clone())
            .collect();
        // Deterministic order (HashMap iteration order is not).
        keys.sort_by_key(|k| self.open[k].id);
        for k in keys {
            self.close(tx, &k, reason, false)?;
        }
        Ok(())
    }

    /// Closes a session. A session that left the document as it found it
    /// (cursor movement, a typo typed and deleted) or holds only whitespace
    /// leaves no row behind, except a commit of a non-empty line, which is a
    /// real submission (a shell command run again from history).
    fn close(&mut self, tx: &Connection, k: &Key, reason: &str, reset: bool) -> Result<()> {
        let Some(s) = self.open.remove(k) else {
            return Ok(());
        };
        self.aliases.retain(|_, v| v != k);
        if self.last_key.as_ref().is_some_and(|(lk, _)| lk == k) {
            self.last_key = None;
        }
        let text = s.editor.buf.text();
        let changed = text_hash(&text) != s.start_hash;
        let keep = (changed || reason == "commit") && !text.trim().is_empty();
        if keep {
            tx.prepare_cached(
                "UPDATE sessions SET status = 'closed', ended_at = ?1, reconstructed_text = ?2,
                    end_offset = ?3, end_reason = ?4,
                    tier2_status = CASE WHEN tier2_status = 'snapshot' THEN 'pending'
                                        ELSE tier2_status END
                 WHERE id = ?5",
            )?
            .execute(params![
                timefmt::format_ms(s.last_activity),
                text,
                s.end_offset as i64,
                reason,
                s.id
            ])?;
            if s.tier2_snapshot {
                self.staged_jobs.push(s.id);
            }
        } else {
            tx.prepare_cached("DELETE FROM sessions WHERE id = ?1")?
                .execute([s.id])?;
            if s.tier2_snapshot {
                self.staged_discards.push(s.id);
            }
        }
        // The document is saved under the session's key and, if typing
        // renamed the window, under its current title too: the next session
        // in this window will be keyed by that title.
        let keys: Vec<&Key> = std::iter::once(k).chain(s.latest.as_ref()).collect();
        for k in keys {
            self.save_context(tx, k, &s, reset)?;
        }
        self.wrote = true;
        Ok(())
    }

    fn save_context(&self, tx: &Connection, k: &Key, s: &OpenSession, reset: bool) -> Result<()> {
        let text = s.editor.buf.text();
        if reset || s.editor.class.single_line() || s.editor.buf.is_empty() {
            tx.prepare_cached("DELETE FROM context_state WHERE app = ?1 AND window = ?2")?
                .execute(params![k.0, k.1])?;
        } else {
            let b = &s.editor.buf;
            tx.prepare_cached(
                "INSERT INTO context_state (app, window, text, cursor, anchor, cursor_unknown, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
                 ON CONFLICT (app, window) DO UPDATE SET text = excluded.text,
                    cursor = excluded.cursor, anchor = excluded.anchor,
                    cursor_unknown = 0, updated_at = excluded.updated_at",
            )?
            .execute(params![
                k.0,
                k.1,
                text,
                b.cursor() as i64,
                b.anchor().map(|a| a as i64),
                timefmt::format_ms(s.last_activity)
            ])?;
        }
        Ok(())
    }

    /// Takes the "something was written" flag for the current transaction.
    pub fn take_wrote(&mut self) -> bool {
        std::mem::take(&mut self.wrote)
    }

    /// After a successful commit: tier 2 jobs to start, and snapshots to drop.
    pub fn take_committed_effects(&mut self) -> (Vec<i64>, Vec<i64>) {
        (
            std::mem::take(&mut self.staged_jobs),
            std::mem::take(&mut self.staged_discards),
        )
    }

    pub fn discard_snapshot(&self, id: i64) {
        if let Some(t2) = &self.tier2 {
            t2.discard(id);
        }
    }

    /// Hands the tier 2 snapshotter over (when rebuilding after a failed step).
    pub fn into_tier2(self) -> Option<Box<dyn Tier2Snapshotter>> {
        self.tier2
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }
}

impl OpenSession {
    fn touch(&mut self, ts: Millis, next: u64) {
        // max(): a backwards clock jump must not make the session look older.
        self.last_activity = self.last_activity.max(ts);
        self.end_offset = next;
        self.dirty = true;
    }
}

/// Loads the modelled document of a context as of `ts`. A document not
/// touched for [`CONTEXT_TTL_MS`] is forgotten. After a restart of `brain run`
/// (cursor unknown: the user may have typed meanwhile) the cursor goes to the
/// end; with `write_back` that resolution is saved, so recovery later loads
/// exactly what this session started from.
fn load_context(
    conn: &Connection,
    app: &str,
    window: &str,
    ts: Millis,
    write_back: bool,
) -> Result<TextBuffer> {
    let row = conn
        .prepare_cached(
            "SELECT text, cursor, anchor, cursor_unknown, updated_at
             FROM context_state WHERE app = ?1 AND window = ?2",
        )?
        .query_row(params![app, window], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .optional()?;
    let Some((text, cursor, anchor, unknown, updated_at)) = row else {
        return Ok(TextBuffer::new());
    };
    let fresh = timefmt::parse_rfc3339_ms(&updated_at).is_ok_and(|u| ts - u < CONTEXT_TTL_MS);
    if !fresh {
        return Ok(TextBuffer::new());
    }
    let mut buf = TextBuffer::from_state(
        &text,
        cursor.max(0) as usize,
        anchor.map(|a| a.max(0) as usize),
    );
    if unknown {
        buf.cursor_unknown();
        if write_back {
            conn.prepare_cached(
                "UPDATE context_state SET cursor = ?1, anchor = NULL, cursor_unknown = 0
                 WHERE app = ?2 AND window = ?3",
            )?
            .execute(params![buf.cursor() as i64, app, window])?;
        }
    }
    Ok(buf)
}

/// The `n`-th most recent line committed in `app` before log offset `before`
/// (shell history, as far as `brain` saw it being typed).
fn history_entry(conn: &Connection, app: &str, before: u64, n: usize) -> Option<String> {
    conn.prepare_cached(
        "SELECT reconstructed_text FROM sessions
         WHERE app = ?1 AND end_reason = 'commit' AND end_offset <= ?2
         ORDER BY end_offset DESC LIMIT 1 OFFSET ?3",
    )
    .and_then(|mut st| {
        st.query_row(params![app, before as i64, n as i64], |r| r.get(0))
            .optional()
    })
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use crate::event::{Event, EventKind, Mod, SystemState};
    use crate::eventlog::LogWriter;
    use crate::ingest::Ingester;
    use crate::store;
    use crate::timefmt::Millis;
    use rusqlite::Connection;
    use std::path::PathBuf;

    const T0: Millis = 1_800_000_000_000;
    /// A clock where every session is still fresh (no wall-clock idle close).
    fn clock_t0() -> Millis {
        T0
    }
    /// A clock far past every event: all idle sessions close.
    fn clock_late() -> Millis {
        T0 + 3_600_000
    }

    struct Log {
        _tmp: tempfile::TempDir,
        dir: PathBuf,
        w: LogWriter,
    }

    impl Log {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().to_owned();
            let w = LogWriter::open(&dir.join("events.jsonl")).unwrap();
            Self { _tmp: tmp, dir, w }
        }
        fn push(&mut self, ts: Millis, app: &str, kind: EventKind) {
            self.w
                .append(&Event {
                    ts,
                    app: app.into(),
                    kind,
                })
                .unwrap();
        }
        fn key(&mut self, ts: Millis, app: &str, win: &str, key: &str, mods: &[Mod]) {
            self.push(
                ts,
                app,
                EventKind::Key {
                    key: key.into(),
                    mods: mods.to_vec(),
                    repeat: false,
                    window: win.into(),
                },
            );
        }
        /// Types `s` one key every 100 ms from `ts`; returns the next free ts.
        fn typ(&mut self, ts: Millis, app: &str, win: &str, s: &str) -> Millis {
            let mut t = ts;
            for c in s.chars() {
                let k = match c {
                    '\n' => "Enter".to_string(),
                    '\u{8}' => "Backspace".to_string(),
                    c => c.to_string(),
                };
                self.key(t, app, win, &k, &[]);
                t += 100;
            }
            t
        }
        fn sys(&mut self, ts: Millis, state: SystemState) {
            self.push(ts, "unknown", EventKind::System { state });
        }
        fn log_path(&self) -> PathBuf {
            self.dir.join("events.jsonl")
        }
        fn ingester(&self, db: &str) -> Ingester {
            Ingester::new(
                store::open_writer(&self.dir.join(db)).unwrap(),
                &self.log_path(),
            )
            .unwrap()
        }
    }

    fn run(ing: &mut Ingester, clock: fn() -> Millis) {
        while ing.step_at(clock).unwrap().lines > 0 {}
    }

    /// (app, window, status, end_reason, text) of every session, by id.
    fn sessions(c: &Connection) -> Vec<(String, String, String, Option<String>, String)> {
        let mut st = c
            .prepare(
                "SELECT app, window, status, end_reason, reconstructed_text FROM sessions ORDER BY id",
            )
            .unwrap();
        st.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    fn texts(c: &Connection) -> Vec<String> {
        sessions(c).into_iter().map(|s| s.4).collect()
    }

    fn search(c: &Connection, q: &str) -> Vec<i64> {
        let mut st = c
            .prepare("SELECT rowid FROM sessions_fts WHERE sessions_fts MATCH ?1 ORDER BY rowid")
            .unwrap();
        st.query_map([q], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn idle_timeout_closes_and_the_document_carries_over() {
        let mut l = Log::new();
        let t = l.typ(T0, "notes", "todo", "the cat");
        // 31 s later, in the same window: a new session.
        l.typ(t + 31_000, "notes", "todo", "\u{8}\u{8}\u{8}dog");
        let mut ing = l.ingester("b.db");
        run(&mut ing, clock_late);
        let s = sessions(ing.connection());
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].3.as_deref(), Some("idle"));
        assert_eq!(s[0].4, "the cat");
        assert_eq!(s[1].4, "the dog", "second session edits the same document");
        // A word present when a session closed stays findable through it,
        // even though a later session deleted it.
        let c = ing.connection();
        assert_eq!(search(c, "cat"), vec![1]);
        assert_eq!(search(c, "dog"), vec![2]);
        assert_eq!(search(c, "the"), vec![1, 2]);
    }

    #[test]
    fn word_deleted_within_the_session_is_not_found() {
        let mut l = Log::new();
        l.typ(
            T0,
            "notes",
            "todo",
            "secret\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}public",
        );
        let mut ing = l.ingester("b.db");
        run(&mut ing, clock_late);
        let c = ing.connection();
        assert_eq!(texts(c), vec!["public"]);
        assert!(search(c, "secret").is_empty());
        assert_eq!(search(c, "public"), vec![1]);
    }

    #[test]
    fn focus_loss_and_return_resumes_the_session() {
        let mut l = Log::new();
        let t = l.typ(T0, "notes", "a.txt", "hello ");
        // Away in another app for ~21 s (under 30 s since the last key in
        // a.txt), then back: the same session.
        let t = l.typ(t + 1_000, "google-chrome", "Search", "query");
        let t = l.typ(t + 20_000, "notes", "a.txt", "world");
        // Away again, and back after 30 s: a new session.
        l.typ(t + 30_500, "notes", "a.txt", "!");
        let mut ing = l.ingester("b.db");
        run(&mut ing, clock_late);
        let s = sessions(ing.connection());
        let notes: Vec<_> = s.iter().filter(|s| s.0 == "notes").collect();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].4, "hello world");
        assert_eq!(notes[1].4, "hello world!");
        assert!(s.iter().any(|s| s.0 == "google-chrome" && s.4 == "query"));
    }

    #[test]
    fn unsaved_marker_does_not_split_a_session() {
        let mut l = Log::new();
        let t = l.typ(T0, "code", "main.rs - brain - Visual Studio Code", "a");
        l.typ(t, "code", "● main.rs - brain - Visual Studio Code", "bc");
        let mut ing = l.ingester("b.db");
        run(&mut ing, clock_late);
        let s = sessions(ing.connection());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].1, "main.rs - brain - Visual Studio Code");
        assert_eq!(s[0].4, "abc");
    }

    /// What VS Code does with a new file: the title becomes the first line.
    fn type_renaming(l: &mut Log, ts: Millis, app: &str, text: &str, suffix: &str) -> Millis {
        let mut t = ts;
        let mut typed = String::new();
        for c in text.chars() {
            let title = if typed.trim().is_empty() {
                suffix.to_string()
            } else {
                format!("● {} • {suffix}", typed.trim_end())
            };
            l.key(t, app, &title, &c.to_string(), &[]);
            typed.push(c);
            let new = format!("● {} • {suffix}", typed.trim_end());
            if new != title {
                l.push(t + 20, app, EventKind::Focus { window: new });
            }
            t += 100;
        }
        t
    }

    #[test]
    fn title_renamed_by_typing_keeps_the_session() {
        let mut l = Log::new();
        let app = "com.microsoft.VSCode";
        let suffix = "Untitled-1 - proj - Visual Studio Code";
        let t = type_renaming(&mut l, T0, app, "testing a title", suffix);
        // Back after the idle timeout: the window is now titled by its
        // content, and the document continues under that title.
        let title = format!("● testing a title • {suffix}");
        l.typ(t + 40_000, app, &title, "!");
        let mut ing = l.ingester("b.db");
        run(&mut ing, clock_late);
        let s = sessions(ing.connection());
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!(s[0].1, suffix, "keyed by the title it started with");
        assert_eq!(s[0].4, "testing a title");
        assert_eq!(s[1].4, "testing a title!");
    }

    #[test]
    fn a_shortcut_before_a_title_change_is_a_real_switch() {
        let mut l = Log::new();
        let t = l.typ(T0, "notes", "tab A", "one");
        l.key(t, "notes", "tab A", "Tab", &[Mod::Ctrl]); // switches tabs
        l.push(
            t + 20,
            "notes",
            EventKind::Focus {
                window: "tab B".into(),
            },
        );
        let t = l.typ(t + 100, "notes", "tab B", "two");
        // A title change long after the last keystroke is not a rename either.
        l.push(
            t + 5_000,
            "notes",
            EventKind::Focus {
                window: "tab C".into(),
            },
        );
        l.typ(t + 5_100, "notes", "tab C", "three");
        let mut ing = l.ingester("b.db");
        run(&mut ing, clock_late);
        assert_eq!(texts(ing.connection()), vec!["one", "two", "three"]);
    }

    #[test]
    fn enter_commits_single_line_fields() {
        let mut l = Log::new();
        let t = l.typ(T0, "org.kde.konsole", "~", "ls -l\n");
        let t = l.typ(t, "org.kde.konsole", "~", "\n"); // empty line: no row
        l.typ(t, "org.kde.konsole", "~", "pwd\n");
        let mut ing = l.ingester("b.db");
        run(&mut ing, clock_t0);
        let s = sessions(ing.connection());
        assert_eq!(texts(ing.connection()), vec!["ls -l", "pwd"]);
        assert!(
            s.iter()
                .all(|s| s.2 == "closed" && s.3.as_deref() == Some("commit"))
        );
    }

    #[test]
    fn shell_history_recall_uses_earlier_commands() {
        let mut l = Log::new();
        let t = l.typ(T0, "org.kde.konsole", "~", "brain query -h\n");
        l.key(t, "org.kde.konsole", "~", "Up", &[]);
        let t = l.typ(t + 100, "org.kde.konsole", "~", "\u{8}\u{8}search -h\n");
        // Ctrl+C abandons a line; it is still recorded (it was written).
        l.typ(t, "org.kde.konsole", "~", "rm -rf");
        l.key(t + 1_000, "org.kde.konsole", "~", "c", &[Mod::Ctrl]);
        let mut ing = l.ingester("b.db");
        run(&mut ing, clock_t0);
        let s = sessions(ing.connection());
        assert_eq!(
            texts(ing.connection()),
            vec!["brain query -h", "brain query search -h", "rm -rf"]
        );
        assert_eq!(s[2].3.as_deref(), Some("cancel"));
    }

    #[test]
    fn paste() {
        let mut l = Log::new();
        let t = l.typ(T0, "notes", "n", "ab");
        l.key(t, "notes", "n", "Home", &[]);
        l.key(t + 100, "notes", "n", "End", &[]);
        let t = l.typ(t + 200, "notes", "n", "c");
        l.key(t, "notes", "n", "v", &[Mod::Ctrl]);
        l.push(
            t,
            "notes",
            EventKind::Paste {
                text: Some(" pasted".into()),
                window: "n".into(),
            },
        );
        // In a terminal, plain Ctrl+V does not paste.
        l.key(t + 100, "org.kde.konsole", "~", "v", &[Mod::Ctrl]);
        l.push(
            t + 100,
            "org.kde.konsole",
            EventKind::Paste {
                text: Some("nope".into()),
                window: "~".into(),
            },
        );
        l.key(
            t + 200,
            "org.kde.konsole",
            "~",
            "V",
            &[Mod::Ctrl, Mod::Shift],
        );
        l.push(
            t + 200,
            "org.kde.konsole",
            EventKind::Paste {
                text: Some("yes".into()),
                window: "~".into(),
            },
        );
        let mut ing = l.ingester("b.db");
        run(&mut ing, clock_late);
        assert_eq!(texts(ing.connection()), vec!["abc pasted", "yes"]);
    }

    #[test]
    fn navigation_only_sessions_leave_no_row() {
        let mut l = Log::new();
        let t = l.typ(T0, "notes", "n", "x");
        l.key(t + 40_000, "notes", "n", "Left", &[]);
        l.key(t + 40_100, "notes", "n", " ", &[]);
        l.key(t + 40_200, "notes", "n", "Backspace", &[]);
        l.key(t + 90_000, "dolphin", "Home", "Down", &[]);
        let mut ing = l.ingester("b.db");
        run(&mut ing, clock_late);
        // Session 2 typed and deleted a space: the document is unchanged, so
        // it leaves no row; Dolphin only saw an arrow key.
        let s = sessions(ing.connection());
        assert_eq!(texts(ing.connection()), vec!["x"]);
        assert_eq!(s.iter().filter(|s| s.0 == "dolphin").count(), 0);
    }

    #[test]
    fn live_text_of_open_sessions_is_searchable() {
        let mut l = Log::new();
        l.typ(T0, "notes", "n", "draft in progress");
        let mut ing = l.ingester("b.db");
        run(&mut ing, clock_t0); // clock at T0: not idle yet
        let s = sessions(ing.connection());
        assert_eq!(s[0].2, "open");
        assert_eq!(s[0].4, "draft in progress");
        assert_eq!(search(ing.connection(), "progress"), vec![1]);
        // Once the wall clock passes the timeout (plus grace), it closes.
        run(&mut ing, clock_late);
        assert_eq!(sessions(ing.connection())[0].2, "closed");
    }

    #[test]
    fn stop_and_restart_close_sessions() {
        let mut l = Log::new();
        let t = l.typ(T0, "notes", "n", "one");
        l.sys(t, SystemState::Stop);
        l.sys(t + 100, SystemState::Start);
        let t = l.typ(t + 200, "notes", "n", " two");
        l.sys(t, SystemState::Start); // crash: start without stop
        // A single-line field does not carry text across sessions.
        let t = l.typ(
            t + 100,
            "google-chrome",
            "New tab - Google Chrome",
            "abandoned",
        );
        l.typ(
            t + 31_000,
            "google-chrome",
            "New tab - Google Chrome",
            "fresh",
        );
        let mut ing = l.ingester("b.db");
        run(&mut ing, clock_t0);
        let s = sessions(ing.connection());
        assert_eq!(s.len(), 4);
        assert_eq!(s[2].4, "abandoned");
        assert_eq!(s[3].4, "fresh");
        assert_eq!(s[0].3.as_deref(), Some("stop"));
        assert_eq!(s[1].3.as_deref(), Some("restart"));
        // After a restart the cursor is unknown, so it is at the end anyway.
        assert_eq!(s[1].4, "one two");
    }

    /// Ingests the log in many small pieces, recreating the ingester (a crash
    /// and restart) after every piece, and compares with one uninterrupted pass.
    #[test]
    fn crash_recovery_reproduces_an_uninterrupted_run() {
        let mut l = Log::new();
        let k = "org.kde.konsole";
        let mut t = l.typ(T0, "notes", "n", "hello wrold");
        l.key(t, "notes", "n", "Left", &[Mod::Ctrl]);
        t = l.typ(t + 100, "notes", "n", "\u{8}");
        l.key(t, "notes", "n", "End", &[]);
        t = l.typ(t + 100, k, "~", "git status\n");
        l.key(t, k, "~", "Up", &[]);
        t = l.typ(t + 100, k, "~", " -s");
        t = l.typ(t + 5_000, "notes", "n", " again");
        l.key(t, "notes", "n", "a", &[Mod::Ctrl]);
        t = l.typ(t + 100, "notes", "n", "replaced");
        l.key(t, "notes", "n", "z", &[Mod::Ctrl]);
        t = l.typ(t + 40_000, k, "~", "\n");
        t = l.typ(t + 100, "notes", "n", "!");
        let t = type_renaming(
            &mut l,
            t + 100,
            "code",
            "renamed doc",
            "Untitled-1 - Visual Studio Code",
        );
        l.typ(
            t + 31_000,
            "code",
            "● renamed doc • Untitled-1 - Visual Studio Code",
            "?",
        );
        let total = l.w.len();

        let mut full = l.ingester("full.db");
        run(&mut full, clock_late);
        let expected = sessions(full.connection());
        let texts: Vec<&str> = expected.iter().map(|r| r.4.as_str()).collect();
        assert!(
            texts.contains(&"renamed doc") && texts.contains(&"renamed doc?"),
            "{texts:?}"
        );

        // Restart after every line, at the ingest level: each "process" only
        // gets to ingest one more line before it dies.
        let db = "crashy.db";
        let path = l.log_path();
        let mut offsets = vec![0u64];
        let mut r = crate::eventlog::LogReader::open(&path, 0).unwrap();
        while r.next_line().unwrap().is_some() {
            offsets.push(r.offset());
        }
        assert_eq!(*offsets.last().unwrap(), total);
        let bytes = std::fs::read(&path).unwrap();
        let partial = l.dir.join("partial.jsonl");
        for &end in &offsets[1..] {
            std::fs::write(&partial, &bytes[..end as usize]).unwrap();
            let mut ing =
                Ingester::new(store::open_writer(&l.dir.join(db)).unwrap(), &partial).unwrap();
            ing.step_at(clock_t0).unwrap();
        }
        let mut ing =
            Ingester::new(store::open_writer(&l.dir.join(db)).unwrap(), &partial).unwrap();
        run(&mut ing, clock_late);
        assert_eq!(sessions(ing.connection()), expected);
        assert_eq!(
            context_rows(ing.connection()),
            context_rows(full.connection())
        );
    }

    fn context_rows(c: &Connection) -> Vec<(String, String, String, i64)> {
        let mut st = c
            .prepare("SELECT app, window, text, cursor FROM context_state ORDER BY app, window")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
}
