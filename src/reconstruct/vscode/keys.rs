//! Turning captured events into DevTools-protocol input for VS Code.
//!
//! [`plan`] decides *what* to replay (and what to skip so the editor keeps
//! focus); [`map_key`] decides *how* each key is sent: the DOM `key`, the
//! physical `code` and the Windows virtual-key code that VS Code's keybinding
//! service resolves shortcuts from (it dispatches on `code` by default).

use super::ReplayEvent;
use crate::event::Mod;

/// CDP `Input.dispatchKeyEvent` modifier bits.
pub(crate) const MOD_ALT: u32 = 1;
pub(crate) const MOD_CTRL: u32 = 2;
pub(crate) const MOD_SHIFT: u32 = 8;

/// One key, ready for `Input.dispatchKeyEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CdpKey {
    pub key: String,
    pub code: &'static str,
    pub vk: u32,
    /// Text the key inserts; `Some` makes it a `keyDown` (with `char`
    /// generation), `None` a `rawKeyDown`.
    pub text: Option<String>,
    pub modifiers: u32,
}

/// What the replay does, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Key(CdpKey),
    /// Insert text as a paste would (`Input.insertText`).
    Insert(String),
    /// The cursor position is unknown (session start, mouse click): focus the
    /// editor and move to the end of the document, like tier 0.
    CursorToEnd,
}

#[derive(Clone, Copy, Default)]
struct Mods {
    shift: bool,
    ctrl: bool,
    alt: bool,
    sup: bool,
}

impl Mods {
    fn of(mods: &[Mod]) -> Self {
        let mut m = Mods::default();
        for x in mods {
            match x {
                Mod::Shift => m.shift = true,
                Mod::Ctrl => m.ctrl = true,
                Mod::Alt => m.alt = true,
                Mod::Super => m.sup = true,
                Mod::Altgr => {} // already folded into the resolved character
            }
        }
        m
    }
    fn bits(self) -> u32 {
        (if self.alt { MOD_ALT } else { 0 })
            | (if self.ctrl { MOD_CTRL } else { 0 })
            | (if self.shift { MOD_SHIFT } else { 0 })
    }
    fn shortcut(self) -> bool {
        self.ctrl || self.alt
    }
}

/// US-layout physical key for an ASCII character: (`code`, virtual key).
fn us_physical(c: char) -> Option<(&'static str, u32)> {
    const LETTER_CODES: [&str; 26] = [
        "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK",
        "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV",
        "KeyW", "KeyX", "KeyY", "KeyZ",
    ];
    const DIGIT_CODES: [&str; 10] = [
        "Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8",
        "Digit9",
    ];
    Some(match c {
        'a'..='z' => {
            let i = (c as u8 - b'a') as usize;
            (LETTER_CODES[i], 65 + i as u32)
        }
        'A'..='Z' => {
            let i = (c as u8 - b'A') as usize;
            (LETTER_CODES[i], 65 + i as u32)
        }
        '0'..='9' => {
            let i = (c as u8 - b'0') as usize;
            (DIGIT_CODES[i], 48 + i as u32)
        }
        ')' => ("Digit0", 48),
        '!' => ("Digit1", 49),
        '@' => ("Digit2", 50),
        '#' => ("Digit3", 51),
        '$' => ("Digit4", 52),
        '%' => ("Digit5", 53),
        '^' => ("Digit6", 54),
        '&' => ("Digit7", 55),
        '*' => ("Digit8", 56),
        '(' => ("Digit9", 57),
        ' ' => ("Space", 32),
        ';' | ':' => ("Semicolon", 186),
        '=' | '+' => ("Equal", 187),
        ',' | '<' => ("Comma", 188),
        '-' | '_' => ("Minus", 189),
        '.' | '>' => ("Period", 190),
        '/' | '?' => ("Slash", 191),
        '`' | '~' => ("Backquote", 192),
        '[' | '{' => ("BracketLeft", 219),
        '\\' | '|' => ("Backslash", 220),
        ']' | '}' => ("BracketRight", 221),
        '\'' | '"' => ("Quote", 222),
        _ => return None,
    })
}

