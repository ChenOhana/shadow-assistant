//! KDE Plasma: focus, clipboard, screen lock and keyboard settings over D-Bus.
//!
//! | What                  | Source                                                     |
//! |-----------------------|------------------------------------------------------------|
//! | active app + window   | KWin script → our `Activated` method (session bus)         |
//! | clipboard at paste    | `org.kde.klipper` `getClipboardContents` (session bus)     |
//! | screen lock           | `org.freedesktop.ScreenSaver` `ActiveChanged` (session bus)|
//! | layout switch/list    | `org.kde.KeyboardLayouts` signals (session bus)            |
//! | layout spec           | kxkbrc → `XKB_DEFAULT_*` env → systemd-localed             |
//! | key repeat            | kcminputrc `[Keyboard]`                                    |
//!
//! Everything here is best effort. Without a session bus or KWin, capture still
//! runs and attributes keys to the `unknown` app (with a warning on stderr).

use super::dbus::{name_owner, spawn_signal_watch};
use super::desktop::{
    Desktop, DesktopContext, DesktopSession, app_id, config_dir, watch_freedesktop_lock,
};
use super::input::{Control, ControlHandle};
use super::keys::RepeatConfig;
use super::layout::system_layout;
use super::xkb::{Keyboard, LayoutSpec};
use crate::capture::recorder::{ClipboardSource, NoClipboard, Recorder};
use crate::timefmt::Millis;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::blocking::Connection;
use zbus::message::Header;

const OBJECT_PATH: &str = "/org/shadowassistant/Brain";
const FOCUS_IFACE: &str = "org.shadowassistant.Brain.Focus";
pub(super) const KWIN: &str = "org.kde.KWin";
const SCRIPT_TEMPLATE: &str = include_str!("kwin_focus.js");

/// The KDE Plasma backend.
pub(super) struct Kde;

impl Desktop for Kde {
    fn name(&self) -> String {
        "KDE Plasma".into()
    }

    fn clipboard(&self) -> Box<dyn ClipboardSource> {
        match KlipperClipboard::connect() {
            Ok(c) => Box::new(c),
            Err(e) => {
                eprintln!("brain: warning: no session bus, pastes will not be resolved: {e:#}");
                Box::new(NoClipboard)
            }
        }
    }

    fn keyboard(&self, system: Option<&Connection>) -> (LayoutSpec, RepeatConfig) {
        let repeat = read_kde_config("kcminputrc")
            .map(|t| repeat_from_kcminputrc(&t))
            .unwrap_or_default();
        (resolve_layout(system), repeat)
    }

    fn start(&self, ctx: DesktopContext<'_>) -> Box<dyn DesktopSession> {
        let kwin = match KwinFocus::start(ctx.recorder.clone(), &ctx.data_dir.kwin_dir()) {
            Ok(k) => {
                let k = Arc::new(k);
                k.watch_kwin_restarts();
                Some(k)
            }
            Err(e) => {
                eprintln!(
                    "brain: warning: focus tracking unavailable, keys will be attributed to \
                     'unknown' (needs KDE Plasma/KWin): {e:#}"
                );
                None
            }
        };
        let session_bus = match &kwin {
            Some(k) => Some(k.connection().clone()),
            None => Connection::session().ok(),
        };
        if let Some(s) = &session_bus {
            watch_freedesktop_lock(s, ctx.recorder.clone());
            watch_layouts(s, ctx.system_bus.clone(), ctx.input.clone());
        }
        Box::new(KdeSession { kwin })
    }
}

struct KdeSession {
    kwin: Option<Arc<KwinFocus>>,
}

impl DesktopSession for KdeSession {
    fn focus_tracking(&self) -> &'static str {
        if self.kwin.is_some() {
            "via KWin"
        } else {
            "unavailable"
        }
    }

    fn stop(&self) {
        if let Some(k) = &self.kwin {
            k.unload();
        }
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
        renamed: bool,
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
            .set_focus(&app_id(&desktop_file, &resource_class), &caption, renamed);
    }
}

