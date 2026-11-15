//! Tier 0: a virtual text buffer that replays editing keys.
//!
//! It models one text field the way most editors behave: a cursor, an
//! optional selection (anchor + cursor), typing replaces the selection, word
//! motions, logical lines for Up/Down/Home/End, and grouped undo/redo. What
//! the app really did is unknowable from keystrokes alone (soft wrapping,
//! autocomplete, where a click landed), so the model makes one consistent
//! assumption wherever information is missing: **an unknown cursor position
//! is the end of the buffer.**

/// Hard cap on buffer size. Beyond it the oldest text is dropped, which keeps
/// `brain run` inside its memory budget even for a runaway session.
pub const MAX_CHARS: usize = 200_000;
/// Undo depth, in edit groups.
const MAX_UNDO: usize = 200;
/// Lines moved by PageUp/PageDown (the real distance depends on window size).
const PAGE_LINES: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
    PageUp,
    PageDown,
    WordLeft,
    WordRight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Edit {
    pos: usize,
    removed: Vec<char>,
    inserted: Vec<char>,
}

#[derive(Debug, Clone)]
struct Group {
    edits: Vec<Edit>,
    before: (usize, Option<usize>),
    after: (usize, Option<usize>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupKind {
    Typing,
    Deleting,
    Other,
}

#[derive(Debug, Clone, Default)]
pub struct TextBuffer {
    text: Vec<char>,
    cursor: usize,
    /// Selection anchor; the selection is `anchor..cursor` (either order).
    anchor: Option<usize>,
    undo: Vec<Group>,
    redo: Vec<Group>,
    /// Kind of the open undo group, if typing/deleting may still extend it.
    open_group: Option<GroupKind>,
    edits: u64,
}

impl TextBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A buffer restored from saved state. Out-of-range positions are clamped.
    pub fn from_state(text: &str, cursor: usize, anchor: Option<usize>) -> Self {
        let text: Vec<char> = text.chars().collect();
        let len = text.len();
        Self {
            cursor: cursor.min(len),
            anchor: anchor.map(|a| a.min(len)).filter(|&a| a != cursor.min(len)),
            text,
            ..Self::default()
        }
    }

    pub fn text(&self) -> String {
        self.text.iter().collect()
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    /// Number of text-changing operations applied so far.
    pub fn edits(&self) -> u64 {
        self.edits
    }

    fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        (a != self.cursor).then(|| (a.min(self.cursor), a.max(self.cursor)))
    }

    // ---- editing -------------------------------------------------------

    /// Types or pastes `s` at the cursor, replacing the selection.
    pub fn insert(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let chars: Vec<char> = s.chars().collect();
        let had_selection = self.selection().is_some();
        // A word boundary (whitespace after a non-space) starts a new undo
        // group, so Ctrl+Z removes roughly a word at a time, as editors do.
        let boundary = chars.len() == 1
            && chars[0].is_whitespace()
            && self.cursor > 0
            && !self.text[self.cursor - 1].is_whitespace();
        // Typing over a selection starts a group that the following
        // keystrokes extend, so one undo restores the selected text.
        if boundary || had_selection {
            self.open_group = None;
        }
        let kind = if chars.len() == 1 {
            GroupKind::Typing
        } else {
            GroupKind::Other
        };
        let (start, end) = self.selection().unwrap_or((self.cursor, self.cursor));
        self.replace(start, end, chars, kind);
    }

    pub fn backspace(&mut self, word: bool) {
        if let Some((s, e)) = self.selection() {
            return self.replace(s, e, Vec::new(), GroupKind::Other);
        }
        if self.cursor == 0 {
            return;
        }
        let start = if word {
            self.word_left(self.cursor)
        } else {
            self.cursor - 1
        };
        self.replace(start, self.cursor, Vec::new(), GroupKind::Deleting);
    }

    pub fn delete(&mut self, word: bool) {
        if let Some((s, e)) = self.selection() {
            return self.replace(s, e, Vec::new(), GroupKind::Other);
        }
        if self.cursor >= self.text.len() {
            return;
        }
        let end = if word {
            self.word_right(self.cursor)
        } else {
            self.cursor + 1
        };
        self.replace(self.cursor, end, Vec::new(), GroupKind::Deleting);
    }

    /// Ctrl+X: removes the selection (the clipboard is not modelled).
    pub fn cut(&mut self) {
        if let Some((s, e)) = self.selection() {
            self.replace(s, e, Vec::new(), GroupKind::Other);
        }
    }

    /// Readline Ctrl+U: delete from the start of the line to the cursor.
    pub fn kill_to_line_start(&mut self) {
        let start = self.line_start(self.cursor);
        if start < self.cursor {
            self.replace(start, self.cursor, Vec::new(), GroupKind::Other);
        }
    }

    /// Readline Ctrl+K: delete from the cursor to the end of the line.
    pub fn kill_to_line_end(&mut self) {
        let end = self.line_end(self.cursor);
        if end > self.cursor {
            self.replace(self.cursor, end, Vec::new(), GroupKind::Other);
        }
    }

    /// Readline Ctrl+W (unix-word-rubout): delete the whitespace-delimited
    /// word before the cursor.
    pub fn kill_word_back(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.text[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.text[i - 1].is_whitespace() {
            i -= 1;
        }
        if i < self.cursor {
            self.replace(i, self.cursor, Vec::new(), GroupKind::Other);
        }
    }

    /// Replaces the whole content (shell history recall), cursor at the end.
    pub fn set_text(&mut self, s: &str) {
        let chars: Vec<char> = s.chars().collect();
        if chars == self.text {
            self.anchor = None;
            self.cursor = self.text.len();
            return;
        }
        self.replace(0, self.text.len(), chars, GroupKind::Other);
        self.cursor = self.text.len();
    }

    /// Empties the buffer without recording undo (a committed single-line
    /// field starts over).
    pub fn clear(&mut self) {
        *self = Self {
            edits: self.edits,
            ..Self::default()
        };
    }

    fn replace(&mut self, start: usize, end: usize, inserted: Vec<char>, kind: GroupKind) {
        let before = (self.cursor, self.anchor);
        let removed: Vec<char> = self
            .text
            .splice(start..end, inserted.iter().copied())
            .collect();
        self.cursor = start + inserted.len();
        self.anchor = None;
        self.edits += 1;
        self.redo.clear();
        let edit = Edit {
            pos: start,
            removed,
            inserted,
        };
        let after = (self.cursor, self.anchor);
        // Extend the open group when typing (or deleting) continues
        // contiguously; anything else starts a new group.
        let extend = self.open_group == Some(kind)
            && kind != GroupKind::Other
            && self
                .undo
                .last()
                .is_some_and(|g| g.after == before && g.after.1.is_none());
        match (extend, self.undo.last_mut()) {
            (true, Some(g)) => {
                g.edits.push(edit);
                g.after = after;
            }
            _ => {
                self.undo.push(Group {
                    edits: vec![edit],
                    before,
                    after,
                });
                if self.undo.len() > MAX_UNDO {
                    self.undo.remove(0);
                }
            }
        }
        self.open_group = Some(kind);
        self.enforce_cap();
    }

    fn enforce_cap(&mut self) {
        if self.text.len() > MAX_CHARS {
            let drop = self.text.len() - MAX_CHARS;
            self.text.drain(..drop);
            self.cursor = self.cursor.saturating_sub(drop);
            self.anchor = self.anchor.map(|a| a.saturating_sub(drop));
            // Positions in the undo history are no longer valid.
            self.undo.clear();
            self.redo.clear();
            self.open_group = None;
        }
    }

    pub fn undo(&mut self) {
        let Some(g) = self.undo.pop() else { return };
        for e in g.edits.iter().rev() {
            self.text
                .splice(e.pos..e.pos + e.inserted.len(), e.removed.iter().copied());
        }
        (self.cursor, self.anchor) = g.before;
        self.clamp();
        self.edits += 1;
        self.open_group = None;
        self.redo.push(g);
    }

    pub fn redo(&mut self) {
        let Some(g) = self.redo.pop() else { return };
        for e in &g.edits {
            self.text
                .splice(e.pos..e.pos + e.removed.len(), e.inserted.iter().copied());
        }
        (self.cursor, self.anchor) = g.after;
        self.clamp();
        self.edits += 1;
        self.open_group = None;
        self.undo.push(g);
    }

    fn clamp(&mut self) {
        let len = self.text.len();
        self.cursor = self.cursor.min(len);
        self.anchor = self.anchor.map(|a| a.min(len));
    }

    // ---- cursor and selection ------------------------------------------

    /// Moves the cursor; with `extend` (Shift held) the selection grows.
    pub fn move_cursor(&mut self, m: Motion, extend: bool) {
        self.open_group = None;
        if !extend && let Some((s, e)) = self.selection() {
            // Left/Right on a selection collapse it to that side, like editors.
            match m {
                Motion::Left => {
                    self.anchor = None;
                    self.cursor = s;
                    return;
                }
                Motion::Right => {
                    self.anchor = None;
                    self.cursor = e;
                    return;
                }
                _ => {}
            }
        }
        let target = match m {
            Motion::Left => self.cursor.saturating_sub(1),
            Motion::Right => (self.cursor + 1).min(self.text.len()),
            Motion::WordLeft => self.word_left(self.cursor),
            Motion::WordRight => self.word_right(self.cursor),
            Motion::LineStart => self.line_start(self.cursor),
            Motion::LineEnd => self.line_end(self.cursor),
            Motion::DocStart => 0,
            Motion::DocEnd => self.text.len(),
            Motion::Up => self.vertical(self.cursor, -1),
            Motion::Down => self.vertical(self.cursor, 1),
            Motion::PageUp => self.vertical(self.cursor, -(PAGE_LINES as isize)),
            Motion::PageDown => self.vertical(self.cursor, PAGE_LINES as isize),
        };
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = target;
        if self.anchor == Some(self.cursor) {
            self.anchor = None;
        }
    }

    pub fn select_all(&mut self) {
        self.open_group = None;
        if self.text.is_empty() {
            return;
        }
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    /// The cursor position became unknown (a mouse click): assume the end.
    pub fn cursor_unknown(&mut self) {
        self.open_group = None;
        self.anchor = None;
        self.cursor = self.text.len();
    }

    // ---- position helpers ------------------------------------------------

    fn line_start(&self, pos: usize) -> usize {
        self.text[..pos]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |i| i + 1)
    }

    fn line_end(&self, pos: usize) -> usize {
        self.text[pos..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(self.text.len(), |i| pos + i)
    }

    /// Moves `lines` logical lines up (negative) or down, keeping the column.
    fn vertical(&self, pos: usize, lines: isize) -> usize {
        let col = pos - self.line_start(pos);
        let mut start = self.line_start(pos);
        if lines < 0 {
            for _ in 0..lines.unsigned_abs() {
                if start == 0 {
                    return 0; // Up on the first line goes to its start
                }
                start = self.line_start(start - 1);
            }
        } else {
            for _ in 0..lines {
                let end = self.line_end(start);
                if end >= self.text.len() {
                    return self.text.len(); // Down on the last line goes to its end
                }
                start = end + 1;
            }
        }
        (start + col).min(self.line_end(start))
    }

    /// Start of the word before `pos`: skip whitespace, then a run of word
    /// characters or a run of punctuation (Ctrl+Left / Ctrl+Backspace).
    fn word_left(&self, pos: usize) -> usize {
        let mut i = pos;
        while i > 0 && self.text[i - 1].is_whitespace() {
            i -= 1;
        }
        if i == 0 {
            return 0;
        }
        let word = is_word_char(self.text[i - 1]);
        while i > 0 && !self.text[i - 1].is_whitespace() && is_word_char(self.text[i - 1]) == word {
            i -= 1;
        }
        i
    }

    /// End of the word after `pos` (Ctrl+Right / Ctrl+Delete).
    fn word_right(&self, pos: usize) -> usize {
        let len = self.text.len();
        let mut i = pos;
        while i < len && self.text[i].is_whitespace() {
            i += 1;
        }
        if i == len {
            return len;
        }
        let word = is_word_char(self.text[i]);
        while i < len && !self.text[i].is_whitespace() && is_word_char(self.text[i]) == word {
            i += 1;
        }
        i
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> TextBuffer {
        let mut b = TextBuffer::new();
        for c in s.chars() {
            b.insert(&c.to_string());
        }
        b
    }

    #[test]
    fn typing_and_backspace() {
        let mut b = typed("helo");
        b.backspace(false);
        b.insert("lo");
        assert_eq!(b.text(), "hello");
        let mut b = TextBuffer::new();
        b.backspace(false); // at start: no-op
        b.delete(false);
        assert_eq!(b.text(), "");
    }

    #[test]
    fn cursor_movement_and_mid_insert() {
        let mut b = typed("hllo");
        for _ in 0..3 {
            b.move_cursor(Motion::Left, false);
        }
        b.insert("e");
        assert_eq!(b.text(), "hello");
        b.move_cursor(Motion::LineEnd, false);
        b.insert("!");
        b.move_cursor(Motion::DocStart, false);
        b.delete(false);
        assert_eq!(b.text(), "ello!");
    }

    #[test]
    fn selection_is_replaced_by_typing() {
        let mut b = typed("hello world");
        b.move_cursor(Motion::WordLeft, true); // select "world"
        b.insert("there");
        assert_eq!(b.text(), "hello there");
        b.select_all();
        b.insert("x");
        assert_eq!(b.text(), "x");
        b.select_all();
        b.backspace(false);
        assert_eq!(b.text(), "");
    }

    #[test]
    fn left_right_collapse_a_selection() {
        let mut b = typed("abcd");
        b.move_cursor(Motion::Left, true);
        b.move_cursor(Motion::Left, true); // "cd" selected, cursor at 2
        b.move_cursor(Motion::Right, false); // collapse to the end
        assert_eq!((b.cursor(), b.anchor()), (4, None));
    }

    #[test]
    fn word_operations() {
        let mut b = typed("foo.bar  baz");
        b.backspace(true);
        assert_eq!(b.text(), "foo.bar  ");
        b.backspace(true);
        assert_eq!(b.text(), "foo.");
        b.move_cursor(Motion::DocStart, false);
        b.delete(true);
        assert_eq!(b.text(), ".");
    }

    #[test]
    fn readline_kills() {
        let mut b = typed("git commit -m msg");
        b.kill_word_back();
        assert_eq!(b.text(), "git commit -m ");
        b.move_cursor(Motion::LineStart, false);
        b.move_cursor(Motion::WordRight, false);
        b.kill_to_line_end();
        assert_eq!(b.text(), "git");
        b.kill_to_line_start();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn vertical_motion_keeps_column() {
        let mut b = typed("abcdef\nab\nabcdef");
        b.move_cursor(Motion::Up, false); // line 2 is shorter: clamp to its end
        assert_eq!(b.cursor(), 9);
        b.move_cursor(Motion::Up, false);
        b.insert("X");
        assert_eq!(b.text(), "abXcdef\nab\nabcdef");
        b.move_cursor(Motion::PageDown, false);
        assert_eq!(b.cursor(), b.len());
        b.move_cursor(Motion::PageUp, false);
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn undo_groups_words_and_redo() {
        let mut b = typed("hello world");
        b.undo();
        assert_eq!(b.text(), "hello");
        b.undo();
        assert_eq!(b.text(), "");
        b.redo();
        assert_eq!(b.text(), "hello");
        b.insert("!");
        b.redo(); // new edit cleared the redo stack
        assert_eq!(b.text(), "hello!");
        // Deletions group too, and undo restores them.
        let mut b = typed("abc");
        b.backspace(false);
        b.backspace(false);
        assert_eq!(b.text(), "a");
        b.undo();
        assert_eq!(b.text(), "abc");
    }

    #[test]
    fn cursor_jump_breaks_the_undo_group() {
        let mut b = typed("ab");
        b.move_cursor(Motion::Left, false);
        b.insert("X");
        b.undo();
        assert_eq!(b.text(), "ab");
        assert_eq!(b.cursor(), 1);
    }

    #[test]
    fn set_text_and_clear() {
        let mut b = typed("ls");
        b.set_text("cargo build");
        assert_eq!((b.text().as_str(), b.cursor()), ("cargo build", 11));
        b.undo();
        assert_eq!(b.text(), "ls");
        b.clear();
        assert!(b.is_empty());
        assert!(b.edits() > 0, "edit count survives a clear");
    }

    #[test]
    fn unknown_cursor_goes_to_end() {
        let mut b = typed("abc");
        b.move_cursor(Motion::DocStart, false);
        b.cursor_unknown();
        b.insert("d");
        assert_eq!(b.text(), "abcd");
    }

    #[test]
    fn non_ascii_text() {
        let mut b = typed("שלום לכ ן");
        b.backspace(false);
        b.backspace(false);
        b.insert("ם");
        assert_eq!(b.text(), "שלום לכם");
    }

    #[test]
    fn size_cap_drops_oldest_text() {
        let mut b = TextBuffer::new();
        b.insert(&"a".repeat(MAX_CHARS));
        b.insert("bc");
        assert_eq!(b.len(), MAX_CHARS);
        assert!(b.text().ends_with("abc"));
        assert_eq!(b.cursor(), MAX_CHARS);
        b.undo(); // history was dropped with the text: no-op, no panic
        assert_eq!(b.len(), MAX_CHARS);
    }

    #[test]
    fn restore_clamps_positions() {
        let b = TextBuffer::from_state("abc", 10, Some(99));
        assert_eq!((b.cursor(), b.anchor()), (3, None));
        let b = TextBuffer::from_state("abc", 1, Some(3));
        assert_eq!((b.cursor(), b.anchor()), (1, Some(3)));
    }
}
