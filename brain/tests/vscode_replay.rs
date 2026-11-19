//! Tier-2 replay against the real VS Code. Ignored by default (they need
//! /usr/bin/code and take a few seconds each):
//!
//!     cargo test --test vscode_replay -- --ignored --test-threads=1
//!
//! To watch VS Code while they run, add `BRAIN_VSCODE_VISIBLE=1` in front
//! (don't type meanwhile: the window takes focus).

use brain::event::Mod;
use brain::reconstruct::vscode::{ReplayEvent, ReplayInput, VsCodeReplayer};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One instance at a time keeps the machine responsive and timings honest.
static SERIAL: Mutex<()> = Mutex::new(());

const TIMEOUT: Duration = Duration::from_secs(60);

fn key(k: &str) -> ReplayEvent {
    let shift = k.chars().count() == 1
        && k.chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase() || "~!@#$%^&*()_+{}|:\"<>?".contains(c));
    ReplayEvent::Key {
        key: k.into(),
        mods: if shift { vec![Mod::Shift] } else { vec![] },
    }
}

fn ctrl(k: &str) -> ReplayEvent {
    ReplayEvent::Key {
        key: k.into(),
        mods: vec![Mod::Ctrl],
    }
}

fn typed(s: &str) -> Vec<ReplayEvent> {
    s.chars()
        .map(|c| {
            if c == '\n' {
                key("Enter")
            } else {
                key(&c.to_string())
            }
        })
        .collect()
}

fn replay(r: &mut VsCodeReplayer, name: &str, initial: &str, events: &[ReplayEvent]) -> String {
    r.replay(
        &ReplayInput {
            file_name: name,
            initial: initial.as_bytes(),
            events,
        },
        TIMEOUT,
    )
    .expect("replay")
}

/// Processes whose command line mentions `needle`, excluding ourselves.
fn procs_with(needle: &str) -> Vec<String> {
    let me = std::process::id().to_string();
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.chars().all(|c| c.is_ascii_digit()) || name == me {
            continue;
        }
        if let Ok(c) = std::fs::read(e.path().join("cmdline")) {
            let c = String::from_utf8_lossy(&c).replace('\0', " ");
            if c.contains(needle) {
                out.push(format!("{name}: {c}"));
            }
        }
    }
    out
}

fn rss_kib_of_group(pgid: u32) -> u64 {
    let mut total = 0;
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        // Fields after the ")" of comm: state ppid pgrp ...
        let Some(rest) = stat.rsplit_once(')').map(|x| x.1) else {
            continue;
        };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.get(2).and_then(|p| p.parse::<u32>().ok()) == Some(pgid)
            && let Ok(status) = std::fs::read_to_string(e.path().join("status"))
            && let Some(l) = status.lines().find(|l| l.starts_with("VmRSS:"))
        {
            total += l
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
        }
    }
    total
}

#[test]
#[ignore]
fn vscode_plain_typing_and_backspace() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let mut r = VsCodeReplayer::new(tmp.path().join("vscode"));
    let t0 = Instant::now();
    let mut ev = typed("hello wrold");
    ev.extend([
        key("Backspace"),
        key("Backspace"),
        key("Backspace"),
        key("Backspace"),
    ]);
    ev.extend(typed("orld!"));
    let out = replay(&mut r, "notes.txt", "first line\n", &ev);
    eprintln!("cold replay: {:?}", t0.elapsed());
    assert_eq!(out, "first line\nhello world!");
}

#[test]
#[ignore]
fn vscode_auto_close_and_auto_indent() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let mut r = VsCodeReplayer::new(tmp.path().join("vscode"));
    // Real VS Code: "(" auto-closes, "{" + Enter puts the cursor on an
    // indented line between the braces, and typing ")" over the auto-inserted
    // one does not double it.
    let mut ev = typed("function f(");
    ev.extend(typed(") {"));
    ev.push(key("Enter"));
    ev.extend(typed("return 1;"));
    let out = replay(&mut r, "a.js", "", &ev);
    assert_eq!(out, "function f() {\n    return 1;\n}");
}

