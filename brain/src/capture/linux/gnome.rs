//! GNOME (stock Ubuntu): focus, clipboard, screen lock and keyboard settings.
//!
//! | What                  | Source                                                       |
//! |-----------------------|--------------------------------------------------------------|
//! | active app + window   | brain's GNOME Shell extension, `Focus` signal                |
//! | clipboard at paste    | the extension's `Clipboard` method                           |
//! | screen lock           | `org.gnome.ScreenSaver` `ActiveChanged` (GNOME Shell)        |
//! | layout switch/list    | the extension's `InputSourceChanged` / `InputSourcesChanged` |
//! | layout spec           | `org.gnome.desktop.input-sources` (`sources`, `xkb-options`) |
//! | key repeat            | `org.gnome.desktop.peripherals.keyboard`                     |
//!
//! GNOME on Wayland gives programs no way to see the focused window, so
//! brain ships a GNOME Shell extension (`packaging/gnome-extension/`) that
//! reports it, the counterpart of the KWin script on KDE. The extension
//! answers on GNOME Shell's own bus connection, and brain accepts its signals
//! only from the owner of `org.gnome.Shell`, so nothing else can pose as it.
//! Without the extension (not installed, or installed but GNOME not yet
//! restarted), keys are still captured, under `unknown`, with a warning.

use super::dbus::spawn_signal_watch;
use super::desktop::{Desktop, DesktopContext, DesktopSession, LockService, app_id, watch_lock};
use super::input::{Control, ControlHandle};
use super::keys::RepeatConfig;
use super::layout::system_layout;
use super::xkb::{Keyboard, LayoutSpec};
use crate::capture::recorder::{ClipboardSource, NoClipboard};
use crate::timefmt::Millis;
use anyhow::Result;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::blocking::Connection;

pub(super) const SHELL: &str = "org.gnome.Shell";
const EXT_PATH: &str = "/org/shadowassistant/GnomeShell";
const EXT_IFACE: &str = "org.shadowassistant.GnomeShell";
const GNOME_LOCK: LockService = LockService {
    name: "org.gnome.ScreenSaver",
    path: "/org/gnome/ScreenSaver",
};
/// XKB keymaps hold at most this many layouts; GNOME (mutter) splits longer
/// input-source lists into keymaps of this many and switches between them.
const XKB_MAX_LAYOUTS: usize = 4;

/// The GNOME backend.
pub(super) struct Gnome;

impl Desktop for Gnome {
    fn name(&self) -> String {
        "GNOME".into()
    }

    fn clipboard(&self) -> Box<dyn ClipboardSource> {
        match ExtensionClipboard::connect() {
            Ok(c) => Box::new(c),
            Err(e) => {
                eprintln!("brain: warning: no session bus, pastes will not be resolved: {e:#}");
                Box::new(NoClipboard)
            }
        }
    }

    fn keyboard(&self, system: Option<&Connection>) -> (LayoutSpec, RepeatConfig) {
        let sources = input_sources();
        let spec = layouts_for(&sources, 0, &xkb_options())
            .map(|(spec, _)| spec)
            .unwrap_or_else(|| system_layout(system));
        (spec, repeat_settings())
    }

    fn start(&self, ctx: DesktopContext<'_>) -> Box<dyn DesktopSession> {
        let conn = match Connection::session() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("brain: warning: no session bus, focus tracking unavailable: {e:#}");
                return Box::new(GnomeSession { extension: false });
            }
        };
        let extension = match call::<(String, String, String)>(&conn, "Current") {
            Ok((desktop_file, class, title)) => {
                ctx.recorder
                    .set_focus(&app_id(&desktop_file, &class), &title, false);
                true
            }
            Err(_) => {
                eprintln!(
                    "brain: warning: brain's GNOME Shell extension (brain@shadowassistant) is \
                     not active (yet): until it is, keys are attributed to 'unknown', pastes \
                     are not resolved and layout switches are not followed. If it is not \
                     installed, run `make -C brain install` from the repository root and log \
                     out and back in once; brain picks \
                     it up as soon as it runs."
                );
                false
            }
        };
        let recorder = ctx.recorder.clone();
        spawn_signal_watch(&conn, SHELL, EXT_IFACE, "Focus", None, move |msg| {
            if let Ok((desktop_file, class, title, renamed)) =
                msg.body().deserialize::<(String, String, String, bool)>()
            {
                recorder.set_focus(&app_id(&desktop_file, &class), &title, renamed);
            }
        });
        watch_lock(&conn, &GNOME_LOCK, ctx.recorder.clone());
        watch_layouts(&conn, ctx.input.clone());
        Box::new(GnomeSession { extension })
    }
}

