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

/// Bumped whenever the schema changes; see [`migrate`].
pub const SCHEMA_VERSION: i64 = 2;

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
  tier2_status       TEXT,                       -- VS Code replay: snapshot | pending | done | failed: .. | skipped: ..
  tier2_file         TEXT                        -- the edited file, for VS Code replay
);
CREATE INDEX idx_sessions_app_time ON sessions(app, started_at);
CREATE INDEX idx_sessions_status ON sessions(status);
CREATE INDEX idx_sessions_history ON sessions(app, end_reason, end_offset);

CREATE VIRTUAL TABLE sessions_fts USING fts5(
  reconstructed_text, content='sessions', content_rowid='id',
  tokenize='porter unicode61'
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

-- Seek points into the capture log. app = '*' rows are global (any app).
CREATE TABLE log_checkpoints (
  app         TEXT NOT NULL,
  ts          TEXT NOT NULL,
  byte_offset INTEGER NOT NULL,
  PRIMARY KEY (app, ts)
);
"#;

/// Everything [`SCHEMA`] creates, for rebuilding an index from an older schema.
const DROP_ALL: &str = "
DROP TRIGGER IF EXISTS sessions_ai;
DROP TRIGGER IF EXISTS sessions_ad;
DROP TRIGGER IF EXISTS sessions_au;
DROP TABLE IF EXISTS sessions_fts;
DROP TABLE IF EXISTS sessions;
DROP TABLE IF EXISTS context_state;
DROP TABLE IF EXISTS ingest_state;
DROP TABLE IF EXISTS log_checkpoints;
";

/// The app value used for global (all-app) checkpoints.
pub const GLOBAL_CHECKPOINT_APP: &str = "*";

/// Opens the database for the writer (`brain run`), creating and migrating it.
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
    migrate(&conn).with_context(|| format!("migrating database {}", path.display()))?;
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
             (restart `brain run` to migrate it)",
            path.display()
        );
    }
    Ok(conn)
}

/// The index is derived from the capture log, so an older schema is not
/// converted: it is dropped and rebuilt by re-ingesting the log from offset 0.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if version > SCHEMA_VERSION {
        bail!("database schema version {version} is newer than this brain ({SCHEMA_VERSION})");
    }
    if version != 0 {
        eprintln!(
            "brain: index schema {version} is older than {SCHEMA_VERSION}; \
             rebuilding it from the capture log"
        );
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(DROP_ALL)?;
    tx.execute_batch(SCHEMA)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
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

pub fn insert_checkpoint(conn: &Connection, app: &str, ts: &str, offset: u64) -> Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO log_checkpoints (app, ts, byte_offset) VALUES (?1, ?2, ?3)",
    )?
    .execute(rusqlite::params![app, ts, offset as i64])?;
    Ok(())
}

/// Latest checkpoint timestamp per app, used to resume checkpoint spacing after a restart.
pub fn latest_checkpoints(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT app, MAX(ts) FROM log_checkpoints GROUP BY app")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Byte offset of the latest global checkpoint at or before `ts`, or 0.
pub fn seek_offset_before(conn: &Connection, ts: &str) -> Result<u64> {
    let off: Option<i64> = conn
        .query_row(
            "SELECT byte_offset FROM log_checkpoints WHERE app = ?1 AND ts <= ?2
             ORDER BY ts DESC LIMIT 1",
            rusqlite::params![GLOBAL_CHECKPOINT_APP, ts],
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
    fn old_schema_is_rebuilt_from_scratch() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("b.db");
        {
            let c = Connection::open(&p).unwrap();
            c.execute_batch(
                "CREATE TABLE sessions (id INTEGER PRIMARY KEY, junk TEXT);
                 CREATE TABLE ingest_state (id INTEGER PRIMARY KEY, log_offset INTEGER);
                 INSERT INTO ingest_state VALUES (1, 999);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let c = open_writer(&p).unwrap();
        assert_eq!(ingest_offset(&c).unwrap(), 0, "re-ingest from the start");
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        c.execute(
            "INSERT INTO context_state (app, window, text, cursor, updated_at) VALUES ('a','w','',0,'t')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn checkpoint_seek() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_writer(&tmp.path().join("b.db")).unwrap();
        insert_checkpoint(&c, "*", "2026-01-01T00:00:00.000Z", 0).unwrap();
        insert_checkpoint(&c, "*", "2026-01-01T00:00:05.000Z", 500).unwrap();
        insert_checkpoint(&c, "*", "2026-01-01T00:00:10.000Z", 900).unwrap();
        insert_checkpoint(&c, "*", "2026-01-01T00:00:10.000Z", 999).unwrap(); // ignored duplicate
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
        let latest = latest_checkpoints(&c).unwrap();
        assert_eq!(
            latest,
            vec![("*".into(), "2026-01-01T00:00:10.000Z".into())]
        );
    }
}
