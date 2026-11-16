//! The recorder: the one place where events are stamped with focus and appended to the log.
//!
//! Shared between the input thread (keys, pastes) and the D-Bus
//! threads (focus changes, lock/sleep markers). A mutex around the log writer
//! serializes appends, so lines never interleave.

use crate::event::{Event, EventKind, Mod, SystemState, UNKNOWN_APP};
use crate::eventlog::LogWriter;
use crate::timefmt::{Millis, now_ms};
use anyhow::Result;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Upper bound on recorded clipboard text per paste, in bytes. Larger pastes
/// are truncated at a character boundary. This keeps one paste from
/// ballooning the log or the process's memory.
pub const MAX_PASTE_BYTES: usize = 64 * 1024;

/// Reads the clipboard at paste time.
pub trait ClipboardSource: Send + Sync {
    fn read(&self) -> Option<String>;
}

pub struct NoClipboard;
impl ClipboardSource for NoClipboard {
    fn read(&self) -> Option<String> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    pub app: String,
    pub window: String,
}

impl Default for Focus {
    fn default() -> Self {
        Self {
            app: UNKNOWN_APP.to_string(),
            window: String::new(),
        }
    }
}

struct Inner {
    log: LogWriter,
    focus: Focus,
    paused: bool,
}

pub struct Recorder {
    inner: Mutex<Inner>,
    clipboard: Box<dyn ClipboardSource>,
    write_errors: AtomicU64,
}

impl Recorder {
    pub fn new(log: LogWriter, clipboard: Box<dyn ClipboardSource>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                log,
                focus: Focus::default(),
                paused: false,
            }),
            clipboard,
            write_errors: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding the lock cannot leave the log half-written
        // (appends are single writes), so recovering from poisoning is safe.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn append(&self, inner: &mut Inner, event: Event) {
        if let Err(e) = inner.log.append(&event) {
            // Never take capture down over a write error (for example, the disk
            // is full). Report the first one, then every 1000th.
            let n = self.write_errors.fetch_add(1, Ordering::Relaxed);
            if n.is_multiple_of(1000) {
                eprintln!(
                    "brain: failed to write capture log ({} errors so far): {e:#}",
                    n + 1
                );
            }
        }
    }

    pub fn focus(&self) -> Focus {
        self.lock().focus.clone()
    }

    /// Records a focus change. A no-op if neither app nor window changed.
    /// A window was activated (`renamed: false`) or the active window's title
    /// changed (`renamed: true`).
    pub fn set_focus(&self, app: &str, window: &str, renamed: bool) {
        let mut g = self.lock();
        if g.focus.app == app && g.focus.window == window {
            return;
        }
        g.focus = Focus {
            app: app.to_string(),
            window: window.to_string(),
        };
        if g.paused {
            return; // recorded on unlock, when it matters
        }
        let ev = Event {
            ts: now_ms(),
            app: app.to_string(),
            kind: EventKind::Focus {
                window: window.to_string(),
                renamed,
            },
        };
        self.append(&mut g, ev);
    }

    /// Pauses or resumes recording of input (screen lock). Lifecycle markers
    /// are still written.
    pub fn set_paused(&self, paused: bool) {
        let mut g = self.lock();
        if g.paused == paused {
            return;
        }
        g.paused = paused;
        let app = g.focus.app.clone();
        let state = if paused {
            SystemState::Lock
        } else {
            SystemState::Unlock
        };
        self.append(
            &mut g,
            Event {
                ts: now_ms(),
                app: app.clone(),
                kind: EventKind::System { state },
            },
        );
        if !paused {
            // Re-state the focus so readers know the context after the gap.
            let window = g.focus.window.clone();
            self.append(
                &mut g,
                Event {
                    ts: now_ms(),
                    app,
                    kind: EventKind::Focus {
                        window,
                        renamed: false,
                    },
                },
            );
        }
    }

    pub fn is_paused(&self) -> bool {
        self.lock().paused
    }

    pub fn system(&self, state: SystemState) {
        let mut g = self.lock();
        let app = g.focus.app.clone();
        self.append(
            &mut g,
            Event {
                ts: now_ms(),
                app,
                kind: EventKind::System { state },
            },
        );
    }

    /// Records a key. If it is a paste shortcut, the clipboard is read (outside
    /// the lock) and written as a `paste` event right after it.
    pub fn key(&self, ts: Millis, key: String, mods: Vec<Mod>, repeat: bool) {
        let paste = is_paste_shortcut(&key, &mods);
        let focus = {
            let mut g = self.lock();
            if g.paused {
                return;
            }
            let Focus { app, window } = g.focus.clone();
            let ev = Event {
                ts,
                app: app.clone(),
                kind: EventKind::Key {
                    key,
                    mods,
                    repeat,
                    window: window.clone(),
                },
            };
            self.append(&mut g, ev);
            Focus { app, window }
        };
        if paste {
            let text = self
                .clipboard
                .read()
                .map(|t| truncate_utf8(t, MAX_PASTE_BYTES));
            let mut g = self.lock();
            let ev = Event {
                ts,
                app: focus.app,
                kind: EventKind::Paste {
                    text,
                    window: focus.window,
                },
            };
            self.append(&mut g, ev);
        }
    }

    /// fdatasync the log if anything was appended since the last call.
    pub fn sync(&self) -> Result<()> {
        self.lock().log.sync()
    }
}