struct GnomeSession {
    extension: bool,
}

impl DesktopSession for GnomeSession {
    fn focus_tracking(&self) -> &'static str {
        if self.extension {
            "via the GNOME Shell extension"
        } else {
            "waiting for the GNOME Shell extension"
        }
    }

    fn stop(&self) {}
}

/// Calls a method of brain's extension, on GNOME Shell's connection.
fn call<T>(conn: &Connection, method: &str) -> Result<T>
where
    T: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
{
    Ok(conn
        .call_method(Some(SHELL), EXT_PATH, Some(EXT_IFACE), method, &())?
        .body()
        .deserialize()?)
}

/// Reads the clipboard through the extension. Uses its own connection with a
/// short timeout, because it is called on the input thread.
struct ExtensionClipboard {
    conn: Connection,
}

impl ExtensionClipboard {
    fn connect() -> Result<Self> {
        let conn = zbus::blocking::connection::Builder::session()?
            .method_timeout(Duration::from_millis(300))
            .build()?;
        Ok(Self { conn })
    }
}

impl ClipboardSource for ExtensionClipboard {
    fn read(&self) -> Option<String> {
        call::<String>(&self.conn, "Clipboard").ok()
    }
}

// ---------------------------------------------------------------------------
// Keyboard layouts and repeat
// ---------------------------------------------------------------------------

/// Follows GNOME's layout switcher (Super+Space) and layout-list changes,
/// which the extension reports. `sources` are the XKB input sources the
/// current keymap was built from; `chunk` which group of four is loaded.
fn watch_layouts(conn: &Connection, control: ControlHandle) {
    let state = Arc::new(Mutex::new(LayoutState {
        sources: input_sources(),
        chunk: 0,
    }));
    if let Ok(id) = call::<String>(conn, "InputSource") {
        lock(&state).switch_to(&id, &control, false);
    }
    let (s, c) = (state.clone(), control.clone());
    spawn_signal_watch(
        conn,
        SHELL,
        EXT_IFACE,
        "InputSourceChanged",
        None,
        move |msg| {
            if let Ok(id) = msg.body().deserialize::<String>() {
                lock(&s).switch_to(&id, &c, true);
            }
        },
    );
    let session = conn.clone();
    spawn_signal_watch(
        conn,
        SHELL,
        EXT_IFACE,
        "InputSourcesChanged",
        None,
        move |_| {
            let mut st = lock(&state);
            st.sources = input_sources();
            st.chunk = usize::MAX; // force a new keymap
            eprintln!(
                "brain: keyboard layout list changed, now {}",
                st.sources.join(",")
            );
            let id = call::<String>(&session, "InputSource").unwrap_or_default();
            let id = if st.sources.contains(&id) {
                id
            } else {
                st.sources.first().cloned().unwrap_or_default()
            };
            st.switch_to(&id, &control, true);
        },
    );
}

struct LayoutState {
    sources: Vec<String>,
    chunk: usize,
}

impl LayoutState {
    /// Activates the XKB source `id` ("us", "il", "us+intl"): loads the
    /// keymap holding it if another one is loaded, then selects it (saying so
    /// on stderr if `announce`). Input methods (IBus) are not XKB layouts and
    /// are ignored.
    fn switch_to(&mut self, id: &str, control: &ControlHandle, announce: bool) {
        let Some(index) = self.sources.iter().position(|s| s == id) else {
            return;
        };
        let Some((spec, group)) = layouts_for(&self.sources, index, &xkb_options()) else {
            return;
        };
        let chunk = index / XKB_MAX_LAYOUTS;
        if chunk != self.chunk {
            match Keyboard::new(&spec) {
                Ok(kb) => control.send(Control::SetKeyboard(Box::new(kb))),
                Err(e) => {
                    eprintln!("brain: warning: keyboard layout {spec:?} failed to compile: {e:#}");
                    return;
                }
            }
            self.chunk = chunk;
        }
        if announce {
            eprintln!("brain: keyboard layout switched to {id} (#{index})");
        }
        control.send(Control::SetLayout(group));
    }
}

fn lock(state: &Mutex<LayoutState>) -> std::sync::MutexGuard<'_, LayoutState> {
    state.lock().unwrap_or_else(|p| p.into_inner())
}

