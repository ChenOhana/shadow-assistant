//! The real capture path end to end: a kernel input device (uinput) typing
//! into real Kate windows while `brain run` captures through evdev,
//! libxkbcommon and the KWin focus script. Kate's saved files are the ground
//! truth for what the apps received.
//!
//! Covers: Hebrew through the compositor's Alt+Shift layout switch and back,
//! focus moving between two windows and back (the first window's session
//! resumes), and a keyboard unplugged while Shift is held followed by typing
//! on a newly plugged keyboard (no stuck modifier, hotplug picked up).
//!
//! Needs a KDE Plasma session with layouts us,il and Alt+Shift switching,
//! Kate, and /dev/uinput access (`scripts/setup-test-permissions.sh`). It
//! types only while brain's log shows the intended Kate window focused, and
//! restores the keyboard layout when done. Do not touch the keyboard or
//! mouse while it runs.
//!
//!   cargo test --release --test real_keyboard_e2e -- --ignored --nocapture

mod common;

use common::*;
use std::time::Duration;

/// Hebrew (il) keys for "שלום", typed as US key positions a, k, u, o.
const SHALOM_KEYS: &str = "akuo";

fn alt_shift(kb: &mut VirtualKeyboard) {
    kb.key(KEY_LEFTALT, true);
    kb.key(KEY_LEFTSHIFT, true);
    kb.key(KEY_LEFTSHIFT, false);
    kb.key(KEY_LEFTALT, false);
    std::thread::sleep(Duration::from_millis(400));
}

fn wait_for_layout(want: u32) {
    for _ in 0..20 {
        if layout_index() == want {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the compositor did not switch to layout #{want}");
}

#[test]
#[ignore]
fn real_keyboard_layouts_focus_and_hotplug() {
    let restore = RestoreLayout(layout_index());
    if layout_index() != 0 {
        // Start on the first layout (us); the guard restores the original.
        let _ = std::process::Command::new("qdbus6")
            .args([
                "org.kde.keyboard",
                "/Layouts",
                "org.kde.KeyboardLayouts.setLayout",
                "0",
            ])
            .status();
        wait_for_layout(0);
    }
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let log = data.join("events.jsonl");
    let pid = std::process::id();
    let (name_a, name_b) = (format!("brain-a-{pid}.txt"), format!("brain-b-{pid}.txt"));
    let (file_a, file_b) = (tmp.path().join(&name_a), tmp.path().join(&name_b));
    std::fs::write(&file_a, "").unwrap();
    std::fs::write(&file_b, "").unwrap();

    let brain = start_brain(&data);
    let mut kb = VirtualKeyboard::new();
    std::thread::sleep(Duration::from_millis(1500));

    // Window A: English, Hebrew via Alt+Shift, English again.
    let kate_a = open_kate(&file_a);
    wait_kate_focused(&log, &name_a);
    std::thread::sleep(Duration::from_millis(1500));
    assert_kate_focused(&log, &name_a);
    kb.type_str("hello ");
    alt_shift(&mut kb);
    wait_for_layout(1);
    assert_kate_focused(&log, &name_a);
    kb.type_str(SHALOM_KEYS);
    alt_shift(&mut kb);
    wait_for_layout(0);
    assert_kate_focused(&log, &name_a);
    kb.type_str(" world");

    // Window B takes focus; then back to A, within the 30 s idle timeout.
    let kate_b = open_kate(&file_b);
    wait_kate_focused(&log, &name_b);
    std::thread::sleep(Duration::from_millis(1500));
    assert_kate_focused(&log, &name_b);
    kb.type_str("beta");
    activate_window(&name_a);
    wait_kate_focused(&log, &name_a);
    kb.type_str(" again");

    // Unplug the keyboard while Shift is held, plug in another one: the
    // next key must not be shifted.
    kb.key(KEY_LEFTSHIFT, true);
    drop(kb);
    std::thread::sleep(Duration::from_millis(500));
    let mut kb = VirtualKeyboard::new();
    std::thread::sleep(Duration::from_millis(1500));
    assert_kate_focused(&log, &name_a);
    kb.type_str("x");

    // Save both and compare.
    kb.ctrl(KEY_S);
    std::thread::sleep(Duration::from_millis(800));
    activate_window(&name_b);
    wait_kate_focused(&log, &name_b);
    kb.ctrl(KEY_S);
    std::thread::sleep(Duration::from_millis(800));
    let got_a = std::fs::read_to_string(&file_a).unwrap();
    let got_b = std::fs::read_to_string(&file_b).unwrap();
    drop(kate_a);
    drop(kate_b);
    drop(kb);
    drop(brain);
    drop(restore);

    let conn = rusqlite::Connection::open(data.join("brain.db")).unwrap();
    let session = |name: &str| -> Vec<String> {
        let mut st = conn
            .prepare(
                "SELECT reconstructed_text FROM sessions
                 WHERE app LIKE '%kate%' AND window LIKE ?1 ORDER BY id",
            )
            .unwrap();
        st.query_map([format!("%{name}%")], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let (sessions_a, sessions_b) = (session(&name_a), session(&name_b));
    eprintln!("kate A: {got_a:?}  brain: {sessions_a:?}");
    eprintln!("kate B: {got_b:?}  brain: {sessions_b:?}");

    let expected_a = "hello שלום world againx";
    assert_eq!(
        got_a.trim_end_matches('\n'),
        expected_a,
        "what Kate received"
    );
    assert_eq!(got_b.trim_end_matches('\n'), "beta", "what Kate received");
    // One session per window: A resumed after the visit to B.
    assert_eq!(sessions_a, vec![expected_a.to_string()]);
    assert_eq!(sessions_b, vec!["beta".to_string()]);
    // The log names every key as the app received it.
    let logged: String = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == "key" && v["window"].as_str().unwrap_or("").contains(&name_a))
        .filter_map(|v| v["key"].as_str().map(str::to_string))
        .filter(|k| k.chars().count() == 1)
        .collect();
    assert!(logged.contains("שלום"), "{logged}");
}
