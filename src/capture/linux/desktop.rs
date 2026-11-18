//! Desktop environments: what `brain` needs from the desktop besides the
//! keys, behind one interface per environment.
//!
//! | Backend          | Focus      | Clipboard | Screen lock                   | Keyboard settings          |
//! |------------------|------------|-----------|-------------------------------|----------------------------|
//! | KDE Plasma       | KWin script| Klipper   | `org.freedesktop.ScreenSaver` | kxkbrc, kcminputrc, layout switch signals |
//! | GNOME            | brain's GNOME Shell extension | the extension | `org.gnome.ScreenSaver` | GSettings, layout switch signals from the extension |
//! | anything else    | none (`unknown`) | none | `org.freedesktop.ScreenSaver` if the desktop has it | `XKB_DEFAULT_*`, systemd-localed |
//!
//! A new desktop is one more [`Desktop`] implementation and a line in
//! [`detect`]; the key capture does not change.

use super::input::ControlHandle;
use super::keys::RepeatConfig;
use super::xkb::LayoutSpec;
use super::{dbus, gnome, kde, layout};
use crate::capture::recorder::{ClipboardSource, NoClipboard, Recorder};
use crate::datadir::DataDir;
use crate::event::UNKNOWN_APP;
use std::path::PathBuf;
use std::sync::Arc;
use zbus::blocking::Connection;

/// A desktop environment backend.
pub trait Desktop {
    /// Its name, for messages (`KDE Plasma`).
    fn name(&self) -> String;
    /// Reads the clipboard at a paste shortcut. Called on the input thread,
    /// so it must answer quickly (or give up).
    fn clipboard(&self) -> Box<dyn ClipboardSource>;
    /// The keyboard layouts and key-repeat schedule the compositor uses, so
    /// keys resolve to what the apps receive.
    fn keyboard(&self, system: Option<&Connection>) -> (LayoutSpec, RepeatConfig);
    /// Starts reporting focus, screen lock and layout switches. What is
    /// unavailable is reported on stderr and skipped.
    fn start(&self, ctx: DesktopContext<'_>) -> Box<dyn DesktopSession>;
}

/// What a desktop backend reports into.
pub struct DesktopContext<'a> {
    /// Where focus, lock and paste events go.
    pub recorder: Arc<Recorder>,
    /// The input thread, for layout switches and keymap changes.
    pub input: ControlHandle,
    /// The system bus, if reachable.
    pub system_bus: Option<Connection>,
    /// The data directory (the KWin script is written there).
    pub data_dir: &'a DataDir,
}

/// A started desktop backend.
pub trait DesktopSession {
    /// How focus is tracked, for the start message (`via KWin`).
    fn focus_tracking(&self) -> &'static str;
    /// Undoes what the backend set up on the desktop (unloads a script).
    fn stop(&self);
}

/// Picks the backend for the running desktop: from `XDG_CURRENT_DESKTOP`
/// (set by the session, also for user services), else by which compositor
/// owns its name on the session bus.
pub fn detect() -> Box<dyn Desktop> {
    let current = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if has_token(&current, "KDE") {
        return Box::new(kde::Kde);
    }
    if has_token(&current, "GNOME") {
        return Box::new(gnome::Gnome);
    }
    if on_session_bus(kde::KWIN) {
        return Box::new(kde::Kde);
    }
    if on_session_bus(gnome::SHELL) {
        return Box::new(gnome::Gnome);
    }
    Box::new(Unsupported {
        name: if current.is_empty() {
            "unknown".into()
        } else {
            current
        },
    })
}

/// Whether the colon-separated desktop list (`ubuntu:GNOME`) names `token`.
fn has_token(list: &str, token: &str) -> bool {
    list.split(':')
        .any(|t| t.trim().eq_ignore_ascii_case(token))
}

fn on_session_bus(name: &str) -> bool {
    Connection::session().is_ok_and(|c| dbus::name_owner(&c, name).is_ok())
}

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

/// The user's configuration directory (`$XDG_CONFIG_HOME`, else `~/.config`).
pub(super) fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

/// A screen-saver service whose `ActiveChanged(bool)` signal and `GetActive`
/// method say whether the screen is locked. Its interface has its name.
pub(super) struct LockService {
    pub name: &'static str,
    pub path: &'static str,
}

/// The freedesktop interface (KDE, Xfce, ...).
pub(super) const FREEDESKTOP_LOCK: LockService = LockService {
    name: "org.freedesktop.ScreenSaver",
    path: "/ScreenSaver",
};

/// Pauses capture while the screen is locked, so the unlock password is never
/// recorded (under whatever app had focus before the lock).
pub(super) fn watch_lock(conn: &Connection, service: &LockService, recorder: Arc<Recorder>) {
    let active: Option<bool> = conn
        .call_method(
            Some(service.name),
            service.path,
            Some(service.name),
            "GetActive",
            &(),
        )
        .ok()
        .and_then(|m| m.body().deserialize().ok());
    if active == Some(true) {
        recorder.set_paused(true);
    }
    dbus::spawn_signal_watch(
        conn,
        service.name,
        service.name,
        "ActiveChanged",
        None,
        move |msg| {
            if let Ok(locked) = msg.body().deserialize::<bool>() {
                recorder.set_paused(locked);
            }
        },
    );
}

/// A desktop without a backend: keys only, filed under `unknown`.
struct Unsupported {
    name: String,
}

impl Desktop for Unsupported {
    fn name(&self) -> String {
        format!("{} (unsupported)", self.name)
    }

    fn clipboard(&self) -> Box<dyn ClipboardSource> {
        Box::new(NoClipboard)
    }

    fn keyboard(&self, system: Option<&Connection>) -> (LayoutSpec, RepeatConfig) {
        (layout::system_layout(system), RepeatConfig::default())
    }

    fn start(&self, ctx: DesktopContext<'_>) -> Box<dyn DesktopSession> {
        eprintln!(
            "brain: warning: desktop '{}' is not supported (KDE Plasma and GNOME are): keys are \
             attributed to 'unknown', pastes are not resolved, and the screen lock is \
             detected only if the desktop implements org.freedesktop.ScreenSaver, so an \
             unlock password may be recorded",
            self.name
        );
        if let Ok(session) = Connection::session() {
            watch_lock(&session, &FREEDESKTOP_LOCK, ctx.recorder);
        }
        Box::new(NoSession)
    }
}

struct NoSession;

impl DesktopSession for NoSession {
    fn focus_tracking(&self) -> &'static str {
        "unavailable"
    }

    fn stop(&self) {}
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
    fn desktop_list_tokens() {
        assert!(has_token("KDE", "KDE"));
        assert!(has_token("ubuntu:GNOME", "gnome"));
        assert!(!has_token("XFCE", "KDE"));
        assert!(!has_token("", "KDE"));
    }
}
