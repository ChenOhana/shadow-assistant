//! `brain query` through the real binary: output shapes, search semantics,
//! time windows, and queries running while `brain run` writes.

use brain::event::{Event, EventKind, Mod};
use brain::eventlog::LogWriter;
use brain::ingest::Ingester;
use brain::store;
use brain::timefmt::{Millis, format_ms, now_ms, parse_rfc3339_ms};
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn brain(dir: &Path, args: &[&str]) -> (i32, Value, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_brain"))
        .args(["query"])
        .args(args)
        .arg("--data-dir")
        .arg(dir)
        .output()
        .unwrap();
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let json = if code == 0 {
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON ({e}): {stdout}"))
    } else {
        Value::Null
    };
    (code, json, String::from_utf8(out.stderr).unwrap())
}

/// An output timestamp as epoch milliseconds (output is local time).
fn instant(v: &Value) -> Millis {
    parse_rfc3339_ms(v.as_str().unwrap_or_else(|| panic!("not a time: {v}"))).unwrap()
}

/// Output times are local, with the local offset (not UTC "Z").
fn assert_local(v: &Value) {
    let s = v.as_str().unwrap();
    let ms = parse_rfc3339_ms(s).unwrap();
    assert_eq!(s, brain::timefmt::format_local(ms), "not local time: {s}");
}

fn ids(v: &Value) -> Vec<String> {
    v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["session_id"].as_str().unwrap().to_string())
        .collect()
}

/// A data dir built from a hand-written log, ingested with the real engine.
struct Fixture {
    _tmp: tempfile::TempDir,
    dir: std::path::PathBuf,
    t0: Millis,
}

fn key(w: &mut LogWriter, ts: Millis, app: &str, win: &str, key: &str, mods: &[Mod]) {
    w.append(&Event {
        ts,
        app: app.into(),
        kind: EventKind::Key {
            key: key.into(),
            mods: mods.to_vec(),
            repeat: false,
            window: win.into(),
        },
    })
    .unwrap();
}

fn typ(w: &mut LogWriter, ts: Millis, app: &str, win: &str, s: &str) -> Millis {
    let mut t = ts;
    for c in s.chars() {
        let k = match c {
            '\n' => "Enter".to_string(),
            '\u{8}' => "Backspace".to_string(),
            c => c.to_string(),
        };
        key(w, t, app, win, &k, &[]);
        t += 100;
    }
    t
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let mut w = LogWriter::open(&dir.join("events.jsonl")).unwrap();
    // Everything happened in the last two hours, so relative flags apply.
    let t0 = now_ms() - 2 * 3_600_000;
    let kate = ("org.kde.kate", "notes.md — Kate");
    // Session 1 (t0): "the cat sat", then "secret" typed and deleted.
    let t = typ(&mut w, t0, kate.0, kate.1, "the cat sat secret");
    typ(&mut w, t, kate.0, kate.1, &"\u{8}".repeat(7));
    // Session 2 (+1 h): deletes "cat" from the document ("the  sat"), adds Hebrew.
    let t1 = t0 + 3_600_000;
    // Home, right past "the ", Delete "cat", then type at the end.
    key(&mut w, t1, kate.0, kate.1, "Home", &[]);
    let mut t = t1 + 100;
    for _ in 0..4 {
        key(&mut w, t, kate.0, kate.1, "Right", &[]);
        t += 100;
    }
    for _ in 0..3 {
        key(&mut w, t, kate.0, kate.1, "Delete", &[]);
        t += 100;
    }
    key(&mut w, t, kate.0, kate.1, "End", &[]);
    typ(&mut w, t + 100, kate.0, kate.1, " גרתי בבית");
    // Session 3 (+90 min): a shell command in a terminal.
    let t2 = t0 + 90 * 60_000;
    typ(&mut w, t2, "org.kde.konsole", "~", "git status\n");
    // Session 4 (+115 min, i.e. ~5 min ago): a paste in Chrome.
    let t3 = t0 + 115 * 60_000;
    key(
        &mut w,
        t3,
        "google-chrome",
        "New tab - Google Chrome",
        "v",
        &[Mod::Ctrl],
    );
    w.append(&Event {
        ts: t3,
        app: "google-chrome".into(),
        kind: EventKind::Paste {
            text: Some("pasted words".into()),
            window: "New tab - Google Chrome".into(),
        },
    })
    .unwrap();
    key(
        &mut w,
        t3 + 100,
        "google-chrome",
        "New tab - Google Chrome",
        "Enter",
        &[],
    );
    drop(w);
    let log = dir.join("events.jsonl");

    let mut ing = Ingester::new(store::open_writer(&dir.join("brain.db")).unwrap(), &log).unwrap();
    while ing.step().unwrap().lines > 0 {}
    Fixture { _tmp: tmp, dir, t0 }
}

