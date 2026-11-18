//! brain on GNOME (stock Ubuntu), against a real GNOME Shell: a private,
//! headless one, started for the test in its own D-Bus session, runtime
//! directory and home, inside its own systemd scope. The desktop you are
//! using is not touched, whatever it is.
//!
//! brain's GNOME Shell extension is installed into that private home, and a
//! real `brain run` (desktop detection, GSettings, the extension's signals)
//! must follow, in its log:
//!
//! * the focus moving to a GTK window without a desktop file (its app id is
//!   the window's Wayland app id) and to one with a desktop file (the app id
//!   is the desktop file's name, as for installed apps), and a rename;
//! * a layout switch to Hebrew, which brain applies to key resolution;
//! * focus that moved while the extension was disabled, and the layout,
//!   once it is enabled again (GNOME does that around the lock screen);
//! * the screen lock, which pauses recording (from a stand-in for GNOME's
//!   lock service: the private shell has no login session, on purpose, so
//!   it cannot reach the real session's lock state);
//!
//! and the extension must hand out the clipboard. A headless shell has no
//! keyboard, so a test-only helper extension sets the clipboard and switches
//! the layout. Ignored by default; it needs gnome-shell, gjs, python3-gi and
//! a user in the `input` group (brain reads the real keyboards, but nothing
//! is typed):
//!
//!   cargo test --release --test gnome_e2e -- --ignored --nocapture

mod common;

use common::{Kill, assert_no_brain_on};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const EXTENSION: &str = "brain@shadowassistant";
const HELPER: &str = "brain-test-helper@shadowassistant";
const DISPLAY: &str = "brain-test-0";

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
    }
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let t = Instant::now();
    while !done() {
        assert!(t.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A private headless GNOME Shell with brain's extension enabled, stopped
/// (with everything it started) when dropped.
struct PrivateGnome {
    dir: tempfile::TempDir,
    unit: String,
    shell: Child,
}

impl PrivateGnome {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path();
        let home = t.join("home");
        let extensions = home.join(".local/share/gnome-shell/extensions");
        copy_dir(
            &repo("packaging/gnome-extension").join(EXTENSION),
            &extensions.join(EXTENSION),
        );
        copy_dir(
            &repo("tests/fixtures/gnome").join(HELPER),
            &extensions.join(HELPER),
        );
        // GSettings in a plain file (GSETTINGS_BACKEND=keyfile), not dconf.
        let settings = home.join(".config/glib-2.0/settings");
        std::fs::create_dir_all(&settings).unwrap();
        std::fs::write(
            settings.join("keyfile"),
            format!(
                "[org/gnome/shell]\nenabled-extensions=['{EXTENSION}', '{HELPER}']\n\n\
                 [org/gnome/desktop/input-sources]\nsources=[('xkb', 'us'), ('xkb', 'il')]\n"
            ),
        )
        .unwrap();
        let run = t.join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        // A bus that starts no services: nothing but what the test runs.
        std::fs::create_dir_all(t.join("services")).unwrap();
        std::fs::write(
            t.join("bus.conf"),
            format!(
                "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN\" \
                 \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\
                 <busconfig><type>session</type><listen>unix:path={}</listen><auth>EXTERNAL</auth>\
                 <servicedir>{}</servicedir><policy context=\"default\">\
                 <allow send_destination=\"*\" eavesdrop=\"true\"/><allow eavesdrop=\"true\"/>\
                 <allow own=\"*\"/></policy></busconfig>",
                t.join("bus").display(),
                t.join("services").display()
            ),
        )
        .unwrap();

        let unit = format!("brain-gnome-test-{}", std::process::id());
        let mut me = Self {
            unit: unit.clone(),
            shell: Command::new("true").spawn().unwrap(), // replaced below
            dir,
        };
        let log = std::fs::File::create(me.path("shell.log")).unwrap();
        me.shell.wait().unwrap();
        me.shell = Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "-q",
                "--collect",
                "--unit",
                &unit,
                "--",
            ])
            .arg("env")
            .arg("-i")
            .args(me.env().iter().map(|(k, v)| format!("{k}={v}")))
            .args([
                "sh",
                "-c",
                &format!(
                    "dbus-daemon --config-file={} --nofork & sleep 0.3; exec gnome-shell \
                     --headless --wayland --no-x11 --virtual-monitor 1280x800 \
                     --wayland-display {DISPLAY}",
                    me.path("bus.conf").display()
                ),
            ])
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        wait_until(
            "the extension in the private GNOME Shell",
            Duration::from_secs(30),
            || {
                me.call(
                    "/org/shadowassistant/GnomeShell",
                    "org.shadowassistant.GnomeShell.Current",
                    &[],
                )
                .is_ok()
            },
        );
        wait_until(
            "GNOME Shell to finish starting",
            Duration::from_secs(30),
            || {
                me.call(
                    "/org/shadowassistant/TestHelper",
                    "org.shadowassistant.TestHelper.Ready",
                    &[],
                )
                .is_ok_and(|r| r == "(true,)")
            },
        );
        me
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn bus(&self) -> String {
        format!("unix:path={}", self.path("bus").display())
    }

    /// The environment of everything that runs in this GNOME.
    fn env(&self) -> Vec<(String, String)> {
        let home = self.path("home");
        let p = |p: PathBuf| p.to_string_lossy().into_owned();
        vec![
            ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
            ("HOME".into(), p(home.clone())),
            ("XDG_RUNTIME_DIR".into(), p(self.path("run"))),
            ("XDG_CONFIG_HOME".into(), p(home.join(".config"))),
            ("XDG_DATA_HOME".into(), p(home.join(".local/share"))),
            ("XDG_CACHE_HOME".into(), p(home.join(".cache"))),
            ("XDG_CURRENT_DESKTOP".into(), "GNOME".into()),
            ("GSETTINGS_BACKEND".into(), "keyfile".into()),
            ("NO_AT_BRIDGE".into(), "1".into()),
            ("GTK_A11Y".into(), "none".into()),
            ("DBUS_SESSION_BUS_ADDRESS".into(), self.bus()),
            ("WAYLAND_DISPLAY".into(), DISPLAY.into()),
            ("QT_QPA_PLATFORM".into(), "wayland".into()),
        ]
    }

    /// A command that runs in this GNOME (its bus, display and home only).
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut c = Command::new(program);
        c.env_clear().envs(self.env());
        c
    }

    /// Calls a method of GNOME Shell's connection; `Err` with its message.
    fn call(&self, path: &str, method: &str, args: &[&str]) -> Result<String, String> {
        self.call_on("org.gnome.Shell", path, method, args)
    }

    fn call_on(
        &self,
        dest: &str,
        path: &str,
        method: &str,
        args: &[&str],
    ) -> Result<String, String> {
        let out = self
            .command("gdbus")
            .args([
                "call",
                "--session",
                "--dest",
                dest,
                "--object-path",
                path,
                "--method",
                method,
            ])
            .args(args)
            .output()
            .unwrap();
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    /// Spawns `command` (made by [`Self::command`]) as a guarded child.
    fn spawn(&self, mut command: Command, log: &str) -> Kill {
        let log = std::fs::File::create(self.path(log)).unwrap();
        Kill(
            command
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        )
    }

    /// Processes still connected to this GNOME's bus (should be none once
    /// it is stopped).
    fn processes_on_bus(bus: &str) -> Vec<String> {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().parse::<u32>().is_ok())
            .filter(|e| {
                std::fs::read(e.path().join("environ")).is_ok_and(|env| {
                    env.split(|b| *b == 0)
                        .any(|v| v == format!("DBUS_SESSION_BUS_ADDRESS={bus}").as_bytes())
                })
            })
            .map(|e| {
                let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
                String::from_utf8_lossy(&cmd).replace('\0', " ")
            })
            .collect()
    }
}

