//! The SQLite index: derived from the capture log and rebuildable from it.
//!
//! WAL mode gives the concurrency model `brain` needs. The single writer (the
//! ingest thread inside `brain run`) never blocks readers. Each `brain query`
//! reads a consistent snapshot as of the start of its read transaction. Ingest
//! advances `ingest_state.log_offset` in the same transaction as the rows it
//! derives from those log bytes, so after a crash the index and the offset
//! always agree.
//!
//! `synchronous = NORMAL` is safe against corruption in WAL mode. A power loss
//! may drop the last few committed transactions, and the log is fsync'd
//! independently, so ingest simply re-derives them on the next start.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::path::Path;
use std::time::Duration;

/// Bumped whenever the schema changes; see `init_schema`.
pub const SCHEMA_VERSION: i64 = 4;

const SCHEMA: &str = r#"
-- One row per writing session. While a session is open its text is the live
-- reconstruction so far (refreshed every ingest step), so search sees it.
CREATE TABLE sessions (
  id                 INTEGER PRIMARY KEY,
  app                TEXT NOT NULL,
  window             TEXT NOT NULL,              -- normalized window title (session key)
  started_at         TEXT NOT NULL,              -- RFC 3339 UTC, same format as the log
  ended_at           TEXT,                       -- last activity; NULL while open
  status             TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'closed')),
  tier_used          INTEGER,
  reconstructed_text TEXT,
  start_offset       INTEGER NOT NULL,           -- log offset of the session's first event
  end_offset         INTEGER,                    -- log offset just past its last event
  end_reason         TEXT,                       -- idle | commit | cancel | field | stop | restart
  tier2_status       TEXT,                       -- VS Code replay: resolving | snapshot | pending | done | failed: .. | skipped: ..
  tier2_file         TEXT                        -- the edited file, for VS Code replay
);
CREATE INDEX idx_sessions_app_time ON sessions(app, started_at);
CREATE INDEX idx_sessions_status ON sessions(status);
CREATE INDEX idx_sessions_history ON sessions(app, end_reason, end_offset);

-- Substring search: the trigram tokenizer indexes every 3-character
-- sequence, so a query word matches anywhere inside a word ("run" finds
-- running and rerun, "בית" finds בבית), case- and accent-insensitively.
-- Query words shorter than 3 characters cannot use this index.
CREATE VIRTUAL TABLE sessions_fts USING fts5(
  reconstructed_text, content='sessions', content_rowid='id',
  tokenize='trigram case_sensitive 0 remove_diacritics 1'
);
CREATE TRIGGER sessions_ai AFTER INSERT ON sessions BEGIN
  INSERT INTO sessions_fts(rowid, reconstructed_text) VALUES (new.id, new.reconstructed_text);
END;
CREATE TRIGGER sessions_ad AFTER DELETE ON sessions BEGIN
  INSERT INTO sessions_fts(sessions_fts, rowid, reconstructed_text)
    VALUES ('delete', old.id, old.reconstructed_text);
END;
CREATE TRIGGER sessions_au AFTER UPDATE OF reconstructed_text ON sessions BEGIN
  INSERT INTO sessions_fts(sessions_fts, rowid, reconstructed_text)
    VALUES ('delete', old.id, old.reconstructed_text);
  INSERT INTO sessions_fts(rowid, reconstructed_text) VALUES (new.id, new.reconstructed_text);
END;

-- The modelled document of each (app, window) between its sessions, so a
-- later session continues where the previous one left it.
CREATE TABLE context_state (
  app            TEXT NOT NULL,
  window         TEXT NOT NULL,
  text           TEXT NOT NULL,
  cursor         INTEGER NOT NULL,
  anchor         INTEGER,
  cursor_unknown INTEGER NOT NULL DEFAULT 0,     -- brain restarted since: cursor goes to the end
  updated_at     TEXT NOT NULL,
  PRIMARY KEY (app, window)
);
CREATE INDEX idx_context_state_updated ON context_state(updated_at);

