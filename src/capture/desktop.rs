//! Desktop integration over D-Bus (KDE Plasma): focus, clipboard, lock, sleep, layouts.
//!
//! | What                  | Source                                                     |
//! |-----------------------|------------------------------------------------------------|
//! | active app + window   | KWin script → our `Activated` method (session bus)         |
//! | clipboard at paste    | `org.kde.klipper` `getClipboardContents` (session bus)     |
//! | screen lock           | `org.freedesktop.ScreenSaver` `ActiveChanged` (session bus)|
//! | layout switch/list    | `org.kde.KeyboardLayouts` signals (session bus)            |
//! | sleep / wake          | logind `PrepareForSleep` (system bus)                      |
//! | layout spec           | kxkbrc → `XKB_DEFAULT_*` env → systemd-localed             |
//!
//! Everything here is best effort. Without a session bus or KWin, capture still
//! runs and attributes keys to the `unknown` app (with a warning on stderr).

use super::input::{Control, ControlHandle};
use super::recorder::{ClipboardSource, Recorder};
use super::xkb::{Keyboard, LayoutSpec};
use crate::event::{SystemState, UNKNOWN_APP};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Header;
use zbus::{MatchRule, message::Type as MsgType};

const OBJECT_PATH: &str = "/org/shadowassistant/Brain";
const FOCUS_IFACE: &str = "org.shadowassistant.Brain.Focus";
const KWIN: &str = "org.kde.KWin";
const SCRIPT_TEMPLATE: &str = include_str!("kwin_focus.js");

/// The app id for a window: its desktop file name (`code`, `google-chrome`,
/// `org.kde.konsole`), else its window class, else `unknown`.
pub fn app_id(desktop_file: &str, resource_class: &str) -> String {
    let d = desktop_file.trim();
    let r = resource_class.trim();
    if !d.is_empty() {
        d.strip_suffix(".desktop").unwrap_or(d).to_string()
    } else if !r.is_empty() {
        r.to_string()
    } else {
        UNKNOWN_APP.to_string()
    }
}

// ---------------------------------------------------------------------------
// Focus service + KWin script
// ---------------------------------------------------------------------------

struct FocusIface {
    recorder: Arc<Recorder>,
    /// Unique bus name of the running KWin; calls from anyone else are ignored.
    kwin_owner: Arc<Mutex<Option<String>>>,
}

#[zbus::interface(name = "org.shadowassistant.Brain.Focus")]
impl FocusIface {
    fn activated(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        desktop_file: String,
        resource_class: String,
        caption: String,
    ) {
        let expected = self
            .kwin_owner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let sender = hdr.sender().map(|s| s.as_str().to_string());
        if expected.is_none() || sender != expected {
            return; // not from KWin: ignore spoofed focus reports
        }
        self.recorder
            .set_focus(&app_id(&desktop_file, &resource_class), &caption);
    }
}

/// The KWin focus-tracking script, registered for as long as this lives.
pub struct KwinFocus {
    conn: Connection,
    plugin: String,
    script_path: PathBuf,
    kwin_owner: Arc<Mutex<Option<String>>>,
}

