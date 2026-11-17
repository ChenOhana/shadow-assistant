//! Ingest: tails the capture log into the SQLite index.
//!
//! Each [`Ingester::step`] reads a bounded batch of complete log lines starting at
//! the persisted offset. It writes everything it derives from them and the new
//! offset in **one transaction**. If the process dies before the commit, nothing
//! from the batch is visible and the next start re-reads the same bytes. If it
//! dies after the commit, the offset already covers the batch. Either way there
//! are no gaps and no duplicates.
//!
//! A step derives log checkpoints (seek points for `query text`) and feeds
//! every event to the [`Sessions`] engine, which opens, updates and closes
//! writing sessions and their reconstructed text in the same transaction.
//! If a step fails, the engine's in-memory state may be ahead of the
//! database, so it is rebuilt from the database and the log
//! ([`Sessions::recover`]) before the next attempt.

use crate::eventlog::LogReader;
use crate::sessions::{Sessions, Tier2Snapshotter};
use crate::store;
use crate::timefmt::{self, Millis};
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Minimum spacing between two checkpoints for the same app, in log time.
pub const CHECKPOINT_INTERVAL_MS: Millis = 5_000;
/// Upper bound on lines per transaction, which bounds memory use and commit latency.
pub const MAX_LINES_PER_STEP: usize = 5_000;

pub struct Ingester {
    conn: Connection,
    log_path: PathBuf,
    offset: u64,
    last_checkpoint: Option<Millis>,
    sessions: Option<Sessions>,
    /// Tier 2 results waiting to be written: (session id, text or error).
    tier2_results: Vec<(i64, std::result::Result<String, String>)>,
    /// Tier 2 jobs whose sessions are committed as pending, to hand to the worker.
    tier2_jobs: Vec<i64>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StepStats {
    pub lines: usize,
    pub invalid_lines: usize,
    pub checkpoints: usize,
}

/// Where the ingest clock comes from: the wall clock in production, fixed in tests.
pub type Clock = fn() -> Millis;

impl Ingester {
    pub fn new(conn: Connection, log_path: &Path) -> Result<Self> {
        Self::with_tier2(conn, log_path, None)
    }

    pub fn with_tier2(
        conn: Connection,
        log_path: &Path,
        tier2: Option<Box<dyn Tier2Snapshotter>>,
    ) -> Result<Self> {
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
        let last_checkpoint =
            store::latest_checkpoint(&conn)?.and_then(|ts| timefmt::parse_rfc3339_ms(&ts).ok());
        let sessions = Sessions::recover(&conn, log_path, offset, tier2)
            .context("recovering open sessions")?;
        Ok(Self {
            conn,
            log_path: log_path.to_owned(),
            offset,
            last_checkpoint,
            sessions: Some(sessions),
            tier2_results: Vec::new(),
            tier2_jobs: Vec::new(),
        })
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Queues a finished tier 2 replay to be written by the next step.
    pub fn add_tier2_result(&mut self, id: i64, result: std::result::Result<String, String>) {
        self.tier2_results.push((id, result));
    }

    /// Tier 2 jobs committed since the last call.
    pub fn take_tier2_jobs(&mut self) -> Vec<i64> {
        std::mem::take(&mut self.tier2_jobs)
    }

    /// Ingests at most [`MAX_LINES_PER_STEP`] new complete lines, using the
    /// wall clock for idle timeouts. Returns `lines == 0` when caught up.
    pub fn step(&mut self) -> Result<StepStats> {
        self.step_at(timefmt::now_ms)
    }

    pub fn step_at(&mut self, clock: Clock) -> Result<StepStats> {
        let r = self.try_step(clock);
        if r.is_err() {
            // The transaction rolled back; rebuild the in-memory sessions so
            // they match the database again.
            let tier2 = self.sessions.take().and_then(Sessions::into_tier2);
            match Sessions::recover(&self.conn, &self.log_path, self.offset, tier2) {
                Ok(s) => self.sessions = Some(s),
                Err(e) => eprintln!("brain: warning: could not rebuild open sessions: {e:#}"),
            }
        }
        r
    }

    fn try_step(&mut self, clock: Clock) -> Result<StepStats> {
        let Some(sessions) = self.sessions.as_mut() else {
            anyhow::bail!("session state unavailable");
        };
        let mut stats = StepStats::default();
        // Checkpoint decisions are staged and only become the in-memory state
        // once the transaction commits, so a failed commit cannot desync them.
        let mut staged: Option<Millis> = None;
        let tx = self
            .conn
            .transaction()
            .context("starting ingest transaction")?;
        let mut new_offset = self.offset;
        if self.log_path.exists() {
            let mut reader = LogReader::open(&self.log_path, self.offset)?;
            while stats.lines < MAX_LINES_PER_STEP {
                let Some(line) = reader.next_line()? else {
                    break;
                };
                stats.lines += 1;
                let Some(event) = line.event else {
                    stats.invalid_lines += 1;
                    continue;
                };
                if checkpoint_due(staged.or(self.last_checkpoint), event.ts) {
                    store::insert_checkpoint(&tx, &timefmt::format_ms(event.ts), line.offset)?;
                    staged = Some(event.ts);
                    stats.checkpoints += 1;
                }
                sessions.on_event(&tx, line.offset, reader.offset(), &event)?;
            }
            new_offset = reader.offset();
        }
        if stats.lines < MAX_LINES_PER_STEP {
            // Caught up with the log: idle sessions can close on the wall clock.
            sessions.close_idle(&tx, clock())?;
            sessions.retry_tier2(&tx)?;
        }
        let results = std::mem::take(&mut self.tier2_results);
        for (id, result) in &results {
            apply_tier2_result(&tx, *id, result)?;
        }
        sessions.flush(&tx)?;
        let wrote = sessions.take_wrote();
        if stats.lines == 0 && !wrote && results.is_empty() {
            return Ok(stats); // nothing to do: dropping `tx` is a no-op rollback
        }
        if new_offset != self.offset {
            store::set_ingest_offset(&tx, new_offset)?;
        }
        if let Err(e) = tx.commit() {
            self.tier2_results = results; // retry them next time
            return Err(e).context("committing ingest transaction");
        }

        self.offset = new_offset;
        if staged.is_some() {
            self.last_checkpoint = staged;
        }
        let (jobs, discards) = sessions.take_committed_effects();
        self.tier2_jobs.extend(jobs);
        for id in discards {
            sessions.discard_snapshot(id);
        }
        for (id, _) in results {
            sessions.discard_snapshot(id);
        }
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

/// Writes a finished tier 2 replay over the session's tier 0/1 text. Only a
/// session still waiting for it is updated.
fn apply_tier2_result(
    tx: &Connection,
    id: i64,
    result: &std::result::Result<String, String>,
) -> Result<()> {
    match result {
        Ok(text) => tx.execute(
            "UPDATE sessions SET reconstructed_text = ?1, tier_used = 2, tier2_status = 'done'
             WHERE id = ?2 AND tier2_status = 'pending'",
            rusqlite::params![text, id],
        )?,
        Err(why) => tx.execute(
            "UPDATE sessions SET tier2_status = ?1 WHERE id = ?2 AND tier2_status = 'pending'",
            rusqlite::params![format!("failed: {why}"), id],
        )?,
    };
    Ok(())
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
    fn ingests_and_checkpoints_every_five_seconds() {
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
        // One checkpoint per 5 s of log time: t = 0, 5, 10, 15 s.
        assert_eq!(count(c, "SELECT count(*) FROM log_checkpoints"), 4);
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
            count(ing.connection(), "SELECT count(*) FROM log_checkpoints"),
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
