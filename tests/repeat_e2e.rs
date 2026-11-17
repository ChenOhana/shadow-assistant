//! Held keys, end to end, against a real application as ground truth.
//!
//! A virtual keyboard (uinput) holds keys for precise durations in a fresh
//! Kate window while `brain run` captures. Kate's saved file is what the app
//! really received, repeats included.
//!
//! What is checked:
//! * brain's log has exactly the repeats of KDE's schedule (600 ms delay,
//!   25 Hz) for every hold that ends between two repeats; for holds ending
//!   right on a repeat's due time, within one (the test's own sleep makes
//!   the real hold a fraction of a millisecond longer than asked);
//! * brain's reconstruction matches Kate's text within one character per
//!   hold (two for holds over 2 s). Wayland apps run their own repeat
//!   timers, and Qt's default coarse timers may fire up to 5% early or late
//!   (a 600 ms delay fires anywhere in ~570–630 ms), so the exact count the
//!   app produced is not knowable from the keyboard.
//!
//! Needs a KDE Plasma session, Kate, and write access to /dev/uinput
//! (`sudo sh scripts/setup-test-permissions.sh`). It types into the focused
//! window, so it only types while `brain`'s own log shows Kate focused, and
//! stops otherwise. Do not touch the keyboard or mouse while it runs.
//!
//!   cargo test --release --test repeat_e2e -- --ignored --nocapture

mod common;

use brain::capture::synthetic::us_keycode;
use common::*;
use std::path::PathBuf;
use std::time::Duration;

/// KDE's repeat schedule (brain's model): first repeat after 600 ms, then 25 Hz.
const DELAY_MS: u64 = 600;
const PERIOD_MS: u64 = 40;

/// Characters a hold of `ms` produces in the model.
fn model_count(ms: u64) -> usize {
    if ms < DELAY_MS {
        1
    } else {
        2 + ((ms - DELAY_MS) / PERIOD_MS) as usize
    }
}

#[test]
#[ignore]
fn held_keys_match_what_kate_received() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let log = data.join("events.jsonl");
    let file_name = format!("brain-repeat-{}.txt", std::process::id());
    let target: PathBuf = tmp.path().join(&file_name);
    std::fs::write(&target, "").unwrap();

    let brain = start_brain(&data);
    let mut kb = VirtualKeyboard::new();
    std::thread::sleep(Duration::from_millis(1500)); // brain and KWin pick the device up
    let kate = open_kate(&target);
    wait_kate_focused(&log, &file_name);
    std::thread::sleep(Duration::from_millis(1500)); // let Kate finish loading

    // One distinct letter per hold, so each hold's characters can be counted.
    // Durations sit mid-way between repeats (600 + 40k + 20) except the
    // explicit boundary cases, where client timing can legitimately differ by
    // one repeat.
    let holds: &[(char, u64, bool)] = &[
        ('a', 80, false),
        ('b', 300, false),
        ('c', 620, false),
        ('d', 1020, false),
        ('e', 2020, false),
        ('f', 4020, false),
        ('g', 1000, true),
        ('h', 1001, true),
        ('i', 999, true),
        ('j', 640, true),
        // A long run for the Backspace hold below to delete into: its ±1
        // can never make a whole run vanish (which would change the order
        // of letters, not just a count).
        ('k', 2020, false),
    ];
    for &(c, ms, _) in holds {
        assert_kate_focused(&log, &file_name);
        kb.hold(us_keycode(c).unwrap().0, ms);
        std::thread::sleep(Duration::from_millis(150));
    }
    // Held Backspace deletes back into the 'k' run.
    assert_kate_focused(&log, &file_name);
    let bs_ms = 1020;
    kb.hold(KEY_BACKSPACE, bs_ms);
    std::thread::sleep(Duration::from_millis(150));
    // Save.
    assert_kate_focused(&log, &file_name);
    kb.ctrl(KEY_S);
    std::thread::sleep(Duration::from_millis(1500));
    let actual = std::fs::read_to_string(&target).unwrap();
    drop(kate);
    drop(kb);
    drop(brain); // SIGTERM: final ingest, sessions closed
    assert_no_brain_on(&data);

    let conn = rusqlite::Connection::open(data.join("brain.db")).unwrap();
    let reconstructed: String = conn
        .query_row(
            "SELECT reconstructed_text FROM sessions WHERE app LIKE '%kate%' ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or_default();

    // Per-hold counts: what Kate got vs brain's log vs the model. Characters
    // deleted by the Backspace hold are counted from the log only.
    let mut logged = std::collections::HashMap::<String, usize>::new();
    for l in std::fs::read_to_string(&log).unwrap().lines() {
        let v: serde_json::Value = serde_json::from_str(l).unwrap();
        if v["type"] == "key" && v["app"].as_str().unwrap_or("").contains("kate") {
            *logged
                .entry(v["key"].as_str().unwrap().to_string())
                .or_default() += 1;
        }
    }
    eprintln!("hold   ms   model  brain-log  kate-file  reconstructed");
    let mut log_mismatch = Vec::new();
    let mut text_mismatch = Vec::new();
    for &(c, ms, boundary) in holds {
        let k = actual.chars().filter(|&x| x == c).count();
        let r = reconstructed.chars().filter(|&x| x == c).count();
        let b = logged.get(&c.to_string()).copied().unwrap_or(0);
        eprintln!(
            "{c}   {ms:5}   {:5}  {b:9}  {k:9}  {r:13}{}",
            model_count(ms),
            if boundary { "  (boundary)" } else { "" }
        );
        if (boundary && b.abs_diff(model_count(ms)) > 1) || (!boundary && b != model_count(ms)) {
            log_mismatch.push(c);
        }
        // The run the Backspace hold deleted into also absorbs its ±1.
        let tolerance = if ms > 2_000 { 2 } else { 1 } + usize::from(c == 'k');
        if k.abs_diff(r) > tolerance {
            text_mismatch.push(c);
        }
    }
    let bs = logged.get("Backspace").copied().unwrap_or(0);
    eprintln!("⌫   {bs_ms:5}   {:5}  {bs:9}", model_count(bs_ms));
    eprintln!("kate file:     {actual:?}");
    eprintln!("reconstructed: {reconstructed:?}");

    assert!(
        log_mismatch.is_empty(),
        "brain logged a different number of repeats than KDE's schedule for {log_mismatch:?}"
    );
    assert_eq!(bs, model_count(bs_ms), "Backspace repeats");
    assert!(
        text_mismatch.is_empty(),
        "reconstruction is off by more than the app's timer slack for {text_mismatch:?}"
    );
    // Same letters in the same order: only run lengths may differ.
    let runs = |s: &str| {
        let mut v: Vec<char> = s.chars().filter(|c| *c != '\n').collect();
        v.dedup();
        v
    };
    assert_eq!(runs(&reconstructed), runs(&actual), "letter order differs");
}