impl KwinFocus {
    /// Exposes the focus service on the session bus and loads the KWin script.
    pub fn start(recorder: Arc<Recorder>, kwin_dir: &Path) -> Result<Self> {
        let service = format!("org.shadowassistant.Brain.p{}", std::process::id());
        let kwin_owner = Arc::new(Mutex::new(None));
        let iface = FocusIface {
            recorder,
            kwin_owner: kwin_owner.clone(),
        };
        let conn = zbus::blocking::connection::Builder::session()?
            .name(service.as_str())?
            .serve_at(OBJECT_PATH, iface)?
            .method_timeout(Duration::from_secs(5))
            .build()
            .context("connecting to the session bus")?;

        let script = SCRIPT_TEMPLATE
            .replace("@SERVICE@", &service)
            .replace("@PATH@", OBJECT_PATH)
            .replace("@IFACE@", FOCUS_IFACE);
        let script_path = kwin_dir.join("brain_focus.js");
        std::fs::write(&script_path, script)
            .with_context(|| format!("writing {}", script_path.display()))?;

        // A previous `brain run` killed with -9 could not unload its script.
        let marker = kwin_dir.join("loaded_plugin");
        if let Ok(stale) = std::fs::read_to_string(&marker) {
            let _ = unload_script(&conn, stale.trim());
        }
        let plugin = format!("brain_focus_{}", std::process::id());
        std::fs::write(&marker, &plugin)?;

        let me = Self {
            conn,
            plugin,
            script_path,
            kwin_owner,
        };
        me.load()?;
        Ok(me)
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// (Re)loads the script into the currently running KWin.
    pub fn load(&self) -> Result<()> {
        let owner =
            name_owner(&self.conn, KWIN).context("KWin is not running on the session bus")?;
        *self.kwin_owner.lock().unwrap_or_else(|p| p.into_inner()) = Some(owner);
        let _ = unload_script(&self.conn, &self.plugin);
        let path = self.script_path.to_string_lossy().into_owned();
        let id: i32 = self
            .conn
            .call_method(
                Some(KWIN),
                "/Scripting",
                Some("org.kde.kwin.Scripting"),
                "loadScript",
                &(path.as_str(), self.plugin.as_str()),
            )?
            .body()
            .deserialize()?;
        if id < 0 {
            bail!("KWin refused to load {}", self.script_path.display());
        }
        self.conn.call_method(
            Some(KWIN),
            format!("/Scripting/Script{id}").as_str(),
            Some("org.kde.kwin.Script"),
            "run",
            &(),
        )?;
        Ok(())
    }

    /// Re-registers the script whenever KWin restarts (it loses all scripts).
    pub fn watch_kwin_restarts(self: &Arc<Self>) {
        let me = self.clone();
        spawn_signal_watch(
            &self.conn,
            "org.freedesktop.DBus",
            "org.freedesktop.DBus",
            "NameOwnerChanged",
            Some(KWIN),
            move |msg| {
                let Ok((name, _old, new)) = msg.body().deserialize::<(String, String, String)>()
                else {
                    return;
                };
                if name == KWIN && !new.is_empty() {
                    // Give the new KWin a moment to export /Scripting.
                    std::thread::sleep(Duration::from_millis(500));
                    if let Err(e) = me.load() {
                        eprintln!(
                            "brain: warning: could not re-register with restarted KWin: {e:#}"
                        );
                    }
                }
            },
        );
    }

    pub fn unload(&self) {
        let _ = unload_script(&self.conn, &self.plugin);
        let _ = std::fs::remove_file(self.script_path.with_file_name("loaded_plugin"));
    }
}

fn unload_script(conn: &Connection, plugin: &str) -> Result<()> {
    conn.call_method(
        Some(KWIN),
        "/Scripting",
        Some("org.kde.kwin.Scripting"),
        "unloadScript",
        &(plugin,),
    )?;
    Ok(())
}

fn name_owner(conn: &Connection, name: &str) -> Result<String> {
    let owner: String = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetNameOwner",
            &(name,),
        )?
        .body()
        .deserialize()?;
    Ok(owner)
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/// Reads the clipboard from KDE's Klipper. Uses its own connection with a short
/// timeout, because it is called on the input thread.
pub struct KlipperClipboard {
    conn: Connection,
}

impl KlipperClipboard {
    pub fn connect() -> Result<Self> {
        let conn = zbus::blocking::connection::Builder::session()?
            .method_timeout(Duration::from_millis(300))
            .build()?;
        Ok(Self { conn })
    }
}

impl ClipboardSource for KlipperClipboard {
    fn read(&self) -> Option<String> {
        let reply = self
            .conn
            .call_method(
                Some("org.kde.klipper"),
                "/klipper",
                Some("org.kde.klipper.klipper"),
                "getClipboardContents",
                &(),
            )
            .ok()?;
        reply.body().deserialize::<String>().ok()
    }
}

// ---------------------------------------------------------------------------
// Lock, sleep and layout signals
// ---------------------------------------------------------------------------

/// Spawns a thread that calls `handler` for each matching signal sent by the
/// current owner of the well-known name `sender`. Signals from anyone else
/// on the bus are dropped. `arg0` restricts on the first string argument.
fn spawn_signal_watch(
    conn: &Connection,
    sender: &'static str,
    interface: &'static str,
    member: &'static str,
    arg0: Option<&'static str>,
    mut handler: impl FnMut(&zbus::Message) + Send + 'static,
) {
    let conn = conn.clone();
    let spawn = std::thread::Builder::new()
        .name(format!("dbus-{member}"))
        .spawn(move || {
            let mut run = || -> Result<()> {
                let mut rule = MatchRule::builder()
                    .msg_type(MsgType::Signal)
                    .interface(interface)?
                    .member(member)?;
                if let Some(a) = arg0 {
                    rule = rule.add_arg(a)?;
                }
                let iter = MessageIterator::for_match_rule(rule.build(), &conn, Some(64))?;
                let mut owner = name_owner(&conn, sender).ok();
                for msg in iter {
                    let msg = msg?;
                    let from = msg.header().sender().map(|s| s.as_str().to_string());
                    if from.is_none() {
                        continue;
                    }
                    if from != owner {
                        // The service may have restarted under a new unique name.
                        owner = name_owner(&conn, sender).ok();
                        if from != owner {
                            continue; // spoofed: not sent by the service we trust
                        }
                    }
                    handler(&msg);
                }
                Ok(())
            };
            if let Err(e) = run() {
                eprintln!("brain: warning: stopped listening for {interface}.{member}: {e:#}");
            }
        });
    if let Err(e) = spawn {
        eprintln!("brain: warning: could not start D-Bus watcher thread: {e}");
    }
}

/// Pauses capture while the screen is locked, so the unlock password is never
/// recorded (under whatever app had focus before the lock).
pub fn watch_screen_lock(conn: &Connection, recorder: Arc<Recorder>) {
    let active: Option<bool> = conn
        .call_method(
            Some("org.freedesktop.ScreenSaver"),
            "/ScreenSaver",
            Some("org.freedesktop.ScreenSaver"),
            "GetActive",
            &(),
        )
        .ok()
        .and_then(|m| m.body().deserialize().ok());
    if active == Some(true) {
        recorder.set_paused(true);
    }
    spawn_signal_watch(
        conn,
        "org.freedesktop.ScreenSaver",
        "org.freedesktop.ScreenSaver",
        "ActiveChanged",
        None,
        move |msg| {
            if let Ok(locked) = msg.body().deserialize::<bool>() {
                recorder.set_paused(locked);
            }
        },
    );
}

/// Follows KDE's layout switcher and layout-list changes.
pub fn watch_layouts(conn: &Connection, system: Option<Connection>, control: ControlHandle) {
    if let Some(i) = current_layout_index(conn) {
        control.send(Control::SetLayout(i));
    }
    let c = control.clone();
    spawn_signal_watch(
        conn,
        "org.kde.keyboard",
        "org.kde.KeyboardLayouts",
        "layoutChanged",
        None,
        move |msg| {
            if let Ok(i) = msg.body().deserialize::<u32>() {
                eprintln!("brain: keyboard layout switched to #{i}");
                c.send(Control::SetLayout(i));
            }
        },
    );
    let session = conn.clone();
    spawn_signal_watch(
        conn,
        "org.kde.keyboard",
        "org.kde.KeyboardLayouts",
        "layoutListChanged",
        None,
        move |_| {
            let spec = resolve_layout(system.as_ref());
            eprintln!("brain: keyboard layout list changed, now {}", spec.layout);
            match Keyboard::new(&spec) {
                Ok(kb) => {
                    control.send(Control::SetKeyboard(Box::new(kb)));
                    if let Some(i) = current_layout_index(&session) {
                        control.send(Control::SetLayout(i));
                    }
                }
                Err(e) => eprintln!(
                    "brain: warning: new keyboard layout {spec:?} failed to compile: {e:#}"
                ),
            }
        },
    );
}

fn current_layout_index(conn: &Connection) -> Option<u32> {
    conn.call_method(
        Some("org.kde.keyboard"),
        "/Layouts",
        Some("org.kde.KeyboardLayouts"),
        "getLayout",
        &(),
    )
    .ok()?
    .body()
    .deserialize()
    .ok()
}

/// Marks sleep/wake in the log and re-opens input devices on resume (their
/// file descriptors can go stale across suspend).
pub fn watch_sleep(system: &Connection, recorder: Arc<Recorder>, control: ControlHandle) {
    spawn_signal_watch(
        system,
        "org.freedesktop.login1",
        "org.freedesktop.login1.Manager",
        "PrepareForSleep",
        None,
        move |msg| match msg.body().deserialize::<bool>() {
            Ok(true) => recorder.system(SystemState::Sleep),
            Ok(false) => {
                recorder.system(SystemState::Wake);
                control.send(Control::Reopen);
            }
            Err(_) => {}
        },
    );
}

// ---------------------------------------------------------------------------
// Keyboard layout and repeat configuration
// ---------------------------------------------------------------------------

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

pub fn read_kde_config(name: &str) -> Option<String> {
    std::fs::read_to_string(config_dir()?.join(name)).ok()
}

/// The XKB layout spec the compositor uses, in KWin's order of precedence.
pub fn resolve_layout(system: Option<&Connection>) -> LayoutSpec {
    if let Some(spec) = read_kde_config("kxkbrc").and_then(|t| layout_from_kxkbrc(&t)) {
        return spec;
    }
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let from_env = LayoutSpec {
        layout: env("XKB_DEFAULT_LAYOUT"),
        variant: env("XKB_DEFAULT_VARIANT"),
        model: env("XKB_DEFAULT_MODEL"),
        options: env("XKB_DEFAULT_OPTIONS"),
    };
    if !from_env.layout.is_empty() {
        return from_env;
    }
    if let Some(spec) = system.and_then(layout_from_localed) {
        return spec;
    }
    LayoutSpec::default()
}

/// Parses `[Layout]` from KDE's kxkbrc. Only used when `Use=true`.
pub fn layout_from_kxkbrc(text: &str) -> Option<LayoutSpec> {
    let mut in_layout = false;
    let mut use_it = false;
    let mut spec = LayoutSpec::default();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_layout = line == "[Layout]";
            continue;
        }
        if !in_layout {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim().to_string();
        match k.trim() {
            "Use" => use_it = v == "true",
            "LayoutList" => spec.layout = v,
            "VariantList" => spec.variant = v,
            "Model" => spec.model = v,
            "Options" => spec.options = v,
            _ => {}
        }
    }
    (use_it && !spec.layout.is_empty()).then_some(spec)
}