/// The keymap holding XKB source number `index`, as mutter builds it (the
/// sources in groups of [`XKB_MAX_LAYOUTS`], with GNOME's XKB `options`),
/// and the source's group in it. `None` when there are no XKB sources.
fn layouts_for(sources: &[String], index: usize, options: &str) -> Option<(LayoutSpec, u32)> {
    let chunk = sources
        .chunks(XKB_MAX_LAYOUTS)
        .nth(index / XKB_MAX_LAYOUTS)?;
    let (layouts, variants): (Vec<&str>, Vec<&str>) = chunk
        .iter()
        .map(|id| id.split_once('+').unwrap_or((id.as_str(), "")))
        .unzip();
    let spec = LayoutSpec {
        layout: layouts.join(","),
        variant: if variants.iter().all(|v| v.is_empty()) {
            String::new()
        } else {
            variants.join(",")
        },
        model: String::new(),
        options: options.to_string(),
    };
    Some((spec, (index % XKB_MAX_LAYOUTS) as u32))
}

/// The XKB ids of GNOME's input sources, in order (IBus input methods left out).
fn input_sources() -> Vec<String> {
    gsettings("org.gnome.desktop.input-sources", "sources")
        .map(|v| parse_sources(&v))
        .unwrap_or_default()
}

fn xkb_options() -> String {
    gsettings("org.gnome.desktop.input-sources", "xkb-options")
        .map(|v| quoted_strings(&v).join(","))
        .unwrap_or_default()
}

/// GNOME's repeat settings, as mutter advertises them to apps.
fn repeat_settings() -> RepeatConfig {
    const SCHEMA: &str = "org.gnome.desktop.peripherals.keyboard";
    let mut cfg = RepeatConfig {
        enabled: true,
        delay_ms: 500,
        rate_hz: 1000.0 / 30.0,
    };
    if let Some(v) = gsettings(SCHEMA, "repeat") {
        cfg.enabled = v.trim() == "true";
    }
    if let Some(d) = gsettings(SCHEMA, "delay").and_then(|v| parse_uint(&v)) {
        cfg.delay_ms = d as Millis;
    }
    if let Some(i) = gsettings(SCHEMA, "repeat-interval").and_then(|v| parse_uint(&v)) {
        if i > 0 {
            cfg.rate_hz = 1000.0 / i as f64;
        } else {
            cfg.enabled = false;
        }
    }
    cfg
}

/// `gsettings get schema key`, in GVariant text form.
fn gsettings(schema: &str, key: &str) -> Option<String> {
    let out = Command::new("gsettings")
        .args(["get", schema, key])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The XKB ids in `[('xkb', 'us'), ('ibus', 'mozc-jp'), ('xkb', 'il')]`.
fn parse_sources(text: &str) -> Vec<String> {
    let strings = quoted_strings(text);
    strings
        .chunks(2)
        .filter(|pair| pair.len() == 2 && pair[0] == "xkb")
        .map(|pair| pair[1].clone())
        .collect()
}

/// The single-quoted strings in a GVariant text value, in order.
fn quoted_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\'' {
            continue;
        }
        let mut s = String::new();
        loop {
            match chars.next() {
                None | Some('\'') => break,
                Some('\\') => s.extend(chars.next()),
                Some(c) => s.push(c),
            }
        }
        out.push(s);
    }
    out
}

/// `uint32 500` or `500`.
fn parse_uint(text: &str) -> Option<u64> {
    text.split_whitespace().last()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gsettings_values() {
        assert_eq!(
            parse_sources("[('xkb', 'us'), ('ibus', 'mozc-jp'), ('xkb', 'il')]\n"),
            ["us", "il"]
        );
        assert!(parse_sources("@a(ss) []\n").is_empty());
        assert_eq!(
            quoted_strings("['grp:alt_shift_toggle', 'compose:ralt']"),
            ["grp:alt_shift_toggle", "compose:ralt"]
        );
        assert!(quoted_strings("@as []").is_empty());
        assert_eq!(parse_uint("uint32 500\n"), Some(500));
        assert_eq!(parse_uint("30"), Some(30));
    }

    #[test]
    fn keymaps_follow_mutters_groups_of_four() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (spec, group) = layouts_for(&ids(&["us", "il"]), 1, "grp:alt_shift_toggle").unwrap();
        assert_eq!(
            (spec.layout.as_str(), spec.variant.as_str(), group),
            ("us,il", "", 1)
        );
        assert_eq!(spec.options, "grp:alt_shift_toggle");
        let (spec, group) = layouts_for(&ids(&["us+intl", "il"]), 0, "").unwrap();
        assert_eq!(
            (spec.layout.as_str(), spec.variant.as_str(), group),
            ("us,il", "intl,", 0)
        );
        let six = ids(&["us", "il", "de", "fr", "ru", "ua"]);
        let (spec, group) = layouts_for(&six, 4, "").unwrap();
        assert_eq!((spec.layout.as_str(), group), ("ru,ua", 0));
        assert!(layouts_for(&[], 0, "").is_none());
    }
}
