//! End to end through ingest: a VS Code session in the log is snapshotted
//! when it opens, marked pending when it closes, replayed by the worker in a
//! real (sandboxed, headless) VS Code, and its tier 2 text written back.
//!
//! The replay tests need /usr/bin/code, so they are ignored by default:
//! `cargo test --release --test tier2_e2e -- --ignored --test-threads=1`.
//! The saved-file tests need no VS Code and run with `cargo test`.

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
    assert_eq!(status, "done: replayed (best effort, unverified)");
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
    assert_eq!(
        status().as_deref(),
        Some("done: replayed (best effort, unverified)")
    );
    assert_eq!(tier, 2);
    // Replayed at the end of the real file (after its final newline): the
    // session's text is the line it added.
    assert_eq!(text, "let b = 2;");
}

/// A VS Code window on a saved file, with brain's log and database, for the
/// saved-file tests (no VS Code is started).
struct SavedFileCase {
    _tmp: tempfile::TempDir,
    file: std::path::PathBuf,
    title: String,
    log_path: std::path::PathBuf,
    db_path: std::path::PathBuf,
    vscode_dir: std::path::PathBuf,
    log: LogWriter,
    ing: Ingester,
    ts: i64,
}

impl SavedFileCase {
    /// `tier2`: whether `--vscode-replay` is on.
    fn new(initial: &str, tier2: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let file = project.join("main.cpp");
        std::fs::write(&file, initial).unwrap();
        std::thread::sleep(Duration::from_millis(20)); // file mtime < session start
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let (log_path, db_path) = (data.join("events.jsonl"), data.join("brain.db"));
        let vscode_dir = data.join("vscode");
        let snaps: Option<Box<dyn brain::sessions::Tier2Snapshotter>> = if tier2 {
            Some(Box::new(
                VsCodeSnapshots::new(vscode_dir.join("snapshots")).unwrap(),
            ))
        } else {
            None
        };
        let log = LogWriter::open(&log_path).unwrap();
        let ing =
            Ingester::with_tier2(store::open_writer(&db_path).unwrap(), &log_path, snaps).unwrap();
        Self {
            title: format!("{} - project - Visual Studio Code", file.display()),
            file,
            log_path,
            db_path,
            vscode_dir,
            log,
            ing,
            ts: now_ms(),
            _tmp: tmp,
        }
    }

    fn append(&mut self, kind: EventKind) {
        let app = match kind {
            EventKind::System { .. } => "unknown",
            _ => "com.microsoft.VSCode",
        };
        self.log
            .append(&Event {
                ts: self.ts,
                app: app.into(),
                kind,
            })
            .unwrap();
        self.ts += 30;
    }

    fn focus(&mut self) {
        let window = self.title.clone();
        self.append(EventKind::Focus {
            window,
            renamed: false,
        });
    }

    fn keys(&mut self, keys: &[(&str, &[Mod])]) {
        for (k, mods) in keys {
            let window = self.title.clone();
            self.append(EventKind::Key {
                key: k.to_string(),
                mods: mods.to_vec(),
                repeat: false,
                window,
            });
        }
    }

