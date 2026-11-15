//! `kill -9` at random moments, then restart: no corruption, no gaps, no duplicates.
//!
//! Drives the real `brain` binary with its synthetic input source (the evdev
//! read is the only part replaced), so it runs unattended and without root.

use brain::eventlog::LogReader;
use brain::ingest::CHECKPOINT_INTERVAL_MS;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn spawn_brain(dir: &Path, rate: u32) -> Child {
    Command::new(env!("CARGO_BIN_EXE_brain"))
        .args(["run", "--synthetic", &rate.to_string(), "--data-dir"])
        .arg(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn brain")
}

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
        let mut child = spawn_brain(&dir, 3000);
        wait_for_file(&db);
        std::thread::sleep(Duration::from_millis(rng.range_ms(50, 1500)));
        child.kill().unwrap(); // SIGKILL
        child.wait().unwrap();

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
    let child = spawn_brain(&dir, 50);
    std::thread::sleep(Duration::from_millis(1500));
    // SAFETY: sending SIGTERM to our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "clean stop must exit 0");

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

    // ...and its derived rows are exactly what one uninterrupted pass over the
    // log produces: across 25 kills, nothing was skipped or applied twice.
    let mut expected: Vec<(String, String, i64)> = Vec::new();
    let mut last: HashMap<String, i64> = HashMap::new();
    for (offset, ev) in &events {
        for app in ["*", ev.app.as_str()] {
            let due = last
                .get(app)
                .is_none_or(|prev| (ev.ts - prev).abs() >= CHECKPOINT_INTERVAL_MS);
            if due {
                let ts = brain::timefmt::format_ms(ev.ts);
                // PRIMARY KEY (app, ts): a same-millisecond duplicate is ignored.
                if !expected.iter().any(|(a, t, _)| a == app && *t == ts) {
                    expected.push((app.to_string(), ts, *offset as i64));
                }
                last.insert(app.to_string(), ev.ts);
            }
        }
    }
    let mut stmt = c
        .prepare("SELECT app, ts, byte_offset FROM log_checkpoints ORDER BY byte_offset, app")
        .unwrap();
    let actual: Vec<(String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    expected.sort_by(|a, b| (a.2, &a.0).cmp(&(b.2, &b.0)));
    assert_eq!(actual, expected);

    eprintln!(
        "{} events, {} torn tails repaired across 25 kills",
        events.len(),
        torn_tails
    );
}