CREATE TABLE ingest_state (
  id         INTEGER PRIMARY KEY CHECK (id = 1),
  log_offset INTEGER NOT NULL DEFAULT 0          -- bytes of the capture log already ingested
);
INSERT INTO ingest_state (id, log_offset) VALUES (1, 0);

-- Seek points into the capture log, at most one per 5 s of log time: the
-- first event at or after `ts` starts at `byte_offset`. `query text` starts
-- reading the log there instead of at the beginning.
CREATE TABLE log_checkpoints (
  ts          TEXT PRIMARY KEY,
  byte_offset INTEGER NOT NULL
);
"#;

/// Opens the database for the writer (`brain run`), creating it if new.
pub fn open_writer(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("opening database {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        bail!(
            "could not enable WAL mode on {} (got {mode})",
            path.display()
        );
    }
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA cache_size = -1024;", // 1 MiB page cache: keeps `run` well inside its memory budget
    )?;
    init_schema(&conn, path)?;
    Ok(conn)
}

/// Opens the database read-only for `brain query`. Safe while `brain run` writes.
pub fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening database {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA query_only = ON; PRAGMA cache_size = -1024;")?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        bail!(
            "database {} has schema version {version}, this brain expects {SCHEMA_VERSION} \
             (delete it and restart `brain run` to rebuild it from the capture log)",
            path.display()
        );
    }
    Ok(conn)
}

/// Creates the schema in a new database. There is no migration (see
/// CLAUDE.md: no backward compatibility yet): a database from another
/// version is refused. Deleting it is safe, since the index is rebuilt from
/// the capture log on the next start.
fn init_schema(conn: &Connection, path: &Path) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    match version {
        SCHEMA_VERSION => Ok(()),
        0 => {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
            Ok(())
        }
        v => bail!(
            "{} has schema version {v}, this brain uses {SCHEMA_VERSION}; delete it \
             and it will be rebuilt from the capture log",
            path.display()
        ),
    }
}

pub fn ingest_offset(conn: &Connection) -> Result<u64> {
    let off: i64 = conn.query_row(
        "SELECT log_offset FROM ingest_state WHERE id = 1",
        [],
        |r| r.get(0),
    )?;
    Ok(off as u64)
}

pub fn set_ingest_offset(conn: &Connection, offset: u64) -> Result<()> {
    conn.execute(
        "UPDATE ingest_state SET log_offset = ?1 WHERE id = 1",
        [offset as i64],
    )?;
    Ok(())
}

pub fn insert_checkpoint(conn: &Connection, ts: &str, offset: u64) -> Result<()> {
    conn.prepare_cached("INSERT OR IGNORE INTO log_checkpoints (ts, byte_offset) VALUES (?1, ?2)")?
        .execute(rusqlite::params![ts, offset as i64])?;
    Ok(())
}

/// Timestamp of the latest checkpoint, to resume checkpoint spacing after a restart.
pub fn latest_checkpoint(conn: &Connection) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT MAX(ts) FROM log_checkpoints", [], |r| r.get(0))?)
}