#[test]
fn search_semantics_and_shapes() {
    let f = fixture();
    let d = &f.dir;

    // Output shape (the assignment's fields, plus window).
    let (code, v, _) = brain(d, &["search", "sat"]);
    assert_eq!(code, 0);
    let r = &v["results"][0];
    for field in [
        "session_id",
        "app",
        "window",
        "started_at",
        "ended_at",
        "snippet",
    ] {
        assert!(r.get(field).is_some(), "missing {field}: {r}");
    }
    assert_eq!(r["app"], "org.kde.kate");
    assert_local(&r["started_at"]);
    assert_local(&r["ended_at"]);
    // Newest first: session 2 (still contains "sat"), then session 1.
    assert_eq!(ids(&v), vec!["2", "1"]);

    // A word typed and deleted within a session is never found.
    assert!(ids(&brain(d, &["search", "secret"]).1).is_empty());
    // A word deleted by a later session stays findable through the earlier one.
    assert_eq!(ids(&brain(d, &["search", "cat"]).1), vec!["1"]);
    // Substrings, any case, every word in the same session.
    assert_eq!(ids(&brain(d, &["search", "STAT"]).1), vec!["3"]);
    assert_eq!(ids(&brain(d, &["search", "the sat"]).1), vec!["2", "1"]);
    assert!(ids(&brain(d, &["search", "cat git"]).1).is_empty());
    // Short words (no trigram index) and Hebrew with an attached prefix.
    assert_eq!(ids(&brain(d, &["search", "it"]).1), vec!["3"]);
    assert_eq!(ids(&brain(d, &["search", "בית"]).1), vec!["2"]);
    // Pasted text is part of the session.
    assert_eq!(ids(&brain(d, &["search", "pasted"]).1), vec!["4"]);
    // --app is a case-insensitive substring of the app id; --limit.
    assert_eq!(
        ids(&brain(d, &["search", "sat", "--app", "KATE"]).1),
        vec!["2", "1"]
    );
    assert!(ids(&brain(d, &["search", "sat", "--app", "chrome"]).1).is_empty());
    assert_eq!(
        ids(&brain(d, &["search", "sat", "--limit", "1"]).1),
        vec!["2"]
    );
}

#[test]
fn search_time_windows() {
    let f = fixture();
    let d = &f.dir;
    // Session 1 is ~2 h old, session 2 ~1 h, session 3 ~30 min, session 4 ~5 min.
    assert_eq!(
        ids(&brain(d, &["search", "sat", "--from", "-90m"]).1),
        vec!["2"]
    );
    assert_eq!(
        ids(&brain(d, &["search", "sat", "--to", "-90m"]).1),
        vec!["1"]
    );
    assert_eq!(
        ids(&brain(d, &["search", "sat", "--from", "-3h", "--to", "-50m"]).1),
        vec!["2", "1"]
    );
    // --at T: sessions overlapping the 10 minutes before T.
    let at = format_ms(f.t0 + 90 * 60_000 + 60_000);
    assert_eq!(ids(&brain(d, &["search", "git", "--at", &at]).1), vec!["3"]);
    assert!(ids(&brain(d, &["search", "git", "--at", "-1m"]).1).is_empty());
    // The resolved window is echoed back, in local time.
    let (_, v, _) = brain(d, &["search", "git", "--from", &at]);
    assert_eq!(instant(&v["from"]), parse_rfc3339_ms(&at).unwrap());
    assert_local(&v["from"]);
    assert_local(&v["to"]);
}

#[test]
fn text_is_reconstructed() {
    let f = fixture();
    let d = &f.dir;
    // Session 1's window, absolute bounds: "secret" was typed and deleted.
    let from = format_ms(f.t0);
    let to = format_ms(f.t0 + 5 * 60_000);
    let (code, v, _) = brain(d, &["text", "--app", "kate", "--from", &from, "--to", &to]);
    assert_eq!(code, 0);
    assert_eq!(v["text"], "the cat sat");
    assert_eq!(v["windows"][0]["window"], "notes.md — Kate");
    // The full app id, not the part that was given.
    assert_eq!(v["app"], "org.kde.kate");
    assert_eq!(v["at"], Value::Null);
    assert_local(&v["from"]);
    // --at: the 10 minutes ending at T; only that app's keys.
    let at = format_ms(f.t0 + 90 * 60_000 + 60_000);
    let (_, v, _) = brain(d, &["text", "--app", "konsole", "--at", &at]);
    assert_eq!(v["text"], "git status");
    assert_eq!(v["app"], "org.kde.konsole");
    assert_eq!(instant(&v["at"]), parse_rfc3339_ms(&at).unwrap());
    assert_local(&v["at"]);
    // Default: the last 10 minutes; the pasted text, submitted with Enter.
    let (_, v, _) = brain(d, &["text", "--app", "chrome"]);
    assert_eq!(v["text"], "pasted words");
    assert_eq!(v["app"], "google-chrome");
    // A known app with nothing typed in the window: empty text, full id.
    let (code, v, _) = brain(d, &["text", "--app", "konsole", "--at", "-1m"]);
    assert_eq!((code, v["text"].as_str()), (0, Some("")));
    assert_eq!(v["windows"], serde_json::json!([]));
    assert_eq!(v["app"], "org.kde.konsole");
    // An unknown app, or one matching several apps, is an error.
    let (code, _, err) = brain(d, &["text", "--app", "nosuchapp"]);
    assert_ne!(code, 0);
    assert!(
        err.contains("no app matches") && err.contains("org.kde.kate"),
        "{err}"
    );
    let (code, _, err) = brain(d, &["text", "--app", "org.kde"]);
    assert_ne!(code, 0);
    assert!(err.contains("several apps"), "{err}");
}

