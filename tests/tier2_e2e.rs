//! End to end through ingest: a VS Code session in the log is snapshotted
//! when it opens, marked pending when it closes, replayed by the worker in a
//! real (sandboxed, headless) VS Code, and its tier 2 text written back.
//!
//! Needs /usr/bin/code, so it is ignored by default:
//! `cargo test --release --test tier2_e2e -- --ignored`

use brain::event::{Event, EventKind, Mod, SystemState};
use brain::eventlog::LogWriter;
use brain::ingest::Ingester;
use brain::reconstruct::tier2::{Tier2Worker, VsCodeSnapshots};
use brain::store;
use brain::timefmt::now_ms;
use rusqlite::Connection;
use std::time::{Duration, Instant};

#[test]
#[ignore]
fn vscode_session_is_upgraded_to_tier_2() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let file = project.join("main.js");
    std::fs::write(&file, "const a = 1;\n").unwrap();
    std::thread::sleep(Duration::from_millis(20)); // file mtime < session start

    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let log_path = data.join("events.jsonl");
    let db_path = data.join("brain.db");
    let mut log = LogWriter::open(&log_path).unwrap();
    // Title with the absolute path (window.title = ${activeEditorLong}).
    let title = format!("{} - project - Visual Studio Code", file.display());
    let mut ts = now_ms();
    let mut key = |log: &mut LogWriter, k: &str, mods: Vec<Mod>| {
        log.append(&Event {
            ts,
            app: "code".into(),
            kind: EventKind::Key {
                key: k.into(),
                mods,
                repeat: false,
                window: title.clone(),
            },
        })
        .unwrap();
        ts += 50;
    };
    // At the end of the file: a function with VS Code's auto-closing braces
    // and auto-indent doing part of the work.
    for c in "function f() {".chars() {
        let mods = if "(){".contains(c) {
            vec![Mod::Shift]
        } else {
            vec![]
        };
        key(&mut log, &c.to_string(), mods);
    }
    key(&mut log, "Enter", vec![]);
    for c in "return a;".chars() {
        key(&mut log, &c.to_string(), vec![]);
    }
    log.append(&Event {
        ts,
        app: "unknown".into(),
        kind: EventKind::System {
            state: SystemState::Stop,
        },
    })
    .unwrap();

    let vscode_dir = data.join("vscode");
    let snaps = VsCodeSnapshots::new(vscode_dir.join("snapshots")).unwrap();
    let mut ing = Ingester::with_tier2(
        store::open_writer(&db_path).unwrap(),
        &log_path,
        Some(Box::new(snaps)),
    )
    .unwrap();
    ing.catch_up(10).unwrap();
    let jobs = ing.take_tier2_jobs();
    assert_eq!(jobs.len(), 1, "one pending VS Code session");

    let c = Connection::open(&db_path).unwrap();
    let (tier0, status): (String, String) = c
        .query_row(
            "SELECT reconstructed_text, tier2_status FROM sessions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "pending");
    // Tier 0/1 knows nothing of the file nor of auto-closing.
    assert_eq!(tier0, "function f() {\nreturn a;");

    let (tx, rx) = std::sync::mpsc::channel();
    let worker = Tier2Worker::spawn(db_path.clone(), log_path.clone(), vscode_dir, tx).unwrap();
    // The worker also picks pending jobs up by itself at start; submitting
    // again is harmless (the second run finds it no longer pending).
    let started = Instant::now();
    let (id, result) = rx
        .recv_timeout(Duration::from_secs(180))
        .expect("tier 2 result");
    eprintln!("tier 2 replay took {:?}: {result:?}", started.elapsed());
    worker.stop();
    assert_eq!(id, jobs[0]);
    ing.add_tier2_result(id, result);
    ing.catch_up(10).unwrap();

    let (text, tier, status): (String, i64, String) = c
        .query_row(
            "SELECT reconstructed_text, tier_used, tier2_status FROM sessions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, "done");
    assert_eq!(tier, 2);
    // VS Code closed the brace and indented the body; the unchanged first
    // line of the file is not part of the session's text.
    assert_eq!(text, "function f() {\n    return a;\n}");
    let hits: i64 = c
        .query_row(
            "SELECT count(*) FROM sessions_fts WHERE sessions_fts MATCH 'return'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
    assert!(
        !data
            .join("vscode/snapshots")
            .read_dir()
            .unwrap()
            .any(|_| true),
        "snapshot removed once applied"
    );
}