#[test]
#[ignore]
fn vscode_undo() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let mut r = VsCodeReplayer::new(tmp.path().join("vscode"));
    let mut ev = typed("keep");
    ev.push(key("Enter"));
    ev.extend(typed("oops"));
    ev.push(ctrl("z"));
    let out = replay(&mut r, "u.txt", "", &ev);
    // Real VS Code: the undo stop falls after "keep", so Enter and "oops"
    // are undone together (tier 0 cannot know this).
    assert_eq!(out, "keep");
}

#[test]
#[ignore]
fn vscode_paste() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let mut r = VsCodeReplayer::new(tmp.path().join("vscode"));
    let ev = vec![
        key("Home"),
        key("X"),
        key("End"),
        ctrl("v"),
        ReplayEvent::Paste {
            text: " pasted\ttext".into(),
        },
    ];
    let out = replay(&mut r, "p.txt", "abc", &ev);
    assert_eq!(out, "Xabc pasted\ttext");
}

#[test]
#[ignore]
fn vscode_hebrew_text() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let mut r = VsCodeReplayer::new(tmp.path().join("vscode"));
    let out = replay(&mut r, "h.md", "", &typed("שלום world"));
    assert_eq!(out, "שלום world");
}

#[test]
#[ignore]
fn vscode_warm_reuse_without_leakage() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let mut r = VsCodeReplayer::new(tmp.path().join("vscode"));
    let t0 = Instant::now();
    let first = replay(&mut r, "same.txt", "one\n", &typed("first job"));
    let cold = t0.elapsed();
    let pid = r.instance_pid();
    let pg = pid.unwrap();
    eprintln!(
        "instance RSS (process group): {} MiB",
        rss_kib_of_group(pg) / 1024
    );

    // ~200 keys.
    let text: String = "the quick brown fox jumps over the lazy dog. ".repeat(5);
    let t1 = Instant::now();
    let second = replay(&mut r, "same.txt", "two\n", &typed(&text));
    let warm = t1.elapsed();
    eprintln!(
        "cold {cold:?}, warm {warm:?} for {} keys",
        text.chars().count()
    );
    assert_eq!(first, "one\nfirst job");
    assert_eq!(second, format!("two\n{text}"));
    assert_eq!(r.instance_pid(), pid, "the warm instance was reused");
    assert!(
        warm < cold,
        "warm replay should be faster than a cold start"
    );

    // A different language, same instance.
    let third = replay(&mut r, "x.rs", "", &typed("fn main() {"));
    assert_eq!(third, "fn main() {}");
}

#[test]
#[ignore]
fn vscode_timeout_fails_and_leaves_no_processes() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("vscode");
    let needle = work.to_string_lossy().to_string();
    let mut r = VsCodeReplayer::new(work.clone());
    // Far too short for a cold start.
    let t0 = Instant::now();
    let res = r.replay(
        &ReplayInput {
            file_name: "t.txt",
            initial: b"",
            events: &typed("abc"),
        },
        Duration::from_millis(800),
    );
    assert!(res.is_err());
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "took {:?}",
        t0.elapsed()
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(procs_with(&needle), Vec::<String>::new());

    // The replayer recovers on the next call.
    let ok = replay(&mut r, "t.txt", "", &typed("abc"));
    assert_eq!(ok, "abc");
    drop(r);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(procs_with(&needle), Vec::<String>::new());
}

#[test]
#[ignore]
fn vscode_kill_handle_aborts_in_flight_replay() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("vscode");
    let needle = work.to_string_lossy().to_string();
    let mut r = VsCodeReplayer::new(work);
    let kill = r.kill_handle();
    kill.kill(); // no-op when nothing runs
    let long: Vec<ReplayEvent> = typed(&"x".repeat(20_000));
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(8));
        let t = Instant::now();
        kill.kill();
        kill.kill(); // idempotent
        t
    });
    let res = r.replay(
        &ReplayInput {
            file_name: "k.txt",
            initial: b"",
            events: &long,
        },
        TIMEOUT,
    );
    let killed_at = killer.join().unwrap();
    assert!(res.is_err());
    assert!(
        killed_at.elapsed() < Duration::from_secs(2),
        "replay did not fail fast"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(procs_with(&needle), Vec::<String>::new());
}