/// Byte offset of the latest checkpoint at or before `ts`, or 0.
pub fn seek_offset_before(conn: &Connection, ts: &str) -> Result<u64> {
    let off: Option<i64> = conn
        .query_row(
            "SELECT byte_offset FROM log_checkpoints WHERE ts <= ?1 ORDER BY ts DESC LIMIT 1",
            [ts],
            |r| r.get(0),
        )
        .optional()?;
    Ok(off.unwrap_or(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_schema_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("b.db");
        let c = open_writer(&p).unwrap();
        assert_eq!(ingest_offset(&c).unwrap(), 0);
        set_ingest_offset(&c, 1234).unwrap();
        drop(c);
        let c = open_writer(&p).unwrap();
        assert_eq!(ingest_offset(&c).unwrap(), 1234);
        let r = open_reader(&p).unwrap();
        assert_eq!(ingest_offset(&r).unwrap(), 1234);
        assert!(
            r.execute("UPDATE ingest_state SET log_offset = 0", [])
                .is_err()
        );
    }

    #[test]
    fn fts_triggers_keep_index_in_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_writer(&tmp.path().join("b.db")).unwrap();
        c.execute(
            "INSERT INTO sessions (app, window, started_at, reconstructed_text, start_offset)
             VALUES ('a', 'w', '2026-01-01T00:00:00.000Z', 'the migration is blocked', 0)",
            [],
        )
        .unwrap();
        let hits = |q: &str| -> i64 {
            c.query_row(
                "SELECT count(*) FROM sessions_fts WHERE sessions_fts MATCH ?1",
                [q],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(hits("migration"), 1);
        c.execute(
            "UPDATE sessions SET reconstructed_text = 'nothing here'",
            [],
        )
        .unwrap();
        assert_eq!(hits("migration"), 0);
        assert_eq!(hits("nothing"), 1);
        c.execute("DELETE FROM sessions", []).unwrap();
        assert_eq!(hits("nothing"), 0);
    }

    #[test]
    fn other_schema_version_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("b.db");
        Connection::open(&p)
            .unwrap()
            .execute_batch("PRAGMA user_version = 1;")
            .unwrap();
        let e = open_writer(&p).unwrap_err().to_string();
        assert!(e.contains("delete it"), "{e}");
    }

    #[test]
    fn search_matches_substrings_in_any_script() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_writer(&tmp.path().join("b.db")).unwrap();
        for (i, t) in [
            "I was Running late",
            "rerun the job",
            "ran away",
            "גרתי בבית הישן",
            "Café",
        ]
        .iter()
        .enumerate()
        {
            c.execute(
                "INSERT INTO sessions (app, window, started_at, reconstructed_text, start_offset)
                 VALUES ('a', 'w', '2026-01-01T00:00:00.000Z', ?1, ?2)",
                rusqlite::params![t, i as i64],
            )
            .unwrap();
        }
        let hits = |q: &str| -> Vec<i64> {
            let mut st = c
                .prepare(
                    "SELECT rowid FROM sessions_fts WHERE sessions_fts MATCH ?1 ORDER BY rowid",
                )
                .unwrap();
            st.query_map([q], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(hits("\"run\""), vec![1, 2], "inside words, any case");
        assert_eq!(hits("\"בית\""), vec![4], "Hebrew with an attached prefix");
        assert_eq!(hits("\"cafe\""), vec![5], "accents ignored");
        assert_eq!(hits("\"run\" AND \"late\""), vec![1]);
        assert!(hits("\"walk\"").is_empty());
    }

    #[test]
    fn checkpoint_seek() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_writer(&tmp.path().join("b.db")).unwrap();
        assert_eq!(latest_checkpoint(&c).unwrap(), None);
        insert_checkpoint(&c, "2026-01-01T00:00:00.000Z", 0).unwrap();
        insert_checkpoint(&c, "2026-01-01T00:00:05.000Z", 500).unwrap();
        insert_checkpoint(&c, "2026-01-01T00:00:10.000Z", 900).unwrap();
        insert_checkpoint(&c, "2026-01-01T00:00:10.000Z", 999).unwrap(); // ignored duplicate
        assert_eq!(
            seek_offset_before(&c, "2026-01-01T00:00:07.000Z").unwrap(),
            500
        );
        assert_eq!(
            seek_offset_before(&c, "2026-01-01T00:00:10.000Z").unwrap(),
            900
        );
        assert_eq!(
            seek_offset_before(&c, "2025-01-01T00:00:00.000Z").unwrap(),
            0
        );
        assert_eq!(
            latest_checkpoint(&c).unwrap().as_deref(),
            Some("2026-01-01T00:00:10.000Z")
        );
    }
}
