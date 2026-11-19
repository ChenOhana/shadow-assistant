//! Two `brain run`s side by side, each on its own data directory: each keeps
//! to its own folder, a second run on the same folder is refused, and one
//! dying or stopping does not disturb the other.
//!
//! The synthetic test runs by default. The desktop test (`#[ignore]`) runs
//! two real captures against Kate windows: both see the same focus and keys,
//! and one stopping or being killed never unloads the other's KWin script.
//! It needs what `real_keyboard_e2e` needs:
//!
//!   cargo test --release --test two_instances -- --ignored --nocapture

mod common;

use brain::event::EventKind;
use brain::eventlog::LogReader;
use common::*;
use rusqlite::Connection;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn log_len(data: &Path) -> u64 {
    std::fs::metadata(data.join("events.jsonl")).map_or(0, |m| m.len())
}

/// Waits until the log of `data` grows past `than` bytes.
fn wait_growing(data: &Path, than: u64) {
    let t = Instant::now();
    while log_len(data) <= than {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "{} is not being written",
            data.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The lifecycle markers (`start`, `stop`, ...) in the log of `data`, in order.
fn markers(data: &Path) -> Vec<String> {
    let mut reader = LogReader::open(&data.join("events.jsonl"), 0).unwrap();
    let mut out = Vec::new();
    while let Some(line) = reader.next_line().unwrap() {
        if let Some(EventKind::System { state }) = line.event.map(|e| e.kind) {
            out.push(format!("{state:?}").to_lowercase());
        }
    }
    out
}

/// How the sessions of the synthetic editor window in `data`'s index ended,
/// in order. The synthetic source keeps typing into that window (between
/// commands in a terminal), so its session is open for the whole run.
fn editor_end_reasons(data: &Path) -> Vec<String> {
    let c = Connection::open(data.join("brain.db")).unwrap();
    let mut st = c
        .prepare("SELECT end_reason FROM sessions WHERE app = 'synthetic' ORDER BY id")
        .unwrap();
    st.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Waits until `brain query search` on `data` finds a session.
fn wait_searchable(data: &Path) {
    let t = Instant::now();
    loop {
        let out = Command::new(env!("CARGO_BIN_EXE_brain"))
            .args(["query", "search", "migration", "--data-dir"])
            .arg(data)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        if !json["results"].as_array().unwrap().is_empty() {
            return;
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "nothing searchable in {}",
            data.display()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Runs `brain` with `args` on `data`, expecting it to exit by itself within
/// 10 s (the guard kills it otherwise); returns its status and stderr.
fn run_to_exit(args: &[&str], data: &Path) -> (std::process::ExitStatus, String) {
    let mut guard = Kill(
        Command::new(env!("CARGO_BIN_EXE_brain"))
            .args(args)
            .arg("--data-dir")
            .arg(data)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let t = Instant::now();
    let status = loop {
        if let Some(status) = guard.0.try_wait().unwrap() {
            break status;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "it did not exit");
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    guard
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status, stderr)
}

#[test]
fn two_runs_on_separate_folders_stay_apart() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    let synthetic = ["run", "--synthetic", "200"];

    let mut run_a = spawn_brain(&synthetic, &a);
    let run_b = spawn_brain(&synthetic, &b);
    wait_growing(&a, 0);
    wait_growing(&b, 0);
    // Each folder's own index answers queries while both write.
    wait_searchable(&a);
    wait_searchable(&b);

    // A second run on a folder in use is refused, and leaves the first alone.
    let (status, stderr) = run_to_exit(&synthetic, &a);
    assert!(!status.success());
    assert!(
        stderr.contains("another `brain run` is already using"),
        "{stderr}"
    );
    wait_growing(&a, log_len(&a));

    // kill -9 of A does not disturb B; A then restarts on its folder.
    run_a.0.kill().unwrap();
    run_a.0.wait().unwrap();
    wait_growing(&b, log_len(&b));
    let mut run_a = spawn_brain(&synthetic, &a);
    wait_growing(&a, log_len(&a));
    wait_growing(&b, log_len(&b));

    let mut run_b = run_b;
    assert!(run_a.stop().success());
    assert!(run_b.stop().success());
    assert_no_brain_on(&a);
    assert_no_brain_on(&b);

    // Nothing was written outside the two folders...
    let mut entries: Vec<String> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, ["a", "b"]);
    // ...and nothing crossed between them: A was started twice (killed once),
    // B once, and each index is exactly its own log's.
    assert_eq!(markers(&a), ["start", "start", "stop"]);
    assert_eq!(markers(&b), ["start", "stop"]);
    assert_eq!(editor_end_reasons(&a), ["restart", "stop"]);
    assert_eq!(editor_end_reasons(&b), ["stop"]);
    for data in [&a, &b] {
        let offset: i64 = Connection::open(data.join("brain.db"))
            .unwrap()
            .query_row("SELECT log_offset FROM ingest_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(offset as u64, log_len(data), "{}", data.display());
    }
    let scratch = tempfile::tempdir().unwrap();
    assert_matches_rebuild(&a, &scratch.path().join("a.db"));
    assert_matches_rebuild(&b, &scratch.path().join("b.db"));
}

/// Whether KWin has the script `plugin` loaded.
fn kwin_script_loaded(plugin: &str) -> bool {
    let out = Command::new("qdbus6")
        .args([
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting.isScriptLoaded",
            plugin,
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim() == "true"
}

/// The texts of the Kate sessions for `file_name` in `data`'s index.
fn kate_texts(data: &Path, file_name: &str) -> Vec<String> {
    let c = Connection::open(data.join("brain.db")).unwrap();
    let mut st = c
        .prepare(
            "SELECT reconstructed_text FROM sessions
             WHERE app LIKE '%kate%' AND window LIKE ?1 ORDER BY id",
        )
        .unwrap();
    st.query_map([format!("%{file_name}%")], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
#[ignore]
fn two_real_captures_share_the_desktop() {
    let restore = use_first_layout();
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    let (log_a, log_b) = (a.join("events.jsonl"), b.join("events.jsonl"));
    let pid = std::process::id();
    let (name_1, name_2) = (
        format!("brain-two-1-{pid}.txt"),
        format!("brain-two-2-{pid}.txt"),
    );
    let (file_1, file_2) = (tmp.path().join(&name_1), tmp.path().join(&name_2));
    std::fs::write(&file_1, "").unwrap();
    std::fs::write(&file_2, "").unwrap();

    let mut run_a = start_brain(&a);
    let mut run_b = start_brain(&b);
    let script_a = format!("brain_focus_{}", run_a.0.id());
    let script_b = format!("brain_focus_{}", run_b.0.id());
    let mut kb = VirtualKeyboard::new();
    std::thread::sleep(Duration::from_millis(1500));

    // Both see the same focus change and the same keys.
    let kate_1 = open_kate(&file_1);
    wait_kate_focused(&log_a, &name_1);
    wait_kate_focused(&log_b, &name_1);
    std::thread::sleep(Duration::from_millis(1500));
    assert_kate_focused(&log_a, &name_1);
    kb.type_str("hello");
    std::thread::sleep(Duration::from_millis(500));

    // A stops cleanly and unloads its own script, not B's.
    assert!(run_a.stop().success());
    assert!(!kwin_script_loaded(&script_a));
    assert!(kwin_script_loaded(&script_b));
    let kate_2 = open_kate(&file_2);
    wait_kate_focused(&log_b, &name_2);

    // kill -9 of B leaves its script loaded; its restart replaces it and
    // still follows focus.
    run_b.0.kill().unwrap();
    run_b.0.wait().unwrap();
    let mut run_b = start_brain(&b);
    let script_b2 = format!("brain_focus_{}", run_b.0.id());
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!kwin_script_loaded(&script_b), "stale script not unloaded");
    activate_window(&name_1);
    wait_kate_focused(&log_b, &name_1);

    drop(kate_1);
    drop(kate_2);
    drop(kb);
    assert!(run_b.stop().success());
    assert_no_brain_on(&a);
    assert_no_brain_on(&b);
    drop(restore);
    assert!(
        !kwin_script_loaded(&script_b2),
        "a brain script is still loaded"
    );

    // The same typing gave the same session text in both folders.
    let (texts_a, texts_b) = (kate_texts(&a, &name_1), kate_texts(&b, &name_1));
    eprintln!("a: {texts_a:?}  b: {texts_b:?}");
    assert_eq!(texts_a, ["hello"]);
    assert_eq!(texts_b, ["hello"]);
}
