//! Shared helpers for the integration tests: a guard for every `brain run`
//! they start, a check of the index against a rebuild from the log, and for
//! the tests that drive a real KDE session, a uinput virtual keyboard and
//! checks of what `brain` saw focused. Each test binary uses a subset, hence
//! the `dead_code` allowance.
#![allow(dead_code)]

use brain::capture::linux::synthetic::us_keycode;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_S: u16 = 31;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;

/// A uinput virtual keyboard, destroyed on drop.
pub struct VirtualKeyboard {
    file: std::fs::File,
}

impl VirtualKeyboard {
    pub fn new() -> Self {
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
            .expect("open /dev/uinput (run brain/scripts/setup-test-permissions.sh)");
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

    pub fn key(&mut self, code: u16, down: bool) {
        self.emit(EV_KEY, code, down as i32);
        self.emit(EV_SYN, 0, 0);
    }

    pub fn hold(&mut self, code: u16, ms: u64) {
        self.key(code, true);
        std::thread::sleep(Duration::from_millis(ms));
        self.key(code, false);
    }

    /// Types `s` with the US layout (Shift for capitals), one key per 30 ms.
    pub fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            let (code, shift) = us_keycode(c).unwrap_or_else(|| panic!("no key for {c:?}"));
            if shift {
                self.key(KEY_LEFTSHIFT, true);
            }
            self.key(code, true);
            self.key(code, false);
            if shift {
                self.key(KEY_LEFTSHIFT, false);
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// Ctrl+`code`.
    pub fn ctrl(&mut self, code: u16) {
        self.key(KEY_LEFTCTRL, true);
        self.key(code, true);
        self.key(code, false);
        self.key(KEY_LEFTCTRL, false);
    }
}

impl Drop for VirtualKeyboard {
    fn drop(&mut self) {
        const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
        // SAFETY: valid uinput fd.
        unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY as _) };
    }
}

/// A child process that is stopped when this goes out of scope, also when
/// the test fails partway: no test may leave a `brain run` behind. Declare it
/// after the test's temp directory, so the process stops before its data is
/// deleted.
pub struct Kill(pub Child);