    /// Writes the file as VS Code saves it, after every logged event so far.
    fn save(&self, content: &str) {
        while now_ms() <= self.ts {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(&self.file, content).unwrap();
    }

    /// Stops brain (closing the session) and runs the tier 2 jobs.
    fn finish(mut self) -> Vec<(String, i64, String)> {
        self.append(EventKind::System {
            state: SystemState::Stop,
        });
        self.ing.catch_up(10).unwrap();
        let jobs = self.ing.take_tier2_jobs();
        if !jobs.is_empty() {
            let (tx, rx) = std::sync::mpsc::channel();
            let worker = Tier2Worker::spawn(
                self.db_path.clone(),
                self.log_path.clone(),
                self.vscode_dir,
                tx,
            )
            .unwrap();
            for _ in &jobs {
                let (id, result) = rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("tier 2 result");
                self.ing.add_tier2_result(id, result);
            }
            worker.stop();
            self.ing.catch_up(10).unwrap();
        }
        let c = Connection::open(&self.db_path).unwrap();
        let mut st = c
            .prepare("SELECT reconstructed_text, tier_used, tier2_status FROM sessions ORDER BY id")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
}

const NONE: &[Mod] = &[];
const CTRL: &[Mod] = &[Mod::Ctrl];

/// When VS Code saved the file after the session's last keystroke (autosave,
/// or Ctrl+S at the end), the session's text is a git-style diff of the
/// saved file against its initial content: exactly what VS Code made of the
/// keys (here an autocompletion the keys alone do not show).
#[test]
fn saved_file_gives_the_session_text_without_replay() {
    let mut case = SavedFileCase::new("int main() {\n}\n", true);
    // "st" + Down×4 + Enter: an autocompletion picked from VS Code's list.
    case.keys(&[
        ("Up", NONE),
        ("End", NONE),
        ("Enter", NONE),
        ("s", NONE),
        ("t", NONE),
    ]);
    case.keys(&[
        ("Down", NONE),
        ("Down", NONE),
        ("Down", NONE),
        ("Down", NONE),
    ]);
    case.keys(&[("Enter", NONE)]);
    case.ing.catch_up(10).unwrap(); // the session opens: file copied
    case.save("int main() {\n    std\n}\n");
    assert_eq!(
        case.finish(),
        vec![(
            "--- a/main.cpp\n+++ b/main.cpp\n@@ -1,2 +1,3 @@\n int main() {\n+    std\n }".into(),
            2,
            "done: saved file".into()
        )]
    );
}

/// Shortcuts that tier 0/1 ignores still make a session in VS Code with
/// tier 2 on: Ctrl+D (duplicate the line, in the owner's keybindings) then
/// Ctrl+S gives the saved file's diff.
#[test]
fn shortcut_only_session_gets_the_saved_files_diff() {
    let mut case = SavedFileCase::new("a\nb\n", true);
    case.keys(&[("d", CTRL)]);
    case.ing.catch_up(10).unwrap(); // the session opens: file copied
    case.keys(&[("s", CTRL)]);
    case.save("a\na\nb\n");
    assert_eq!(
        case.finish(),
        vec![(
            "--- a/main.cpp\n+++ b/main.cpp\n@@ -1,2 +1,3 @@\n a\n+a\n b".into(),
            2,
            "done: saved file".into()
        )]
    );
}

/// A session of shortcuts that changed nothing on disk leaves no row: not
/// saved (no evidence of a change, and such sessions are never replayed), or
/// saved unchanged.
#[test]
fn shortcut_only_session_without_change_is_removed() {
    let mut case = SavedFileCase::new("a\nb\n", true);
    case.keys(&[("p", CTRL), ("Escape", NONE)]);
    assert_eq!(case.finish(), vec![]);

    let mut case = SavedFileCase::new("a\nb\n", true);
    case.keys(&[("s", CTRL)]);
    case.save("a\nb\n");
    assert_eq!(case.finish(), vec![]);
}

/// Without `--vscode-replay`, shortcuts open no session, as in other apps.
#[test]
fn shortcuts_make_no_session_without_tier_2() {
    let mut case = SavedFileCase::new("a\nb\n", false);
    case.keys(&[("d", CTRL), ("s", CTRL)]);
    case.save("a\na\nb\n");
    assert_eq!(case.finish(), vec![]);
}

/// Ctrl+D then Ctrl+S right after focusing the window: the file is saved
/// before ingest opens the session and could copy it, so the copy taken
/// when the window was focused is the session's initial content.
#[test]
fn file_saved_before_its_snapshot_uses_the_copy_taken_on_focus() {
    let mut case = SavedFileCase::new("a\nb\n", true);
    case.focus();
    case.ing.catch_up(10).unwrap(); // the focus is ingested: file copied
    case.keys(&[("d", CTRL), ("s", CTRL)]);
    case.save("a\na\nb\n"); // before ingest sees the keys
    assert_eq!(
        case.finish(),
        vec![(
            "--- a/main.cpp\n+++ b/main.cpp\n@@ -1,2 +1,3 @@\n a\n+a\n b".into(),
            2,
            "done: saved file".into()
        )]
    );

    // Without the focus copy, the same session cannot have a diff.
    let mut case = SavedFileCase::new("a\nb\n", true);
    case.keys(&[("d", CTRL), ("s", CTRL)]);
    case.save("a\na\nb\n");
    assert_eq!(case.finish(), vec![]);
}
