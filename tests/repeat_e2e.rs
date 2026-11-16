//! Held keys, end to end, against a real application as ground truth.
//!
//! A virtual keyboard (uinput) holds keys for precise durations in a fresh
//! Kate window while `brain run` captures. Kate's saved file is what the app
//! really received, repeats included.
//!
//! What is checked:
//! * brain's log has exactly the repeats of KDE's schedule (600 ms delay,
//!   25 Hz) for every hold, boundary cases included;
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

use brain::capture::synthetic::us_keycode;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const KEY_BACKSPACE: u16 = 14;
const KEY_LEFTCTRL: u16 = 29;
const KEY_S: u16 = 31;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;

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

/// A uinput virtual keyboard, destroyed on drop.
struct VirtualKeyboard {
    file: std::fs::File,
}

impl VirtualKeyboard {
    fn new() -> Self {
        const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
        const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
        const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
        const UI_DEV_CREATE: libc::c_ulong = 0x5501;
        #[repr(C)]
        struct Setup {
            bustype: u16,
            vendor: u16,
            product: u16,
            version: u16,
            name: [u8; 80],
            ff_effects_max: u32,
        }
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .expect("open /dev/uinput (run scripts/setup-test-permissions.sh)");
        let fd = file.as_raw_fd();
        let mut name = [0u8; 80];
        name[..21].copy_from_slice(b"brain-test-keyboard\0\0");
        let setup = Setup {
            bustype: 0x03,
            vendor: 0x1234,
            product: 0x5678,
            version: 1,
            name,
            ff_effects_max: 0,
        };
        // SAFETY: ioctls on a valid uinput fd with correctly sized arguments.
        unsafe {
            assert_eq!(libc::ioctl(fd, UI_SET_EVBIT as _, EV_KEY as libc::c_int), 0);
            for code in 1..=127 {
                libc::ioctl(fd, UI_SET_KEYBIT as _, code as libc::c_int);
            }
            assert_eq!(libc::ioctl(fd, UI_DEV_SETUP as _, &setup), 0);
            assert_eq!(libc::ioctl(fd, UI_DEV_CREATE as _), 0);
        }
        Self { file }
    }

    fn emit(&mut self, kind: u16, code: u16, value: i32) {
        #[repr(C)]
        struct InputEvent {
            sec: i64,
            usec: i64,
            kind: u16,
            code: u16,
            value: i32,
        }
        let ev = InputEvent {
            sec: 0,
            usec: 0,
            kind,
            code,
            value,
        };
        // SAFETY: InputEvent is plain old data matching struct input_event.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&ev as *const InputEvent).cast::<u8>(),
                std::mem::size_of::<InputEvent>(),
            )
        };
        self.file.write_all(bytes).unwrap();
    }

    fn key(&mut self, code: u16, down: bool) {
        self.emit(EV_KEY, code, down as i32);
        self.emit(EV_SYN, 0, 0);
    }

    fn hold(&mut self, code: u16, ms: u64) {
        self.key(code, true);
        std::thread::sleep(Duration::from_millis(ms));
        self.key(code, false);
    }
}

impl Drop for VirtualKeyboard {
    fn drop(&mut self) {
        const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
        // SAFETY: valid uinput fd.
        unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY as _) };
    }
}

/// Kills a child process on drop (also on panic).
struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        // SAFETY: SIGTERM to our own child.
        unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
        let _ = self.0.wait();
    }
}

/// Closes every process whose command line contains `needle` (Kate
/// detaches from the process we start, so it is found by its unique file).
struct KillMatching(String);
impl Drop for KillMatching {
    fn drop(&mut self) {
        let me = std::process::id();
        let find = || -> Vec<i32> {
            std::fs::read_dir("/proc")
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
                .filter(|&pid| pid as u32 != me)
                .filter(|pid| {
                    std::fs::read(format!("/proc/{pid}/cmdline"))
                        .is_ok_and(|c| String::from_utf8_lossy(&c).contains(&self.0))
                })
                .collect()
        };
        for sig in [libc::SIGTERM, libc::SIGKILL] {
            for pid in find() {
                // SAFETY: signalling a process started by this test.
                unsafe { libc::kill(pid, sig) };
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// The window brain last saw focused: (app, title).
fn focused(log: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(log).ok()?;
    text.lines().rev().find_map(|l| {
        let v: serde_json::Value = serde_json::from_str(l).ok()?;
        (v["type"] == "focus").then(|| {
            (
                v["app"].as_str().unwrap_or("").to_string(),
                v["window"].as_str().unwrap_or("").to_string(),
            )
        })
    })
}

fn kate_focused(log: &Path, file_name: &str) -> bool {
    focused(log).is_some_and(|(app, title)| app.contains("kate") && title.contains(file_name))
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

    let _brain = Kill(
        Command::new(env!("CARGO_BIN_EXE_brain"))
            .args(["run", "--data-dir"])
            .arg(&data)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut kb = VirtualKeyboard::new();
    std::thread::sleep(Duration::from_millis(1500)); // brain and KWin pick the device up
    let kate = KillMatching(target.to_string_lossy().into_owned());
    // Kate forks the real window process and this launcher exits; it is
    // reaped after `kate` (the window) is closed below.
    let mut launcher = Command::new("kate")
        .args(["-n"])
        .arg(&target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let t = Instant::now();
    while !kate_focused(&log, &file_name) {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "Kate never got focus (last: {:?}); nothing was typed",
            focused(&log)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
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
        assert!(
            kate_focused(&log, &file_name),
            "focus left Kate; stopped typing"
        );
        kb.hold(us_keycode(c).unwrap().0, ms);
        std::thread::sleep(Duration::from_millis(150));
    }
    // Held Backspace deletes back into the 'k' run.
    assert!(
        kate_focused(&log, &file_name),
        "focus left Kate; stopped typing"
    );
    let bs_ms = 1020;
    kb.hold(KEY_BACKSPACE, bs_ms);
    std::thread::sleep(Duration::from_millis(150));
    // Save.
    assert!(
        kate_focused(&log, &file_name),
        "focus left Kate; stopped typing"
    );
    kb.key(KEY_LEFTCTRL, true);
    kb.key(KEY_S, true);
    kb.key(KEY_S, false);
    kb.key(KEY_LEFTCTRL, false);
    std::thread::sleep(Duration::from_millis(1500));
    let actual = std::fs::read_to_string(&target).unwrap();
    drop(kate);
    let _ = launcher.wait();
    drop(kb);
    drop(_brain); // SIGTERM: final ingest, sessions closed

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
        if b != model_count(ms) {
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