/// Named (non-character) keys: (DOM `key`, `code`, virtual key, text).
fn named(name: &str) -> Option<(&'static str, &'static str, u32, Option<&'static str>)> {
    Some(match name {
        "Enter" => ("Enter", "Enter", 13, Some("\r")),
        "Tab" => ("Tab", "Tab", 9, None),
        "Backspace" => ("Backspace", "Backspace", 8, None),
        "Delete" => ("Delete", "Delete", 46, None),
        "Escape" => ("Escape", "Escape", 27, None),
        "Left" => ("ArrowLeft", "ArrowLeft", 37, None),
        "Right" => ("ArrowRight", "ArrowRight", 39, None),
        "Up" => ("ArrowUp", "ArrowUp", 38, None),
        "Down" => ("ArrowDown", "ArrowDown", 40, None),
        "Home" => ("Home", "Home", 36, None),
        "End" => ("End", "End", 35, None),
        "PageUp" => ("PageUp", "PageUp", 33, None),
        "PageDown" => ("PageDown", "PageDown", 34, None),
        "Insert" => ("Insert", "Insert", 45, None),
        _ => return None,
    })
}

fn function_key(name: &str) -> Option<(&'static str, u32)> {
    const F: [&str; 24] = [
        "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14",
        "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
    ];
    let n: usize = name.strip_prefix('F')?.parse().ok()?;
    (1..=24).contains(&n).then(|| (F[n - 1], 111 + n as u32))
}

/// How a captured key is sent over CDP, or `None` if it cannot be (unknown
/// keysym names, super combinations, shortcuts on keys without a known
/// physical position).
pub(crate) fn map_key(key: &str, mods: &[Mod]) -> Option<CdpKey> {
    let m = Mods::of(mods);
    if m.sup {
        return None;
    }
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.clone().next()) {
        let phys = us_physical(c);
        if m.shortcut() {
            // Shortcuts resolve on the physical key; without one we cannot
            // reproduce what VS Code would have matched.
            let (code, vk) = phys?;
            return Some(CdpKey {
                key: c.to_string(),
                code,
                vk,
                text: None,
                modifiers: m.bits(),
            });
        }
        let (code, vk) = phys.unwrap_or(("", 0));
        return Some(CdpKey {
            key: c.to_string(),
            code,
            vk,
            text: Some(c.to_string()),
            modifiers: m.bits(),
        });
    }
    if let Some((k, code, vk, text)) = named(key) {
        return Some(CdpKey {
            key: k.to_string(),
            code,
            vk,
            text: if m.shortcut() {
                None
            } else {
                text.map(str::to_string)
            },
            modifiers: m.bits(),
        });
    }
    let (code, vk) = function_key(key)?;
    Some(CdpKey {
        key: code.to_string(),
        code,
        vk,
        text: None,
        modifiers: m.bits(),
    })
}

/// Lowercased single-character key, for shortcut matching.
fn letter(key: &str) -> Option<char> {
    let mut it = key.chars();
    let c = it.next()?;
    it.next().is_none().then(|| c.to_ascii_lowercase())
}

/// Second keys of a `Ctrl+K` chord that only edit text or fold in place, so
/// replaying them keeps the editor focused: comment/uncomment (C/U), format
/// selection (F), trim whitespace (X), fold/unfold (0-9, J, L, [, ]).
fn chord_second_ok(key: &str) -> bool {
    matches!(letter(key), Some(c) if "cufxjl[]0123456789".contains(c))
}

