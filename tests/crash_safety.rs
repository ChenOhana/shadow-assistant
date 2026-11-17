//! `kill -9` at random moments, then restart: no corruption, no gaps, no duplicates.
//!
//! Drives the real `brain` binary with its synthetic input source (the evdev
//! read is the only part replaced), so it runs unattended and without root.

mod common;

use brain::eventlog::LogReader;
use brain::ingest::CHECKPOINT_INTERVAL_MS;
use common::{assert_no_brain_on, spawn_brain};
use rusqlite::Connection;
use std::path::Path;
use std::time::{Duration, Instant};

/// xorshift64*: deterministic, dependency-free randomness for kill timing.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn range_ms(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo)
    }
}

fn wait_for_file(p: &Path) {
    let t = Instant::now();
    while !p.exists() {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "{} never appeared",
            p.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn kill_dash_nine_never_corrupts() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let log = dir.join("events.jsonl");
    let db = dir.join("brain.db");
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut torn_tails = 0;

    for _ in 0..25 {
        // A high key rate makes it likely that some kills land mid-write.
        let mut child = spawn_brain(&["run", "--synthetic", "3000"], &dir);
        wait_for_file(&db);
        std::thread::sleep(Duration::from_millis(rng.range_ms(50, 1500)));
        child.0.kill().unwrap(); // SIGKILL
        child.0.wait().unwrap();

        let bytes = std::fs::read(&log).unwrap();
        if bytes.last().is_some_and(|&b| b != b'\n') {
            torn_tails += 1;
        }
        // The database is never corrupt, and its offset is at a line boundary
        // inside the log.
        let c = Connection::open(&db).unwrap();
        let ok: String = c
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
        let off: i64 = c
            .query_row("SELECT log_offset FROM ingest_state", [], |r| r.get(0))
            .unwrap();
        let off = off as usize;
        assert!(off <= bytes.len());
        assert!(
            off == 0 || bytes[off - 1] == b'\n',
            "offset {off} not at a line boundary"
        );
    }

    // A final clean run: start, let ingest catch up, stop with SIGTERM.
    let mut child = spawn_brain(&["run", "--synthetic", "50"], &dir);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(child.stop().success(), "clean stop must exit 0");
    assert_no_brain_on(&dir);

    // Every line in the log is a complete, valid event.
    let mut reader = LogReader::open(&log, 0).unwrap();
    let mut events = Vec::new();
    while let Some(line) = reader.next_line().unwrap() {
        let ev = line
            .event
            .unwrap_or_else(|| panic!("invalid line at offset {}", line.offset));
        events.push((line.offset, ev));
    }
    let log_len = std::fs::metadata(&log).unwrap().len();
    assert_eq!(reader.offset(), log_len, "no torn tail after a clean stop");
    assert!(
        events.len() > 1000,
        "expected plenty of events, got {}",
        events.len()
    );

    // Ingest covered the whole log...
    let c = Connection::open(&db).unwrap();
    let off: i64 = c
        .query_row("SELECT log_offset FROM ingest_state", [], |r| r.get(0))
        .unwrap();
    assert_eq!(off as u64, log_len);

    // ...and its checkpoints are exactly what one uninterrupted pass over the
    // log produces: across 25 kills, nothing was skipped or applied twice.
    let mut expected: Vec<(String, i64)> = Vec::new();
    let mut last: Option<i64> = None;
    for (offset, ev) in &events {
        if last.is_none_or(|prev| (ev.ts - prev).abs() >= CHECKPOINT_INTERVAL_MS) {
            let ts = brain::timefmt::format_ms(ev.ts);
            // PRIMARY KEY (ts): a same-millisecond duplicate is ignored.
            if !expected.iter().any(|(t, _)| *t == ts) {
                expected.push((ts, *offset as i64));
            }
            last = Some(ev.ts);
        }
    }
    let mut stmt = c
        .prepare("SELECT ts, byte_offset FROM log_checkpoints ORDER BY byte_offset")
        .unwrap();
    let actual: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(actual, expected);

    // Sessions and documents are a pure function of the log: rebuilding the
    // index from the log alone gives exactly the rows the crashing runs built.
    let fresh = tmp.path().join("rebuild.db");
    let mut ing =
        brain::ingest::Ingester::new(brain::store::open_writer(&fresh).unwrap(), &log).unwrap();
    while ing.step().unwrap().lines > 0 {}
    let rebuilt = Connection::open(&fresh).unwrap();
    let live_sessions = session_rows(&c);
    assert_eq!(live_sessions, session_rows(&rebuilt));
    assert_eq!(context_rows(&c), context_rows(&rebuilt));
    // The synthetic source types continuously, so each run is one session,
    // closed by the next run's start (after a kill) or by the final stop.
    assert!(
        live_sessions.len() >= 20,
        "got {} sessions",
        live_sessions.len()
    );
    assert!(live_sessions.iter().all(|r| r.4 == "closed"));
    assert!(live_sessions.iter().any(|r| {
        r.6.contains("the migration is blocked on the schema change.")
    }));

    eprintln!(
        "{} events, {} sessions, {} torn tails repaired across 25 kills",
        events.len(),
        live_sessions.len(),
        torn_tails
    );
}

type SessionRow = (
    i64,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    i64,
    Option<i64>,
);

fn session_rows(c: &Connection) -> Vec<SessionRow> {
    let mut st = c
        .prepare(
            "SELECT id, app, window, started_at, status, end_reason, reconstructed_text,
                    start_offset, end_offset FROM sessions ORDER BY id",
        )
        .unwrap();
    st.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
        ))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
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
