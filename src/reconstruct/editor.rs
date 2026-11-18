//! Turns logged events into edits of one text field: key → [`Action`] (per app
//! class, tier 1) → [`TextBuffer`] operation (tier 0).

use super::apps::AppClass;
use super::buffer::{Motion, TextBuffer};
use crate::event::Mod;

/// What a key does to the text, decided per app class from the key and its modifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Typed text, replacing the selection.
    Insert(String),
    /// Backspace (Ctrl+Backspace: a word).
    Backspace {
        /// Delete a word (Ctrl), not one character.
        word: bool,
    },
    /// Delete, forward (Ctrl+Delete: a word).
    Delete {
        /// Delete a word (Ctrl), not one character.
        word: bool,
    },
    /// Cursor movement (arrows, Home/End, Ctrl+arrows, PageUp/PageDown).
    Move {
        /// Where the cursor goes.
        motion: Motion,
        /// Shift held: extend the selection instead of moving.
        extend: bool,
    },
    /// Ctrl+A in fields that select all (not in terminals, where it is line start).
    SelectAll,
    /// Ctrl+X: the selection is removed (and goes to the clipboard).
    Cut,
    /// Ctrl+Z (Ctrl+_ in terminals).
    Undo,
    /// Ctrl+Shift+Z or Ctrl+Y.
    Redo,
    /// Ctrl+U in a terminal: delete to the start of the line.
    KillLineStart,
    /// Ctrl+K in a terminal: delete to the end of the line.
    KillLineEnd,
    /// Ctrl+W in a terminal: delete the word before the cursor.
    KillWordBack,
    /// Up or Ctrl+P in a terminal: the previous command brain saw run.
    HistoryPrev,
    /// Down or Ctrl+N in a terminal: back toward the line being typed.
    HistoryNext,
    /// Enter in a single-line field: the text is submitted and the field clears.
    Commit,
    /// Ctrl+C at a shell prompt: the line is abandoned and the prompt clears.
    Cancel,
    /// Focus moved to another field inside the same window (Tab in a web
    /// form, Ctrl+L to the address bar): the session ends, the new field
    /// starts empty.
    FieldChange,
    /// A shortcut with no modelled effect on the text (Ctrl+C copy, Alt+Tab,
    /// Ctrl+S, F5...). Not activity: it neither opens nor extends a session.
    Ignore,
}

/// What an applied action did to the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The session goes on.
    Continue,
    /// A single-line field was submitted (Enter at a prompt): the session ends.
    Commit,
    /// A single-line field was cancelled (Ctrl+C at a prompt): the session ends.
    Cancel,
    /// Focus moved to another field of the same window: the session ends.
    FieldChange,
}

fn has(mods: &[Mod], m: Mod) -> bool {
    mods.contains(&m)
}

/// Maps a logged key (already layout-resolved by capture) to an action.
pub fn action(class: AppClass, key: &str, mods: &[Mod]) -> Action {
    let ctrl = has(mods, Mod::Ctrl);
    let alt = has(mods, Mod::Alt);
    let shift = has(mods, Mod::Shift);
    if has(mods, Mod::Super) {
        return Action::Ignore; // desktop shortcuts
    }
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.clone().next()) {
        return char_action(class, c, ctrl, alt, shift);
    }
    let term = class == AppClass::Terminal;
    let mv = |motion| Action::Move {
        motion,
        // Shift+arrows select in text fields; terminals have no selection.
        extend: shift && !term,
    };
    match key {
        "Enter" if ctrl || alt => match class {
            AppClass::Chat | AppClass::Browser | AppClass::Launcher => Action::Commit,
            _ => Action::Ignore,
        },
        "Enter" => match class {
            AppClass::Terminal => Action::Commit,
            AppClass::Launcher => Action::Commit,
            AppClass::Browser | AppClass::Chat if !shift => Action::Commit,
            _ => Action::Insert("\n".into()),
        },
        "Backspace" if alt && !term => Action::Ignore,
        "Backspace" => Action::Backspace { word: ctrl || alt },
        "Delete" if term && alt => Action::Ignore,
        "Delete" => Action::Delete { word: ctrl },
        "Tab" if ctrl || alt => Action::Ignore,
        "Tab" => match class {
            AppClass::Browser => Action::FieldChange,
            AppClass::Editor | AppClass::Generic if !shift => Action::Insert("\t".into()),
            // Shell completion and chat autocomplete insert text we cannot see.
            _ => Action::Ignore,
        },
        "Left" | "Right" if alt => Action::Ignore, // browser back/forward, editor navigation
        "Left" => mv(if ctrl { Motion::WordLeft } else { Motion::Left }),
        "Right" => mv(if ctrl {
            Motion::WordRight
        } else {
            Motion::Right
        }),
        "Up" | "Down" if alt || ctrl => Action::Ignore, // move line / scroll
        "Up" if term => Action::HistoryPrev,
        "Down" if term => Action::HistoryNext,
        "Up" => mv(Motion::Up),
        "Down" => mv(Motion::Down),
        "Home" => mv(if ctrl {
            Motion::DocStart
        } else {
            Motion::LineStart
        }),
        "End" => mv(if ctrl {
            Motion::DocEnd
        } else {
            Motion::LineEnd
        }),
        "PageUp" | "PageDown" if term || ctrl || alt => Action::Ignore,
        "PageUp" => mv(Motion::PageUp),
        "PageDown" => mv(Motion::PageDown),
        "F6" if class == AppClass::Browser => Action::FieldChange,
        // Escape, Insert, F-keys, media keys...
        _ => Action::Ignore,
    }
}