/// Why a key is not replayed:
///
/// - **Paste shortcuts** (Ctrl+V, Ctrl+Shift+V, Shift+Insert): the capture log
///   follows each with a paste event carrying the user's real clipboard text,
///   which is what gets inserted; letting the sandbox paste would insert its
///   own clipboard instead (or as well).
/// - **Focus leaves the editor**: quick open (Ctrl+P, Ctrl+E), command palette
///   (Ctrl+Shift+P, F1), side bar views (Ctrl+Shift+E/F/G/D/X/H), panels and
///   terminals (Ctrl+J, Ctrl+`, Ctrl+Shift+`/M/U/Y), menus (Alt+F/E/S/V/G/R/T/H),
///   dialogs (Ctrl+O, Ctrl+Shift+S, Ctrl+R), settings (Ctrl+,).
/// - **Closes, opens or switches editors and windows**: Ctrl+W, Ctrl+F4, Ctrl+Q,
///   Ctrl+N, Ctrl+Shift+N/T/W, Ctrl+Tab, Ctrl+PageUp/PageDown, Ctrl+1…9, Ctrl+\,
///   Ctrl+Shift+5; Ctrl+K chords other than the in-place ones in
///   [`chord_second_ok`].
/// - **Needs what the sandbox lacks**: debugger and tasks (F5…, Ctrl+Shift+B),
///   language-server navigation and refactoring (F2, F12, Ctrl+T,
///   Ctrl+Shift+O/R/.), external terminal (Ctrl+Shift+C), sidebar toggle
///   (Ctrl+B). All function keys except F3 (find next) are skipped.
fn skipped(key: &str, m: Mods) -> bool {
    if let Some(c) = letter(key) {
        if m.ctrl && !m.alt && !m.shift {
            return "wqnoprtjebv,`\\123456789".contains(c);
        }
        if m.ctrl && m.shift && !m.alt {
            return "pnefgdxhjmuytcvbsro.`5".contains(c);
        }
        if m.alt && !m.ctrl {
            // Menu-bar mnemonics: File Edit Selection View Go Run Terminal Help.
            return "fesvgrth".contains(c);
        }
        return false;
    }
    if let Some((name, _)) = function_key(key) {
        return name != "F3"; // find next/previous only
    }
    match key {
        // Editor switching.
        "Tab" | "PageUp" | "PageDown" if m.ctrl => true,
        // Paste shortcut: the following paste event carries the text.
        "Insert" if m.shift && !m.ctrl && !m.alt => true,
        _ => false,
    }
}

