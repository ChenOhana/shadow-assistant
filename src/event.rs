//! The capture event format: one JSON object per line of the capture log.
//!
//! ```json
//! {"ts":"2026-11-08T14:03:22.118Z","app":"code","type":"key","key":"H","mods":["shift"],"window":"main.rs - brain - Visual Studio Code"}
//! {"ts":"2026-11-08T14:03:25.400Z","app":"org.kde.konsole","type":"focus","window":"~ : bash — Konsole"}
//! ```
//!
//! Every event carries the app it belongs to. Events that relate to a text
//! context (keys, pastes) also carry the window title, so each line is
//! self-describing: a reader can start at any line (for example after a
//! checkpoint seek) without replaying earlier focus events.

use crate::timefmt::{self, Millis};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// App id used when no window is focused or the focus source is unavailable.
pub const UNKNOWN_APP: &str = "unknown";

/// One line of the capture log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// When it happened, epoch ms (RFC 3339 UTC in the log); for a key, the
    /// kernel's timestamp of the key event.
    #[serde(serialize_with = "ser_ts", deserialize_with = "de_ts")]
    pub ts: Millis,
    /// The focused app's id (`org.kde.konsole`), or `unknown`.
    pub app: String,
    /// What happened.
    #[serde(flatten)]
    pub kind: EventKind,
}

/// The kinds of events, tagged by `type` in the JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum EventKind {
    /// A key press, or a key auto-repeat (`repeat: true`), as delivered to the app.
    Key {
        /// The resolved character (`"H"`, `"é"`, `" "`) or, for non-printing keys, a
        /// name (`"Enter"`, `"Backspace"`, `"Left"`, `"F5"`).
        key: String,
        /// Modifiers held, in canonical order: shift, ctrl, alt, super, altgr.
        mods: Vec<Mod>,
        /// An auto-repeat synthesized on the desktop's schedule, not a new press.
        #[serde(default, skip_serializing_if = "is_false")]
        repeat: bool,
        /// The focused window's title when the key was pressed.
        window: String,
    },
    /// A window was activated, or (`renamed: true`) the focused window's
    /// title changed.
    Focus {
        /// The window's title.
        window: String,
        /// The same window's title changed (not another window activated).
        #[serde(default, skip_serializing_if = "is_false")]
        renamed: bool,
    },
    /// Clipboard contents at the moment a paste shortcut was pressed. Written
    /// right after the key event that triggered it. `None` when the clipboard
    /// could not be read.
    Paste {
        /// The clipboard text (up to 64 KiB), or `None` if it could not be read.
        text: Option<String>,
        /// The focused window's title.
        window: String,
    },
    /// Lifecycle markers: start, stop, sleep, wake, lock, unlock.
    System {
        /// Which marker.
        state: SystemState,
    },
}

/// A modifier key held with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mod {
    /// Shift.
    Shift,
    /// Control.
    Ctrl,
    /// Alt.
    Alt,
    /// Super (the Windows/Meta key).
    Super,
    /// AltGr (ISO level 3).
    Altgr,
}

/// A lifecycle marker in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemState {
    /// `brain run` started (without a `stop` before it, it crashed).
    Start,
    /// `brain run` stopped cleanly.
    Stop,
    /// The machine is going to sleep.
    Sleep,
    /// The machine woke up (keyboards were reopened).
    Wake,
    /// The screen locked: nothing is recorded until `unlock`.
    Lock,
    /// The screen unlocked.
    Unlock,
}

impl Event {
    /// The window title this event belongs to, if it relates to a text context.
    pub fn window(&self) -> Option<&str> {
        match &self.kind {
            EventKind::Key { window, .. }
            | EventKind::Focus { window, .. }
            | EventKind::Paste { window, .. } => Some(window),
            EventKind::System { .. } => None,
        }
    }

    /// Serializes as one log line, including the trailing newline.
    pub fn to_line(&self, buf: &mut Vec<u8>) {
        buf.clear();
        serde_json::to_writer(&mut *buf, self).expect("serializing an Event cannot fail");
        buf.push(b'\n');
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn ser_ts<S: Serializer>(ms: &Millis, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&timefmt::format_ms(*ms))
}

fn de_ts<'de, D: Deserializer<'de>>(d: D) -> Result<Millis, D::Error> {
    let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
    timefmt::parse_rfc3339_ms(&s).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_event_matches_assignment_format() {
        let e = Event {
            ts: 1_794_146_602_118,
            app: "com.tinyspeck.slackmacgap".into(),
            kind: EventKind::Key {
                key: "H".into(),
                mods: vec![Mod::Shift],
                repeat: false,
                window: "general".into(),
            },
        };
        let mut buf = Vec::new();
        e.to_line(&mut buf);
        let line = String::from_utf8(buf).unwrap();
        assert_eq!(
            line,
            "{\"ts\":\"2026-11-08T14:03:22.118Z\",\"app\":\"com.tinyspeck.slackmacgap\",\
             \"type\":\"key\",\"key\":\"H\",\"mods\":[\"shift\"],\"window\":\"general\"}\n"
        );
        let back: Event = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn all_kinds_round_trip() {
        let kinds = vec![
            EventKind::Key {
                key: "Backspace".into(),
                mods: vec![],
                repeat: true,
                window: "w".into(),
            },
            EventKind::Focus {
                window: "w".into(),
                renamed: false,
            },
            EventKind::Focus {
                window: "w2".into(),
                renamed: true,
            },
            EventKind::Paste {
                text: Some("x\ny".into()),
                window: "w".into(),
            },
            EventKind::Paste {
                text: None,
                window: "w".into(),
            },
            EventKind::System {
                state: SystemState::Wake,
            },
        ];
        for kind in kinds {
            let e = Event {
                ts: 5,
                app: "a".into(),
                kind,
            };
            let mut buf = Vec::new();
            e.to_line(&mut buf);
            let back: Event = serde_json::from_slice(&buf).unwrap();
            assert_eq!(back, e);
        }
    }
}