fn char_action(class: AppClass, c: char, ctrl: bool, alt: bool, shift: bool) -> Action {
    if !ctrl && !alt {
        return Action::Insert(c.to_string());
    }
    let k = c.to_ascii_lowercase();
    if class == AppClass::Terminal {
        // GNU readline (emacs mode) defaults, as in bash and zsh.
        return match (ctrl, alt, shift, k) {
            (true, false, false, 'a') => Action::Move {
                motion: Motion::LineStart,
                extend: false,
            },
            (true, false, false, 'e') => Action::Move {
                motion: Motion::LineEnd,
                extend: false,
            },
            (true, false, false, 'b') => Action::Move {
                motion: Motion::Left,
                extend: false,
            },
            (true, false, false, 'f') => Action::Move {
                motion: Motion::Right,
                extend: false,
            },
            (false, true, _, 'b') => Action::Move {
                motion: Motion::WordLeft,
                extend: false,
            },
            (false, true, _, 'f') => Action::Move {
                motion: Motion::WordRight,
                extend: false,
            },
            (true, false, false, 'h') => Action::Backspace { word: false },
            (true, false, false, 'd') => Action::Delete { word: false },
            (false, true, _, 'd') => Action::Delete { word: true },
            (true, false, false, 'u') => Action::KillLineStart,
            (true, false, false, 'k') => Action::KillLineEnd,
            (true, false, false, 'w') => Action::KillWordBack,
            (true, false, false, 'p') => Action::HistoryPrev,
            (true, false, false, 'n') => Action::HistoryNext,
            (true, false, false, 'c') => Action::Cancel,
            (true, false, false, '_') | (true, false, true, '_') => Action::Undo,
            // Ctrl+Shift+V pastes (the paste event carries the text); plain
            // Ctrl+V is readline's quoted-insert, Ctrl+L clears the screen.
            _ => Action::Ignore,
        };
    }
    if alt {
        return Action::Ignore; // menu accelerators
    }
    match (shift, k) {
        (false, 'a') => Action::SelectAll,
        (false, 'x') => Action::Cut,
        (false, 'z') => Action::Undo,
        (true, 'z') | (false, 'y') => Action::Redo,
        // Moves focus into another field: the address bar (Ctrl+L), the
        // search box (Ctrl+K/Ctrl+E), find (Ctrl+F), a new tab or window, or
        // closes the tab (Ctrl+W).
        (false, 'l' | 'k' | 'e' | 'f' | 't' | 'n' | 'w') if class == AppClass::Browser => {
            Action::FieldChange
        }
        (false, 'k') if class == AppClass::Chat => Action::FieldChange,
        // Ctrl+V/Ctrl+Shift+V: the paste event that follows carries the text.
        _ => Action::Ignore,
    }
}