/// Builds the replay: an initial cursor-to-end, then each event mapped, with
/// the skip rules above applied.
pub(crate) fn plan(events: &[ReplayEvent]) -> Vec<Action> {
    let mut out = vec![Action::CursorToEnd];
    let mut pending_chord: Option<CdpKey> = None;
    // The paste event right after Ctrl+Shift+V is dropped too: VS Code's
    // editor has no paste binding on it (it is the Markdown preview), so the
    // real editor inserted nothing. Ctrl+V and Shift+Insert do paste there.
    let mut drop_next_paste = false;
    for ev in events {
        match ev {
            ReplayEvent::Key { key, mods } => {
                let m = Mods::of(mods);
                drop_next_paste = m.ctrl && m.shift && !m.alt && letter(key) == Some('v');
                if let Some(first) = pending_chord.take() {
                    if chord_second_ok(key)
                        && let Some(k) = map_key(key, mods)
                    {
                        out.push(Action::Key(first));
                        out.push(Action::Key(k));
                    }
                    continue;
                }
                if m.ctrl && !m.alt && !m.shift && !m.sup && letter(key) == Some('k') {
                    pending_chord = map_key(key, mods);
                    continue;
                }
                if m.sup || skipped(key, m) {
                    continue;
                }
                if let Some(k) = map_key(key, mods) {
                    out.push(Action::Key(k));
                }
            }
            ReplayEvent::Paste { text } => {
                if !std::mem::take(&mut drop_next_paste) && !text.is_empty() {
                    out.push(Action::Insert(text.clone()));
                }
            }
            ReplayEvent::Click => {
                drop_next_paste = false;
                out.push(Action::CursorToEnd);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(key: &str, mods: &[Mod]) -> ReplayEvent {
        ReplayEvent::Key {
            key: key.into(),
            mods: mods.to_vec(),
        }
    }

    #[test]
    fn maps_text_keys() {
        let a = map_key("a", &[]).unwrap();
        assert_eq!((a.code, a.vk, a.text.as_deref()), ("KeyA", 65, Some("a")));
        let h = map_key("H", &[Mod::Shift]).unwrap();
        assert_eq!(
            (h.code, h.vk, h.text.as_deref(), h.modifiers),
            ("KeyH", 72, Some("H"), MOD_SHIFT)
        );
        let paren = map_key("(", &[Mod::Shift]).unwrap();
        assert_eq!((paren.code, paren.vk), ("Digit9", 57));
        let brace = map_key("{", &[Mod::Shift]).unwrap();
        assert_eq!((brace.code, brace.vk), ("BracketLeft", 219));
        let quote = map_key("\"", &[Mod::Shift]).unwrap();
        assert_eq!((quote.code, quote.vk), ("Quote", 222));
        let space = map_key(" ", &[]).unwrap();
        assert_eq!((space.code, space.text.as_deref()), ("Space", Some(" ")));
    }

    #[test]
    fn maps_non_ascii_text_without_physical_key() {
        let shin = map_key("ש", &[]).unwrap();
        assert_eq!(
            (shin.code, shin.vk, shin.text.as_deref()),
            ("", 0, Some("ש"))
        );
        let e = map_key("é", &[Mod::Altgr]).unwrap();
        assert_eq!((e.text.as_deref(), e.modifiers), (Some("é"), 0));
        // A shortcut on a key with no known position cannot be replayed.
        assert_eq!(map_key("ש", &[Mod::Ctrl]), None);
    }

    #[test]
    fn maps_shortcuts_and_named_keys() {
        let z = map_key("z", &[Mod::Ctrl]).unwrap();
        assert_eq!(
            (z.code, z.vk, z.text, z.modifiers),
            ("KeyZ", 90, None, MOD_CTRL)
        );
        let slash = map_key("/", &[Mod::Ctrl]).unwrap();
        assert_eq!((slash.code, slash.vk), ("Slash", 191));
        let up = map_key("Up", &[Mod::Alt]).unwrap();
        assert_eq!(
            (up.key.as_str(), up.code, up.vk, up.modifiers),
            ("ArrowUp", "ArrowUp", 38, MOD_ALT)
        );
        let enter = map_key("Enter", &[]).unwrap();
        assert_eq!(enter.text.as_deref(), Some("\r"));
        let ctrl_enter = map_key("Enter", &[Mod::Ctrl]).unwrap();
        assert_eq!(ctrl_enter.text, None);
        let f3 = map_key("F3", &[Mod::Shift]).unwrap();
        assert_eq!((f3.code, f3.vk), ("F3", 114));
        assert_eq!(map_key("Begin", &[]), None);
        assert_eq!(map_key("XF86AudioPlay", &[]), None);
        assert_eq!(map_key("a", &[Mod::Super]), None);
    }

    #[test]
    fn plan_starts_at_end_and_maps_clicks_and_pastes() {
        let p = plan(&[
            k("a", &[]),
            ReplayEvent::Click,
            k("v", &[Mod::Ctrl]),
            ReplayEvent::Paste { text: "xyz".into() },
        ]);
        assert_eq!(p.len(), 4);
        assert_eq!(p[0], Action::CursorToEnd);
        assert!(matches!(&p[1], Action::Key(k) if k.key == "a"));
        assert_eq!(p[2], Action::CursorToEnd);
        assert_eq!(p[3], Action::Insert("xyz".into())); // the Ctrl+V key itself is skipped
    }

    #[test]
    fn plan_skips_focus_stealing_shortcuts() {
        let p = plan(&[
            k("p", &[Mod::Ctrl]),
            k("P", &[Mod::Shift, Mod::Ctrl]),
            k("F1", &[]),
            k("w", &[Mod::Ctrl]),
            k("`", &[Mod::Ctrl]),
            k("f", &[Mod::Alt]),
            k("Tab", &[Mod::Ctrl]),
            k("e", &[Mod::Super]),
            k("z", &[Mod::Ctrl]),
            k("s", &[Mod::Ctrl]),
        ]);
        let keys: Vec<&str> = p
            .iter()
            .filter_map(|a| match a {
                Action::Key(k) => Some(k.code),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["KeyZ", "KeyS"]);
    }

    #[test]
    fn plan_ctrl_k_chords() {
        let p = plan(&[
            k("k", &[Mod::Ctrl]),
            k("c", &[Mod::Ctrl]), // comment: kept
            k("k", &[Mod::Ctrl]),
            k("w", &[Mod::Ctrl]), // close all: dropped with its prefix
            k("x", &[]),
        ]);
        let keys: Vec<&str> = p
            .iter()
            .filter_map(|a| match a {
                Action::Key(k) => Some(k.code),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["KeyK", "KeyC", "KeyX"]);
    }

    #[test]
    fn plan_drops_paste_after_ctrl_shift_v() {
        let p = plan(&[
            k("V", &[Mod::Shift, Mod::Ctrl]),
            ReplayEvent::Paste { text: "no".into() },
            k("Insert", &[Mod::Shift]),
            ReplayEvent::Paste { text: "yes".into() },
        ]);
        assert!(!p.contains(&Action::Insert("no".into())));
        assert!(p.contains(&Action::Insert("yes".into())));
    }
}