#[test]
#[ignore]
fn vscode_instance_has_no_network() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let mut r = VsCodeReplayer::new(tmp.path().join("vscode"));
    replay(&mut r, "n.txt", "", &typed("x"));
    let pid = r.instance_pid().unwrap();
    let theirs = std::fs::read_link(format!("/proc/{pid}/ns/net")).unwrap();
    let ours = std::fs::read_link("/proc/self/ns/net").unwrap();
    assert_ne!(
        theirs, ours,
        "VS Code must run in its own network namespace"
    );
    let dev = std::fs::read_to_string(format!("/proc/{pid}/net/dev")).unwrap();
    let ifaces: Vec<&str> = dev
        .lines()
        .skip(2)
        .filter_map(|l| l.split(':').next().map(str::trim))
        .collect();
    assert_eq!(ifaces, vec!["lo"]);
    // And it is inside its own systemd scope with caps.
    let cg = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    assert!(cg.contains("brain-vscode-"), "cgroup: {cg}");
    let rel = cg.lines().find_map(|l| l.strip_prefix("0::")).unwrap();
    let max = std::fs::read_to_string(format!("/sys/fs/cgroup{rel}/memory.max")).unwrap();
    assert_eq!(max.trim(), (1500u64 << 20).to_string());
}

/// Resolves a default-format title ("<file> - <rootName> - Visual Studio
/// Code") through this user's real VS Code history. Set
/// BRAIN_TEST_VSCODE_TITLE and BRAIN_TEST_VSCODE_EXPECT to try one; skipped
/// otherwise.
#[test]
#[ignore]
fn vscode_resolves_default_title_from_history() {
    let (Some(title), Some(expect)) = (
        std::env::var_os("BRAIN_TEST_VSCODE_TITLE"),
        std::env::var_os("BRAIN_TEST_VSCODE_EXPECT"),
    ) else {
        eprintln!("skipped: BRAIN_TEST_VSCODE_TITLE / BRAIN_TEST_VSCODE_EXPECT not set");
        return;
    };
    let got = brain::reconstruct::vscode::resolve_file(&title.to_string_lossy());
    assert_eq!(got, Some(std::path::PathBuf::from(expect)));
}

/// Helper for `vscode_dies_with_its_owner`: when run with BRAIN_VSCODE_OWNER
/// set to a work dir, starts a long replay there and never returns.
#[test]
#[ignore]
fn vscode_owner_helper() {
    let Some(work) = std::env::var_os("BRAIN_VSCODE_OWNER") else {
        return;
    };
    let mut r = VsCodeReplayer::new(work.into());
    let _ = r.replay(
        &ReplayInput {
            file_name: "o.txt",
            initial: b"",
            events: &typed(&"y".repeat(50_000)),
        },
        Duration::from_secs(600),
    );
}

#[test]
#[ignore]
fn vscode_dies_with_its_owner() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("vscode");
    let needle = work.to_string_lossy().to_string();
    // `kill -9` of the process that owns the replayer (as for `brain run`).
    let mut owner = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "vscode_owner_helper", "--ignored", "--nocapture"])
        .env("BRAIN_VSCODE_OWNER", &work)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let t0 = Instant::now();
    while procs_with(&format!("{needle}/user-data")).len() < 3 {
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "VS Code never started"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_secs(5));
    owner.kill().unwrap();
    owner.wait().unwrap();
    let t1 = Instant::now();
    while !procs_with(&needle).is_empty() {
        assert!(
            t1.elapsed() < Duration::from_secs(10),
            "orphans after owner was killed: {:?}",
            procs_with(&needle)
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    eprintln!(
        "instance gone {:?} after its owner was killed",
        t1.elapsed()
    );

    // And a stale instance file left by the dead owner is harmless.
    let mut r = VsCodeReplayer::new(work);
    assert_eq!(replay(&mut r, "s.txt", "", &typed("ok")), "ok");
}