/// Capture logs a paste on Ctrl+V in every app, but a terminal does not
/// paste on plain Ctrl+V (it is readline's quoted-insert; terminals paste on
/// Ctrl+Shift+V). The paste event right after such a key must be dropped.
pub fn key_suppresses_paste(class: AppClass, key: &str, mods: &[Mod]) -> bool {
    class == AppClass::Terminal
        && key.eq_ignore_ascii_case("v")
        && has(mods, Mod::Ctrl)
        && !has(mods, Mod::Shift)
}

/// The state of one text field being edited during a session.
#[derive(Debug, Clone)]
pub struct FieldEditor {
    /// Which rules apply (terminal, browser, editor, ...).
    pub class: AppClass,
    /// The text and cursor being edited.
    pub buf: TextBuffer,
    /// Shell history position: 0 = most recent command.
    history_index: Option<usize>,
    /// The line being edited before history navigation began.
    saved_line: Option<String>,
}

impl FieldEditor {
    /// An editor of `class` starting from `buf` (empty, or a window's document).
    pub fn new(class: AppClass, buf: TextBuffer) -> Self {
        Self {
            class,
            buf,
            history_index: None,
            saved_line: None,
        }
    }

    /// Applies one action. `history(n)` returns the n-th most recent command
    /// line committed in this app, for shell history recall.
    pub fn apply(
        &mut self,
        a: &Action,
        history: &mut dyn FnMut(usize) -> Option<String>,
    ) -> Outcome {
        let b = &mut self.buf;
        match a {
            Action::Insert(s) => b.insert(s),
            Action::Backspace { word } => b.backspace(*word),
            Action::Delete { word } => b.delete(*word),
            Action::Move { motion, extend } => b.move_cursor(*motion, *extend),
            Action::SelectAll => b.select_all(),
            Action::Cut => b.cut(),
            Action::Undo => b.undo(),
            Action::Redo => b.redo(),
            Action::KillLineStart => b.kill_to_line_start(),
            Action::KillLineEnd => b.kill_to_line_end(),
            Action::KillWordBack => b.kill_word_back(),
            Action::HistoryPrev => {
                let next = self.history_index.map_or(0, |i| i + 1);
                if let Some(line) = history(next) {
                    if self.history_index.is_none() {
                        self.saved_line = Some(b.text());
                    }
                    self.history_index = Some(next);
                    b.set_text(&line);
                }
                // No older entry: readline keeps the line (and beeps).
            }
            Action::HistoryNext => match self.history_index {
                Some(0) => {
                    self.history_index = None;
                    let line = self.saved_line.take().unwrap_or_default();
                    b.set_text(&line);
                }
                Some(i) => {
                    if let Some(line) = history(i - 1) {
                        self.history_index = Some(i - 1);
                        b.set_text(&line);
                    }
                }
                None => {}
            },
            Action::Commit => return Outcome::Commit,
            Action::Cancel => return Outcome::Cancel,
            Action::FieldChange => return Outcome::FieldChange,
            Action::Ignore => {}
        }
        Outcome::Continue
    }