/// Ctrl+V, Ctrl+Shift+V (terminals) and Shift+Insert.
pub fn is_paste_shortcut(key: &str, mods: &[Mod]) -> bool {
    match key {
        "v" | "V" => mods == [Mod::Ctrl] || mods == [Mod::Shift, Mod::Ctrl],
        "Insert" => mods == [Mod::Shift],
        _ => false,
    }
}

fn truncate_utf8(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eventlog::LogReader;

    struct FixedClipboard(&'static str);
    impl ClipboardSource for FixedClipboard {
        fn read(&self) -> Option<String> {
            Some(self.0.to_string())
        }
    }

    fn events(path: &std::path::Path) -> Vec<Event> {
        let mut r = LogReader::open(path, 0).unwrap();
        let mut v = Vec::new();
        while let Some(l) = r.next_line().unwrap() {
            v.push(l.event.unwrap());
        }
        v
    }

    #[test]
    fn stamps_focus_and_handles_paste_and_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("events.jsonl");
        let rec = Recorder::new(
            LogWriter::open(&p).unwrap(),
            Box::new(FixedClipboard("pasted")),
        );
        rec.set_focus("code", "main.rs", false);
        rec.set_focus("code", "main.rs", true); // same title: no event
        rec.key(10, "h".into(), vec![], false);
        rec.key(11, "v".into(), vec![Mod::Ctrl], false);
        rec.set_paused(true);
        rec.key(12, "s".into(), vec![], false); // lock-screen password: dropped
        rec.set_focus("org.kde.konsole", "bash", false); // remembered, not written
        rec.set_paused(false);
        rec.key(13, "x".into(), vec![], false);

        let evs = events(&p);
        let kinds: Vec<String> = evs
            .iter()
            .map(|e| match &e.kind {
                EventKind::Key { key, window, .. } => format!("{}:key:{key}@{window}", e.app),
                EventKind::Focus { window, .. } => format!("{}:focus@{window}", e.app),
                EventKind::Paste { text, .. } => {
                    format!("{}:paste:{}", e.app, text.as_deref().unwrap_or("-"))
                }
                EventKind::System { state } => format!("{}:{state:?}", e.app),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "code:focus@main.rs",
                "code:key:h@main.rs",
                "code:key:v@main.rs",
                "code:paste:pasted",
                "code:Lock",
                "org.kde.konsole:Unlock",
                "org.kde.konsole:focus@bash",
                "org.kde.konsole:key:x@bash",
            ]
        );
    }

    #[test]
    fn paste_shortcuts() {
        assert!(is_paste_shortcut("v", &[Mod::Ctrl]));
        assert!(is_paste_shortcut("V", &[Mod::Shift, Mod::Ctrl]));
        assert!(is_paste_shortcut("Insert", &[Mod::Shift]));
        assert!(!is_paste_shortcut("v", &[]));
        assert!(!is_paste_shortcut("v", &[Mod::Ctrl, Mod::Alt]));
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_utf8("héllo".into(), 2), "h");
        assert_eq!(truncate_utf8("abc".into(), 10), "abc");
    }
}
