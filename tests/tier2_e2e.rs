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

/// Writes a fake VS Code window state for folder `project` listing `open`
/// as its open editors (the shape VS Code writes in workspaceStorage).
fn write_window_state(
    config: &std::path::Path,
    project: &std::path::Path,
    open: &[&std::path::Path],
) {
    let ws = config.join("User/workspaceStorage/0123abcd");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(
        ws.join("workspace.json"),
        format!(r#"{{"folder":"file://{}"}}"#, project.display()),
    )
    .unwrap();
    let editors: Vec<String> = open
        .iter()
        .map(|p| {
            format!(
                r#"{{"id":"workbench.editors.files.fileEditorInput","value":"{{\"resourceJSON\":{{\"fsPath\":\"{p}\",\"external\":\"file://{p}\",\"scheme\":\"file\"}}}}"}}"#,
                p = p.display()
            )
        })
        .collect();
    let db = Connection::open(ws.join("state.vscdb")).unwrap();
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
    )
    .unwrap();
    db.execute(
        "INSERT INTO ItemTable VALUES ('memento/workbench.parts.editor', ?1)",
        [format!(
            r#"{{"editorpart.state":{{"editors":[{}]}}}}"#,
            editors.join(",")
        )],
    )
    .unwrap();
}

/// The owner's case: VS Code's default title (`notes.js - project - Visual
/// Studio Code`) for a file opened from outside the window's folder, which
/// VS Code lists in its window state only a while after it was opened. The
/// lookup is retried until it appears, then the session is replayed.
#[test]
#[ignore]
fn file_outside_the_folder_is_identified_late_and_replayed() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let desktop = tmp.path().join("Desktop");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&desktop).unwrap();
    let file = desktop.join("notes.js");
    std::fs::write(&file, "const a = 1;\n").unwrap();
    std::thread::sleep(Duration::from_millis(20)); // file mtime < session start
    let config = tmp.path().join("Code");
    write_window_state(&config, &project, &[]); // not listed yet

    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let log_path = data.join("events.jsonl");
    let db_path = data.join("brain.db");
    let mut log = LogWriter::open(&log_path).unwrap();
    let title = "notes.js - project - Visual Studio Code";
    let mut ts = now_ms();
    for c in "let b = 2;".chars() {
        log.append(&Event {
            ts,
            app: "com.microsoft.VSCode".into(),
            kind: EventKind::Key {
                key: c.to_string(),
                mods: vec![],
                repeat: false,
                window: title.into(),
            },
        })
        .unwrap();
        ts += 50;
    }
    let vscode_dir = data.join("vscode");
    let snaps = VsCodeSnapshots::new(vscode_dir.join("snapshots"))
        .unwrap()
        .with_vscode_config(config.clone());
    let mut ing = Ingester::with_tier2(
        store::open_writer(&db_path).unwrap(),
        &log_path,
        Some(Box::new(snaps)),
    )
    .unwrap();
    let c = Connection::open(&db_path).unwrap();
    let status = || -> Option<String> {
        c.query_row("SELECT tier2_status FROM sessions", [], |r| r.get(0))
            .unwrap()
    };
    ing.catch_up(10).unwrap();
    assert_eq!(status().as_deref(), Some("resolving"));

    // VS Code saves its window state: the next ingest pass finds the file.
    write_window_state(&config, &project, &[&file]);
    ing.catch_up(10).unwrap();
    assert_eq!(status().as_deref(), Some("snapshot"));
    let snapshot_file: String = c
        .query_row("SELECT tier2_file FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(snapshot_file, file.to_string_lossy());

    log.append(&Event {
        ts,
        app: "unknown".into(),
        kind: EventKind::System {
            state: SystemState::Stop,
        },
    })
    .unwrap();
    ing.catch_up(10).unwrap();
    assert_eq!(status().as_deref(), Some("pending"));
    let jobs = ing.take_tier2_jobs();
    assert_eq!(jobs.len(), 1);

    let (tx, rx) = std::sync::mpsc::channel();
    let worker = Tier2Worker::spawn(db_path.clone(), log_path.clone(), vscode_dir, tx).unwrap();
    let (id, result) = rx
        .recv_timeout(Duration::from_secs(180))
        .expect("tier 2 result");
    worker.stop();
    assert_eq!(id, jobs[0]);
    ing.add_tier2_result(id, result);
    ing.catch_up(10).unwrap();
    let (text, tier): (String, i64) = c
        .query_row(
            "SELECT reconstructed_text, tier_used FROM sessions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status().as_deref(), Some("done"));
    assert_eq!(tier, 2);
    // Replayed at the end of the real file (after its final newline): the
    // session's text is the line it added.
    assert_eq!(text, "let b = 2;");
}