    /// Inserts pasted text at the cursor, replacing the selection.
    pub fn paste(&mut self, text: &str) {
        self.buf.insert(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(class: AppClass, keys: &[(&str, &[Mod])], hist: &[&str]) -> (String, Vec<Outcome>) {
        let mut ed = FieldEditor::new(class, TextBuffer::new());
        let mut outs = Vec::new();
        let mut h = |i: usize| hist.get(i).map(|s| s.to_string());
        for (k, m) in keys {
            let o = ed.apply(&action(class, k, m), &mut h);
            if o != Outcome::Continue {
                outs.push(o);
            }
        }
        (ed.buf.text(), outs)
    }

    fn typing(s: &str) -> Vec<(String, Vec<Mod>)> {
        s.chars().map(|c| (c.to_string(), vec![])).collect()
    }

    fn keys(v: &[(String, Vec<Mod>)]) -> Vec<(&str, &[Mod])> {
        v.iter().map(|(k, m)| (k.as_str(), m.as_slice())).collect()
    }

    const NONE: &[Mod] = &[];
    const CTRL: &[Mod] = &[Mod::Ctrl];
    const SHIFT: &[Mod] = &[Mod::Shift];

    #[test]
    fn enter_commits_only_in_single_line_apps() {
        let (t, o) = run(
            AppClass::Terminal,
            &[("l", NONE), ("s", NONE), ("Enter", NONE)],
            &[],
        );
        assert_eq!((t.as_str(), o), ("ls", vec![Outcome::Commit]));
        let (t, o) = run(
            AppClass::Editor,
            &[("a", NONE), ("Enter", NONE), ("b", NONE)],
            &[],
        );
        assert_eq!((t.as_str(), o), ("a\nb", vec![]));
        let (t, o) = run(
            AppClass::Chat,
            &[("a", NONE), ("Enter", SHIFT), ("b", NONE)],
            &[],
        );
        assert_eq!((t.as_str(), o), ("a\nb", vec![]));
        let (_, o) = run(AppClass::Browser, &[("a", NONE), ("Enter", NONE)], &[]);
        assert_eq!(o, vec![Outcome::Commit]);
    }

    #[test]
    fn readline_shortcuts_in_terminals() {
        let mut k = typing("echo hi there");
        k.push(("w".into(), CTRL.to_vec())); // kill "there"
        k.push(("a".into(), CTRL.to_vec())); // line start (not select-all)
        k.push(("x".into(), NONE.to_vec()));
        let (t, _) = run(AppClass::Terminal, &keys(&k), &[]);
        assert_eq!(t, "xecho hi ");
        let (_, o) = run(AppClass::Terminal, &[("a", NONE), ("c", CTRL)], &[]);
        assert_eq!(o, vec![Outcome::Cancel]);
    }

    #[test]
    fn shell_history_recall() {
        let hist = ["brain query -h", "cargo build"];
        // Up recalls the last command; edit it; run it.
        let mut k: Vec<(String, Vec<Mod>)> = vec![("Up".into(), vec![])];
        k.extend(std::iter::repeat_n(("Backspace".to_string(), vec![]), 2));
        k.extend(typing("search -h"));
        let (t, _) = run(AppClass::Terminal, &keys(&k), &hist);
        assert_eq!(t, "brain query search -h");
        // Up, Up, Down: back to the newer entry; Down again: the draft line.
        let mut k = typing("dra");
        for d in ["Up", "Up", "Up", "Down"] {
            k.push((d.into(), vec![]));
        }
        let (t, _) = run(AppClass::Terminal, &keys(&k), &hist);
        assert_eq!(t, "brain query -h");
        k.push(("Down".into(), vec![]));
        let (t, _) = run(AppClass::Terminal, &keys(&k), &hist);
        assert_eq!(t, "dra");
    }

    #[test]
    fn text_field_shortcuts() {
        let mut k = typing("hello");
        k.push(("a".into(), CTRL.to_vec()));
        k.extend(typing("bye"));
        k.push(("z".into(), CTRL.to_vec()));
        let (t, _) = run(AppClass::Generic, &keys(&k), &[]);
        assert_eq!(t, "hello", "undo restores the replaced selection");
        let (_, o) = run(AppClass::Browser, &[("a", NONE), ("l", CTRL)], &[]);
        assert_eq!(o, vec![Outcome::FieldChange]);
        let (_, o) = run(AppClass::Browser, &[("a", NONE), ("Tab", NONE)], &[]);
        assert_eq!(o, vec![Outcome::FieldChange]);
    }

    #[test]
    fn shortcuts_are_ignored() {
        for (k, m) in [
            ("c", CTRL),
            ("s", CTRL),
            ("Tab", &[Mod::Alt][..]),
            ("e", &[Mod::Super][..]),
            ("Escape", NONE),
            ("F5", NONE),
            ("ש", CTRL),
        ] {
            assert_eq!(action(AppClass::Generic, k, m), Action::Ignore, "{k} {m:?}");
        }
        assert_eq!(action(AppClass::Terminal, "Tab", NONE), Action::Ignore);
        assert_eq!(
            action(AppClass::Editor, "Tab", NONE),
            Action::Insert("\t".into())
        );
        // AltGr characters are text.
        assert_eq!(
            action(AppClass::Generic, "€", &[Mod::Altgr]),
            Action::Insert("€".into())
        );
    }

    #[test]
    fn terminal_plain_ctrl_v_does_not_paste() {
        assert!(key_suppresses_paste(AppClass::Terminal, "v", CTRL));
        assert!(!key_suppresses_paste(
            AppClass::Terminal,
            "V",
            &[Mod::Ctrl, Mod::Shift]
        ));
        assert!(!key_suppresses_paste(AppClass::Generic, "v", CTRL));
    }
}