/// The KWin focus-tracking script, registered for as long as this lives.
struct KwinFocus {
    conn: Connection,
    plugin: String,
    script_path: PathBuf,
    kwin_owner: Arc<Mutex<Option<String>>>,
}

impl KwinFocus {
    /// Exposes the focus service on the session bus and loads the KWin script.
    fn start(recorder: Arc<Recorder>, kwin_dir: &Path) -> Result<Self> {
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
        // KWin may not be up yet (brain started as a service early in the
        // session). Keep the connection: `watch_kwin_restarts` loads the
        // script as soon as KWin's name appears on the bus.
        if let Err(e) = me.load() {
            eprintln!(
                "brain: KWin is not reachable yet ({e:#}); focus tracking starts when it appears"
            );
        }
        Ok(me)
    }

    fn connection(&self) -> &Connection {
        &self.conn
    }

    /// (Re)loads the script into the currently running KWin.
    fn load(&self) -> Result<()> {
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
    fn watch_kwin_restarts(self: &Arc<Self>) {
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

    fn unload(&self) {
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

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/// Reads the clipboard from KDE's Klipper. Uses its own connection with a short
/// timeout, because it is called on the input thread.
struct KlipperClipboard {
    conn: Connection,
}

impl KlipperClipboard {
    fn connect() -> Result<Self> {
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
// Keyboard layouts and repeat
// ---------------------------------------------------------------------------

/// Follows KDE's layout switcher and layout-list changes.
fn watch_layouts(conn: &Connection, system: Option<Connection>, control: ControlHandle) {
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

fn read_kde_config(name: &str) -> Option<String> {
    std::fs::read_to_string(config_dir()?.join(name)).ok()
}

/// The XKB layout spec KWin uses, in its order of precedence: kxkbrc (when
/// its layout list is in use), then the system's.
fn resolve_layout(system: Option<&Connection>) -> LayoutSpec {
    read_kde_config("kxkbrc")
        .and_then(|t| layout_from_kxkbrc(&t))
        .unwrap_or_else(|| system_layout(system))
}

/// Parses `[Layout]` from KDE's kxkbrc. Only used when `Use=true`.
fn layout_from_kxkbrc(text: &str) -> Option<LayoutSpec> {
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

/// Reads `[Keyboard]` from KDE's `kcminputrc` text, falling back to defaults.
fn repeat_from_kcminputrc(text: &str) -> RepeatConfig {
    let mut cfg = RepeatConfig::default();
    let mut in_keyboard = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_keyboard = line == "[Keyboard]";
            continue;
        }
        if !in_keyboard {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "KeyRepeat" => cfg.enabled = v.trim() == "repeat" || v.trim() == "true",
            "RepeatDelay" => {
                if let Ok(d) = v.trim().parse::<f64>()
                    && d >= 0.0
                {
                    cfg.delay_ms = d as Millis;
                }
            }
            "RepeatRate" => {
                if let Ok(r) = v.trim().parse::<f64>() {
                    if r > 0.0 {
                        cfg.rate_hz = r;
                    } else {
                        cfg.enabled = false;
                    }
                }
            }
            _ => {}
        }
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parses_kcminputrc() {
        let cfg = repeat_from_kcminputrc(
            "[Mouse]\nRepeatDelay=1\n[Keyboard]\nKeyRepeat=repeat\nRepeatDelay=250\nRepeatRate=30.5\n",
        );
        assert_eq!(
            cfg,
            RepeatConfig {
                enabled: true,
                delay_ms: 250,
                rate_hz: 30.5
            }
        );
        let off = repeat_from_kcminputrc("[Keyboard]\nKeyRepeat=nothing\n");
        assert!(!off.enabled);
        assert_eq!(repeat_from_kcminputrc(""), RepeatConfig::default());
    }

    #[test]
    fn script_template_has_all_placeholders() {
        for p in ["@SERVICE@", "@PATH@", "@IFACE@"] {
            assert!(SCRIPT_TEMPLATE.contains(p), "{p}");
        }
    }
}