impl Kill {
    /// Stops the process: SIGTERM (a clean stop), then SIGKILL if it has not
    /// exited after 10 s. Returns its exit status. Never signals a process
    /// that has already been reaped (its pid may belong to another by then).
    pub fn stop(&mut self) -> std::process::ExitStatus {
        if let Ok(Some(status)) = self.0.try_wait() {
            return status;
        }
        // SAFETY: SIGTERM to our own child, which try_wait says is not reaped.
        unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.0.try_wait() {
                return status;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill(); // SIGKILL
        self.0.wait().expect("reaping the child")
    }
}

impl Drop for Kill {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts `brain` with `args` (output discarded), guarded by [`Kill`].
pub fn spawn_brain(args: &[&str], data: &Path) -> Kill {
    Kill(
        Command::new(env!("CARGO_BIN_EXE_brain"))
            .args(args)
            .arg("--data-dir")
            .arg(data)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

/// Panics if any process still runs `brain` on the data directory `data`.
pub fn assert_no_brain_on(data: &Path) {
    let needle = data.to_string_lossy().into_owned();
    let left: Vec<String> = std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.parse::<u32>().is_ok())
        })
        .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
        .map(|c| String::from_utf8_lossy(&c).replace('\0', " "))
        .filter(|c| c.contains("brain") && c.contains(&needle))
        .collect();
    assert!(left.is_empty(), "brain still running on {needle}: {left:?}");
}

/// Closes every process whose command line contains `needle` (Kate
/// detaches from the process we start, so it is found by its unique file).
pub struct KillMatching(pub String);
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
pub fn focused(log: &Path) -> Option<(String, String)> {
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

pub fn kate_focused(log: &Path, file_name: &str) -> bool {
    focused(log).is_some_and(|(app, title)| app.contains("kate") && title.contains(file_name))
}

/// Starts `brain run` on `data`, guarded by [`Kill`].
pub fn start_brain(data: &Path) -> Kill {
    spawn_brain(&["run"], data)
}

/// Opens `file` in a new Kate window; closed (by file name) when dropped.
pub fn open_kate(file: &Path) -> KillMatching {
    let guard = KillMatching(file.to_string_lossy().into_owned());
    // Kate forks the window process and the launcher exits at once.
    let _ = Command::new("kate")
        .args(["-n"])
        .arg(file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    guard
}

/// Waits until brain's log shows Kate focused on `file_name`.
pub fn wait_kate_focused(log: &Path, file_name: &str) {
    let t = Instant::now();
    while !kate_focused(log, file_name) {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "Kate ({file_name}) never got focus (last: {:?}); nothing was typed",
            focused(log)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Panics (stopping the test before it types anything else) unless brain's
/// log shows Kate focused on `file_name`.
pub fn assert_kate_focused(log: &Path, file_name: &str) {
    assert!(
        kate_focused(log, file_name),
        "focus left Kate ({file_name}): {:?}; stopped typing",
        focused(log)
    );
}

fn qdbus(args: &[&str]) -> String {
    let out = Command::new("qdbus6").args(args).output().expect("qdbus6");
    assert!(out.status.success(), "qdbus6 {args:?} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Activates the window whose title contains `needle`, through a one-shot
/// KWin script (deterministic, unlike Alt+Tab, whose target depends on
/// window history).
pub fn activate_window(needle: &str) {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("activate.js");
    std::fs::write(
        &script,
        format!(
            "for (const w of workspace.windowList()) {{ if (w.caption.indexOf({needle:?}) >= 0) {{ workspace.activeWindow = w; break; }} }}"
        ),
    )
    .unwrap();
    let name = format!("brain_test_activate_{}", std::process::id());
    let id = qdbus(&[
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting.loadScript",
        script.to_str().unwrap(),
        &name,
    ]);
    qdbus(&[
        "org.kde.KWin",
        &format!("/Scripting/Script{id}"),
        "org.kde.kwin.Script.run",
    ]);
    std::thread::sleep(Duration::from_millis(300));
    qdbus(&[
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting.unloadScript",
        &name,
    ]);
}

/// The active keyboard layout index (KDE).
pub fn layout_index() -> u32 {
    qdbus(&[
        "org.kde.keyboard",
        "/Layouts",
        "org.kde.KeyboardLayouts.getLayout",
    ])
    .parse()
    .unwrap()
}

/// Restores the keyboard layout that was active when created.
pub struct RestoreLayout(pub u32);
impl Drop for RestoreLayout {
    fn drop(&mut self) {
        if layout_index() != self.0 {
            qdbus(&[
                "org.kde.keyboard",
                "/Layouts",
                "org.kde.KeyboardLayouts.setLayout",
                &self.0.to_string(),
            ]);
        }
    }
}

/// Switches to the first keyboard layout (us) for the test, if needed; the
/// returned guard restores the layout that was active.
pub fn use_first_layout() -> RestoreLayout {
    let restore = RestoreLayout(layout_index());
    if restore.0 != 0 {
        qdbus(&[
            "org.kde.keyboard",
            "/Layouts",
            "org.kde.KeyboardLayouts.setLayout",
            "0",
        ]);
        let t = Instant::now();
        while layout_index() != 0 {
            assert!(
                t.elapsed() < Duration::from_secs(2),
                "layout did not switch to #0"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    restore
}

/// Rebuilds the index of the data directory `data` from its log alone, into
/// a new database at `scratch`, and asserts that the live index has exactly
/// the same sessions and documents. Returns the live sessions.
pub fn assert_matches_rebuild(data: &Path, scratch: &Path) -> Vec<SessionRow> {
    let log = data.join("events.jsonl");
    let mut ing =
        brain::ingest::Ingester::new(brain::store::open_writer(scratch).unwrap(), &log).unwrap();
    while ing.step().unwrap().lines > 0 {}
    let live = rusqlite::Connection::open(data.join("brain.db")).unwrap();
    let rebuilt = rusqlite::Connection::open(scratch).unwrap();
    let sessions = session_rows(&live);
    assert_eq!(
        sessions,
        session_rows(&rebuilt),
        "sessions differ from a rebuild"
    );
    assert_eq!(
        context_rows(&live),
        context_rows(&rebuilt),
        "documents differ from a rebuild"
    );
    sessions
}

/// A session row: id, app, window, started_at, status, end_reason, text,
/// start_offset, end_offset.
pub type SessionRow = (
    i64,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    i64,
    Option<i64>,
);

pub fn session_rows(c: &rusqlite::Connection) -> Vec<SessionRow> {
    let mut st = c
        .prepare(
            "SELECT id, app, window, started_at, status, end_reason, reconstructed_text,
                    start_offset, end_offset FROM sessions ORDER BY id",
        )
        .unwrap();
    st.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
        ))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

pub fn context_rows(c: &rusqlite::Connection) -> Vec<(String, String, String, i64)> {
    let mut st = c
        .prepare("SELECT app, window, text, cursor FROM context_state ORDER BY app, window")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}