fn layout_from_localed(system: &Connection) -> Option<LayoutSpec> {
    let get = |prop: &str| -> Option<String> {
        let reply = system
            .call_method(
                Some("org.freedesktop.locale1"),
                "/org/freedesktop/locale1",
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.freedesktop.locale1", prop),
            )
            .ok()?;
        let v: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
        String::try_from(v).ok()
    };
    let spec = LayoutSpec {
        layout: get("X11Layout")?,
        variant: get("X11Variant").unwrap_or_default(),
        model: get("X11Model").unwrap_or_default(),
        options: get("X11Options").unwrap_or_default(),
    };
    (!spec.layout.is_empty()).then_some(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_precedence() {
        assert_eq!(app_id("code", "Code"), "code");
        assert_eq!(
            app_id("org.kde.konsole.desktop", "konsole"),
            "org.kde.konsole"
        );
        assert_eq!(app_id("", "kwin_wayland"), "kwin_wayland");
        assert_eq!(app_id(" ", ""), UNKNOWN_APP);
    }

    #[test]
    fn kxkbrc_parsing() {
        let t = "[Layout]\nDisplayNames=,\nLayoutList=us,il\nModel=pc105\nOptions=grp:alt_shift_toggle\nUse=true\nVariantList=,\n";
        let s = layout_from_kxkbrc(t).unwrap();
        assert_eq!(s.layout, "us,il");
        assert_eq!(s.variant, ",");
        assert_eq!(s.options, "grp:alt_shift_toggle");
        assert!(layout_from_kxkbrc("[Layout]\nLayoutList=us\nUse=false\n").is_none());
    }

    #[test]
    fn script_template_has_all_placeholders() {
        for p in ["@SERVICE@", "@PATH@", "@IFACE@"] {
            assert!(SCRIPT_TEMPLATE.contains(p), "{p}");
        }
    }
}