/// The detailed example from the README, through the binary: two Kate
/// windows (notes.md edited in two separate sessions) and a Konsole prompt,
/// with Hebrew, an emoji, quotes, a backslash and a Tab.
#[test]
fn text_example_from_the_readme() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let mut w = LogWriter::open(&dir.join("events.jsonl")).unwrap();
    let t0 = now_ms() - 30 * 60_000; // "10:00"
    // Types `s` (⏎ Enter, ⌫ Backspace, ⇥ Tab) from `ts`, 100 ms apart.
    let typ = |w: &mut LogWriter, ts: Millis, app: &str, win: &str, s: &str| -> Millis {
        let mut t = ts;
        for c in s.chars() {
            let k = match c {
                '⏎' => "Enter".to_string(),
                '⌫' => "Backspace".to_string(),
                '⇥' => "Tab".to_string(),
                c => c.to_string(),
            };
            key(w, t, app, win, &k, &[]);
            t += 100;
        }
        t
    };
    let kate = "org.kde.kate";
    let notes = "notes.md — Kate";
    typ(&mut w, t0, kate, notes, "Shopping list:⏎- milk⏎- breadd⌫");
    typ(
        &mut w,
        t0 + 60_000,
        kate,
        "todo.md — Kate",
        "TODO: call Dana⏎⇥send the \"Q3\" report⏎path: C:\\temp",
    );
    // 10:03: a new session on notes.md (unsaved title, same window).
    let s2 = t0 + 180_000;
    let dirty = "notes.md * — Kate";
    key(&mut w, s2, kate, dirty, "Home", &[Mod::Ctrl]);
    key(&mut w, s2 + 100, kate, dirty, "Down", &[]);
    key(&mut w, s2 + 200, kate, dirty, "End", &[]);
    key(&mut w, s2 + 300, kate, dirty, "Backspace", &[Mod::Ctrl]);
    let t = typ(&mut w, s2 + 400, kate, dirty, "oat milk");
    key(&mut w, t, kate, dirty, "End", &[Mod::Ctrl]);
    let t = typ(&mut w, t + 100, kate, dirty, "⏎- קפה");
    key(&mut w, t, kate, dirty, "v", &[Mod::Ctrl]);
    w.append(&Event {
        ts: t,
        app: kate.into(),
        kind: EventKind::Paste {
            text: Some(" ☕ x2".into()),
            window: dirty.into(),
        },
    })
    .unwrap();
    key(&mut w, t + 100, kate, dirty, "z", &[Mod::Ctrl]);
    typ(&mut w, t + 200, kate, dirty, " x2");
    // 10:05: Konsole.
    let k = "org.kde.konsole";
    let t = typ(&mut w, t0 + 300_000, k, "~", "git stauts⌫⌫⌫tus⏎");
    key(&mut w, t, k, "~", "Up", &[]);
    let t = typ(&mut w, t + 100, k, "~", " -s⏎rm -rf build");
    key(&mut w, t, k, "~", "c", &[Mod::Ctrl]);
    typ(&mut w, t + 100, k, "~", "echo \"שלום\" > out.txt⏎");
    drop(w);
    let log = dir.join("events.jsonl");
    let mut ing = Ingester::new(store::open_writer(&dir.join("brain.db")).unwrap(), &log).unwrap();
    while ing.step().unwrap().lines > 0 {}
    // The database has two sessions for notes.md; `text` does not care.
    let notes_sessions: i64 = ing
        .connection()
        .query_row(
            "SELECT count(*) FROM sessions WHERE window = ?1",
            [notes],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(notes_sessions, 2);

    let at = |ms: Millis| format_ms(t0 + ms);
    // Query 1: Kate, 10:00–10:10.
    let (code, v, err) = brain(
        &dir,
        &[
            "text",
            "--app",
            "kate",
            "--from",
            &at(0),
            "--to",
            &at(600_000),
        ],
    );
    assert_eq!(code, 0, "{err}");
    let notes_text = "Shopping list:\n- oat milk\n- bread\n- קפה x2";
    let todo_text = "TODO: call Dana\n\tsend the \"Q3\" report\npath: C:\\temp";
    assert_eq!(v["app"], "org.kde.kate");
    assert_eq!(v["text"], format!("{notes_text}\n{todo_text}"));
    assert_eq!(
        v["windows"],
        serde_json::json!([
            {"window": notes, "text": notes_text},
            {"window": "todo.md — Kate", "text": todo_text},
        ])
    );
    assert_local(&v["from"]);
    assert_local(&v["to"]);
    // Query 2: from 10:02, notes.md starts empty.
    let (_, v, _) = brain(
        &dir,
        &[
            "text",
            "--app",
            "kate",
            "--from",
            &at(120_000),
            "--to",
            &at(600_000),
        ],
    );
    assert_eq!(v["text"], "oat milk\n- קפה x2");
    assert_eq!(v["windows"].as_array().unwrap().len(), 1);
    // Query 3: Konsole, --at 10:10.
    let (_, v, _) = brain(&dir, &["text", "--app", "konsole", "--at", &at(600_000)]);
    let konsole_text = "git status\ngit status -s\nrm -rf build\necho \"שלום\" > out.txt";
    assert_eq!(v["app"], "org.kde.konsole");
    assert_eq!(v["text"], konsole_text);
    assert_eq!(
        v["windows"],
        serde_json::json!([{"window": "~", "text": konsole_text}])
    );
    // The JSON escapes quotes, backslashes, newlines and tabs; Hebrew stays UTF-8.
    let raw = Command::new(env!("CARGO_BIN_EXE_brain"))
        .args([
            "query",
            "text",
            "--app",
            "kate",
            "--from",
            &at(0),
            "--to",
            &at(600_000),
            "--data-dir",
        ])
        .arg(&dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8(raw.stdout).unwrap();
    assert!(
        stdout.contains(r#""TODO: call Dana\n\tsend the \"Q3\" report\npath: C:\\temp""#),
        "{stdout}"
    );
    assert!(stdout.contains("קפה x2"), "{stdout}");
}

#[test]
fn errors_exit_non_zero_with_a_message() {
    let f = fixture();
    let d = &f.dir;
    for args in [
        vec!["search", ""],
        vec!["search", "x", "--limit", "0"],
        vec!["search", "x", "--from", "yesterday"],
        vec!["text", "--app", "a", "--from", "-1h", "--to", "-2h"],
    ] {
        let (code, _, err) = brain(d, &args);
        assert_ne!(code, 0, "{args:?}");
        assert!(err.starts_with("brain: error: "), "{args:?}: {err}");
    }
    let (code, _, err) = brain(Path::new("/nonexistent/brain"), &["search", "x"]);
    assert_ne!(code, 0);
    assert!(err.contains("does not exist"), "{err}");
}

/// `query` in a tight loop while `run` writes: every response is complete,
/// valid JSON in the documented shape, and every search hit really contains
/// the word (no half-written session text is ever visible).
#[test]
fn queries_are_consistent_while_run_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let mut run = Command::new(env!("CARGO_BIN_EXE_brain"))
        .args(["run", "--synthetic", "200", "--data-dir"])
        .arg(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let t = Instant::now();
    while !dir.join("brain.db").exists() {
        assert!(t.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(1500));
    let mut searches = 0;
    let mut hits = 0;
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        for word in ["migration", "status", "ship"] {
            let (code, v, err) = brain(&dir, &["search", word, "--limit", "50"]);
            assert_eq!(code, 0, "{err}");
            for r in v["results"].as_array().unwrap() {
                let snippet = r["snippet"].as_str().unwrap().to_lowercase();
                assert!(snippet.contains(word), "{word} not in {snippet}");
                assert_local(&r["started_at"]);
                hits += 1;
            }
            searches += 1;
        }
        let (code, v, err) = brain(&dir, &["text", "--app", "synthetic", "--from", "-5s"]);
        assert_eq!(code, 0, "{err}");
        assert!(v["text"].is_string());
        assert_eq!(v["app"], "synthetic");
    }
    // SAFETY: SIGTERM to our own child.
    unsafe { libc::kill(run.id() as i32, libc::SIGTERM) };
    assert!(run.wait().unwrap().success());
    assert!(
        searches > 30 && hits > 0,
        "{searches} searches, {hits} hits"
    );
    eprintln!("{searches} searches during capture, {hits} hits, all consistent");
}