impl Drop for PrivateGnome {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // The directory is about to go: show what happened in it.
            for name in [
                "brain.log",
                "shell.log",
                "window.log",
                "app.log",
                "screensaver.log",
            ] {
                let text = std::fs::read_to_string(self.path(name)).unwrap_or_default();
                let tail: Vec<&str> = text.lines().rev().take(15).collect();
                eprintln!("--- {name} (last lines):");
                tail.iter().rev().for_each(|l| eprintln!("{l}"));
            }
            eprintln!("--- brain's focus events:");
            for e in events(&self.path("data")) {
                if e["type"] == "focus" {
                    eprintln!("{e}");
                }
            }
        }
        let _ = Command::new("systemctl")
            .args(["--user", "stop", &format!("{}.scope", self.unit)])
            .status();
        let _ = self.shell.kill();
        let _ = self.shell.wait();
    }
}

/// The events in brain's log.
fn events(data: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(data.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn has_focus(data: &Path, app: &str, window: &str, renamed: bool) -> bool {
    events(data).iter().any(|e| {
        e["type"] == "focus"
            && e["app"] == app
            && e["window"].as_str().is_some_and(|w| w.contains(window))
            && e["renamed"].as_bool().unwrap_or(false) == renamed
    })
}

#[test]
#[ignore]
fn brain_follows_a_real_gnome_shell() {
    let gnome = PrivateGnome::start();
    let bus = gnome.bus();
    let data = gnome.path("data");
    let screensaver = gnome.spawn(
        {
            let mut c = gnome.command("python3");
            c.arg(repo("tests/fixtures/gnome/screensaver.py"));
            c
        },
        "screensaver.log",
    );
    wait_until("the lock-service stand-in", Duration::from_secs(10), || {
        gnome
            .call_on(
                "org.gnome.ScreenSaver",
                "/org/gnome/ScreenSaver",
                "org.gnome.ScreenSaver.GetActive",
                &[],
            )
            .is_ok()
    });

    let mut brain = gnome.spawn(
        {
            let mut c = gnome.command(env!("CARGO_BIN_EXE_brain"));
            c.args(["run", "--data-dir"]).arg(&data);
            c
        },
        "brain.log",
    );
    let brain_log = || std::fs::read_to_string(gnome.path("brain.log")).unwrap_or_default();
    wait_until("brain to start", Duration::from_secs(10), || {
        brain_log().contains("capturing from")
    });
    assert!(
        brain_log().contains("desktop GNOME, focus tracking via the GNOME Shell extension"),
        "{}",
        brain_log()
    );

    // Focus and rename, from a GTK window without a desktop file.
    let gtk_window = |title: &str, app_id: &str, log: &str| {
        let mut c = gnome.command("gjs");
        c.args(["-m"])
            .arg(repo("tests/fixtures/gnome/window.js"))
            .args([title, "1500", app_id]);
        gnome.spawn(c, log)
    };
    let plain = gtk_window(
        "brain gnome test",
        "org.shadowassistant.TestWindow",
        "window.log",
    );
    wait_until("focus on the GTK window", Duration::from_secs(10), || {
        has_focus(
            &data,
            "org.shadowassistant.TestWindow",
            "brain gnome test",
            false,
        )
    });
    wait_until("the GTK window's rename", Duration::from_secs(10), || {
        has_focus(
            &data,
            "org.shadowassistant.TestWindow",
            "brain gnome test renamed",
            true,
        )
    });

    // Focus on an app with a desktop file: the app id is the file's name.
    let applications = gnome.path("home/.local/share/applications");
    std::fs::create_dir_all(&applications).unwrap();
    std::fs::write(
        applications.join("org.shadowassistant.TestApp.desktop"),
        "[Desktop Entry]\nType=Application\nName=brain test app\nExec=true\n",
    )
    .unwrap();
    let installed = gtk_window("brain test app", "org.shadowassistant.TestApp", "app.log");
    wait_until(
        "focus on the app with a desktop file",
        Duration::from_secs(10),
        || {
            has_focus(
                &data,
                "org.shadowassistant.TestApp",
                "brain test app",
                false,
            )
        },
    );

    // Back to the first window, as a click on it would.
    let back = events(&data).len();
    gnome
        .call(
            "/org/shadowassistant/TestHelper",
            "org.shadowassistant.TestHelper.ActivateWindow",
            &["brain gnome test renamed"],
        )
        .unwrap();
    wait_until(
        "focus back on the GTK window",
        Duration::from_secs(10),
        || {
            events(&data)[back..].iter().any(|e| {
                e["type"] == "focus"
                    && e["app"] == "org.shadowassistant.TestWindow"
                    && e["window"] == "brain gnome test renamed"
                    && e.get("renamed").is_none()
            })
        },
    );

    // Super+Space to Hebrew, done by the helper: brain switches its keymap.
    gnome
        .call(
            "/org/shadowassistant/TestHelper",
            "org.shadowassistant.TestHelper.ActivateInputSource",
            &["il"],
        )
        .unwrap();
    wait_until(
        "brain to follow the layout switch",
        Duration::from_secs(10),
        || brain_log().contains("keyboard layout switched to il (#1)"),
    );

    // GNOME disables extensions while the screen is locked and enables them
    // again after (and may load them after a brain started early at login).
    // Focus that moved meanwhile, and the layout, arrive on re-enable.
    let extensions = |method: &str| {
        gnome
            .call(
                "/org/gnome/Shell",
                &format!("org.gnome.Shell.Extensions.{method}"),
                &[EXTENSION],
            )
            .unwrap();
    };
    let switches = || {
        brain_log()
            .matches("keyboard layout switched to il")
            .count()
    };
    let before = events(&data).len();
    extensions("DisableExtension");
    gnome
        .call(
            "/org/shadowassistant/TestHelper",
            "org.shadowassistant.TestHelper.ActivateWindow",
            &["brain test app"],
        )
        .unwrap();
    extensions("EnableExtension");
    wait_until(
        "the re-enabled extension's reports",
        Duration::from_secs(10),
        || {
            switches() == 2
                && events(&data)[before..]
                    .iter()
                    .any(|e| e["type"] == "focus" && e["app"] == "org.shadowassistant.TestApp")
        },
    );

    // The extension hands out the clipboard (what brain reads at a paste).
    gnome
        .call(
            "/org/shadowassistant/TestHelper",
            "org.shadowassistant.TestHelper.SetClipboard",
            &["שלום from GNOME"],
        )
        .unwrap();
    let clip = gnome
        .call(
            "/org/shadowassistant/GnomeShell",
            "org.shadowassistant.GnomeShell.Clipboard",
            &[],
        )
        .unwrap();
    assert_eq!(clip, "('שלום from GNOME',)");

    // The screen locks: brain pauses recording.
    gnome
        .call_on(
            "org.gnome.ScreenSaver",
            "/org/gnome/ScreenSaver",
            "org.gnome.ScreenSaver.Lock",
            &[],
        )
        .unwrap();
    wait_until("brain to record the lock", Duration::from_secs(10), || {
        events(&data)
            .iter()
            .any(|e| e["type"] == "system" && e["state"] == "lock")
    });

    assert!(brain.stop().success(), "{}", brain_log());
    assert_no_brain_on(&data);
    drop((installed, plain, screensaver));
    drop(gnome);
    let left = PrivateGnome::processes_on_bus(&bus);
    assert!(left.is_empty(), "left running: {left:?}");
}
