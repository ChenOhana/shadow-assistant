//! `brain query`: `search` over sessions and `text` over the log.
//!
//! Both run while `brain run` writes. `search` is a single SELECT on a
//! read-only connection, so it sees one consistent snapshot of the index
//! (WAL). `text` reads the capture log, whose reader only returns complete
//! lines, starting at the checkpoint just before its time range, and runs
//! the range's keys through the tier 0/1 reconstruction, one text per app
//! window.

use crate::datadir::DataDir;
use crate::event::EventKind;
use crate::eventlog::LogReader;
use crate::reconstruct::apps::{AppClass, classify, normalize_window};
use crate::reconstruct::buffer::TextBuffer;
use crate::reconstruct::editor::{Action, FieldEditor, Outcome, action, key_suppresses_paste};
use crate::store;
use crate::timefmt::{self, Millis};
use anyhow::{Result, bail};
use serde::Serialize;

/// Length of the window `--at T` (and `text` without flags) covers.
pub const DEFAULT_WINDOW_MS: Millis = 10 * 60 * 1000;
/// Snippet context on each side of the first match, in characters.
const SNIPPET_CONTEXT: usize = 40;
/// `text` keeps reading this far past its window, in case the clock stepped
/// back while writing the log (timestamps are almost, not strictly, ordered).
const CLOCK_SLACK_MS: Millis = 60_000;

/// The `--at`, `--from` and `--to` flags, as given (see [`timefmt::parse_user_time_at`]).
#[derive(Debug, Clone, Default)]
pub struct TimeFlags {
    pub at: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Search,
    Text,
}

/// A resolved time window; `None` bounds are open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub from: Option<Millis>,
    pub to: Option<Millis>,
    pub at: Option<Millis>,
}

/// Turns the flags into a window.
/// * `--at T`: the 10 minutes ending at T.
/// * `--from A --to B`: [A, B]. `--from A` alone: [A, now].
/// * `--to B` alone: for `text` the 10 minutes ending at B (as `--at`); for
///   `search` everything up to B.
/// * no flags: `text` covers the last 10 minutes, `search` all time.
fn resolve(flags: &TimeFlags, cmd: Command, now: Millis) -> Result<Window> {
    let parse = |s: &Option<String>| -> Result<Option<Millis>> {
        s.as_deref()
            .map(|s| timefmt::parse_user_time_at(s, now))
            .transpose()
    };
    let (at, from, to) = (parse(&flags.at)?, parse(&flags.from)?, parse(&flags.to)?);
    if at.is_some() && (from.is_some() || to.is_some()) {
        bail!("--at cannot be combined with --from or --to");
    }
    let w = match (at, from, to, cmd) {
        (Some(t), ..) => Window {
            from: Some(t - DEFAULT_WINDOW_MS),
            to: Some(t),
            at: Some(t),
        },
        (None, Some(f), Some(t), _) => Window {
            from: Some(f),
            to: Some(t),
            at: None,
        },
        (None, Some(f), None, _) => Window {
            from: Some(f),
            to: Some(now),
            at: None,
        },
        (None, None, Some(t), Command::Text) => Window {
            from: Some(t - DEFAULT_WINDOW_MS),
            to: Some(t),
            at: None,
        },
        (None, None, Some(t), Command::Search) => Window {
            from: None,
            to: Some(t),
            at: None,
        },
        (None, None, None, Command::Text) => Window {
            from: Some(now - DEFAULT_WINDOW_MS),
            to: Some(now),
            at: None,
        },
        (None, None, None, Command::Search) => Window {
            from: None,
            to: None,
            at: None,
        },
    };
    if let (Some(f), Some(t)) = (w.from, w.to)
        && f > t
    {
        bail!(
            "the time window is empty: it starts at {} and ends at {}",
            timefmt::format_local(f),
            timefmt::format_local(t)
        );
    }
    Ok(w)
}

/// Output times are local, with their offset (the database stores UTC).
fn fmt_opt(t: Option<Millis>) -> Option<String> {
    t.map(timefmt::format_local)
}

/// `LIKE` pattern matching `s` anywhere (case-insensitive for ASCII).
fn like_contains(s: &str) -> String {
    let mut out = String::from("%");
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

// ---- search ---------------------------------------------------------------

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SearchResult {
    pub session_id: String,
    pub app: String,
    pub window: String,
    pub started_at: String,
    /// `None` while the session is still open.
    pub ended_at: Option<String>,
    pub snippet: String,
}

#[derive(Debug, Serialize)]
pub struct SearchOutput {
    pub query: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub results: Vec<SearchResult>,
}

/// Sessions whose text contains every word of `query` (substrings, any
/// case), newest first. Open sessions are included with their live text.
pub fn search(
    dir: &DataDir,
    query: &str,
    app: Option<&str>,
    flags: &TimeFlags,
    limit: usize,
    now: Millis,
) -> Result<SearchOutput> {
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        bail!("the search query is empty");
    }
    if limit == 0 {
        bail!("--limit must be at least 1");
    }
    let w = resolve(flags, Command::Search, now)?;
    let conn = store::open_reader(&dir.db_path())?;

    // Words of 3+ characters use the trigram index; shorter ones cannot, and
    // are matched with LIKE instead.
    let long: Vec<String> = words
        .iter()
        .filter(|w| w.chars().count() >= 3)
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect();
    let mut sql = String::from(
        "SELECT id, app, window, started_at, ended_at, reconstructed_text FROM sessions
         WHERE reconstructed_text IS NOT NULL",
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if !long.is_empty() {
        params.push(Box::new(long.join(" AND ")));
        sql += &format!(
            " AND id IN (SELECT rowid FROM sessions_fts WHERE sessions_fts MATCH ?{})",
            params.len()
        );
    }
    for short in words.iter().filter(|w| w.chars().count() < 3) {
        params.push(Box::new(like_contains(short)));
        sql += &format!(" AND reconstructed_text LIKE ?{} ESCAPE '\\'", params.len());
    }
    if let Some(app) = app {
        params.push(Box::new(like_contains(app)));
        sql += &format!(" AND app LIKE ?{} ESCAPE '\\'", params.len());
    }
    // A session overlaps the window if it started before the window ends
    // and ended (or is still open, i.e. "ends now") after it starts.
    if let Some(to) = w.to {
        params.push(Box::new(timefmt::format_ms(to)));
        sql += &format!(" AND started_at <= ?{}", params.len());
    }
    if let Some(from) = w.from {
        params.push(Box::new(timefmt::format_ms(from)));
        let n = params.len();
        params.push(Box::new(timefmt::format_ms(now)));
        sql += &format!(" AND COALESCE(ended_at, ?{}) >= ?{n}", params.len());
    }
    params.push(Box::new(limit as i64));
    sql += &format!(" ORDER BY started_at DESC, id DESC LIMIT ?{}", params.len());

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
        let text: String = r.get(5)?;
        Ok(SearchResult {
            session_id: r.get::<_, i64>(0)?.to_string(),
            app: r.get(1)?,
            window: r.get(2)?,
            started_at: timefmt::to_local(&r.get::<_, String>(3)?),
            ended_at: r
                .get::<_, Option<String>>(4)?
                .map(|t| timefmt::to_local(&t)),
            snippet: snippet(&text, &words),
        })
    })?;
    Ok(SearchOutput {
        query: query.to_string(),
        from: fmt_opt(w.from),
        to: fmt_opt(w.to),
        results: rows.collect::<rusqlite::Result<_>>()?,
    })
}

/// About 40 characters on each side of the first match of any query word,
/// on one line, with "…" where the text was cut.
fn snippet(text: &str, words: &[&str]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| fold(*c)).collect();
    let hit = words
        .iter()
        .filter_map(|w| {
            let needle: Vec<char> = w.chars().map(fold).collect();
            find(&lower, &needle).map(|i| (i, needle.len()))
        })
        .min();
    let (pos, len) = hit.unwrap_or((0, 0));
    let start = pos.saturating_sub(SNIPPET_CONTEXT);
    let end = (pos + len + SNIPPET_CONTEXT).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let mut last_space = false;
    for &c in &chars[start..end] {
        let space = c.is_whitespace();
        if !(space && last_space) {
            out.push(if space { ' ' } else { c });
        }
        last_space = space;
    }
    if end < chars.len() {
        out.push('…');
    }
    out.trim().to_string()
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn find(hay: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

// ---- text -----------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct TextOutput {
    pub app: String,
    pub at: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// The windows' texts, one per line.
    pub text: String,
    pub windows: Vec<TextWindow>,
}

/// One window's reconstructed text.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TextWindow {
    pub window: String,
    pub text: String,
}

/// The text typed into one app over the window, reconstructed: the window's
/// keys and pastes, per window title, run through the app's tier 0/1 adapter
/// (see [`reconstruct_windows`]). `app` may be part of the app id; see
/// `resolve_app`.
pub fn text(dir: &DataDir, app: &str, flags: &TimeFlags, now: Millis) -> Result<TextOutput> {
    let w = resolve(flags, Command::Text, now)?;
    let (from, to) = (w.from.unwrap_or(Millis::MIN), w.to.unwrap_or(now));
    let conn = store::open_reader(&dir.db_path())?;
    let start = store::seek_offset_before(&conn, &timefmt::format_ms(from))?;
    let needle = app.to_lowercase();
    let mut known: Vec<String> = conn
        .prepare("SELECT DISTINCT app FROM sessions")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(conn);
    // Events of every app matching `app` in the window, in log order; the
    // app is resolved once the window's apps are known too.
    let mut matching: Vec<(String, Millis, EventKind)> = Vec::new();
    if dir.log_path().exists() {
        let mut reader = LogReader::open(&dir.log_path(), start)?;
        while let Some(line) = reader.next_line()? {
            let Some(ev) = line.event else { continue };
            if ev.ts > to.saturating_add(CLOCK_SLACK_MS) {
                break;
            }
            let typed = matches!(ev.kind, EventKind::Key { .. } | EventKind::Paste { .. });
            if typed && ev.ts >= from && ev.ts <= to && ev.app.to_lowercase().contains(&needle) {
                if !known.contains(&ev.app) {
                    known.push(ev.app.clone());
                }
                matching.push((ev.app, ev.ts, ev.kind));
            }
        }
    }
    let full = resolve_app(app, &known)?;
    let events: Vec<(Millis, EventKind)> = matching
        .into_iter()
        .filter(|(a, ..)| *a == full)
        .map(|(_, ts, kind)| (ts, kind))
        .collect();
    let windows = reconstruct_windows(classify(&full), &events);
    Ok(TextOutput {
        at: fmt_opt(w.at),
        from: fmt_opt(w.from),
        to: fmt_opt(w.to),
        text: windows
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        app: full,
        windows,
    })
}

/// Reconstructs one app's typing, given its key and paste events in log
/// order. The events are split per window (normalized title) and each
/// window's events are run, from an empty text, through the app's tier 0/1
/// adapter: the same key-to-action rules ([`action`]) and text field
/// ([`FieldEditor`]) the session engine uses. Sessions play no part.
///
/// When the adapter submits or leaves a single-line field (Enter at a shell
/// prompt, Ctrl+C there, Tab in a browser) the field clears, as in the app:
/// its line is kept, and typing continues on a fresh line of the same
/// window's text. Shell history (Up/Down) recalls lines submitted earlier in
/// the same window. Windows are ordered by their first event; windows with
/// no text are left out.
pub fn reconstruct_windows(class: AppClass, events: &[(Millis, EventKind)]) -> Vec<TextWindow> {
    struct Field {
        window: String,
        editor: FieldEditor,
        /// Lines of the window's text that are finished.
        lines: Vec<String>,
        /// Lines submitted with Enter, for history recall (oldest first).
        submitted: Vec<String>,
        /// A plain Ctrl+V in a terminal at this time: drop its paste.
        suppress_paste_at: Option<Millis>,
    }
    let mut fields: Vec<Field> = Vec::new();
    for (ts, kind) in events {
        let Some(title) = (match kind {
            EventKind::Key { window, .. } | EventKind::Paste { window, .. } => Some(window),
            _ => None,
        }) else {
            continue;
        };
        let window = normalize_window(title);
        let i = match fields.iter().position(|f| f.window == window) {
            Some(i) => i,
            None => {
                fields.push(Field {
                    window,
                    editor: FieldEditor::new(class, TextBuffer::new()),
                    lines: Vec::new(),
                    submitted: Vec::new(),
                    suppress_paste_at: None,
                });
                fields.len() - 1
            }
        };
        let f = &mut fields[i];
        match kind {
            EventKind::Key { key, mods, .. } => {
                f.suppress_paste_at = key_suppresses_paste(class, key, mods).then_some(*ts);
                let act = action(class, key, mods);
                if act == Action::Ignore {
                    continue;
                }
                let submitted = &f.submitted;
                let mut history = |n: usize| submitted.iter().rev().nth(n).cloned();
                let outcome = f.editor.apply(&act, &mut history);
                if outcome != Outcome::Continue {
                    let line = f.editor.buf.text();
                    if outcome == Outcome::Commit && !line.trim().is_empty() {
                        f.submitted.push(line.clone());
                    }
                    if !line.trim().is_empty() {
                        f.lines.push(line);
                    }
                    f.editor = FieldEditor::new(class, TextBuffer::new());
                }
            }
            // `take`: the suppression applies to the one paste right after it.
            EventKind::Paste {
                text: Some(text), ..
            } if f.suppress_paste_at.take() != Some(*ts) => f.editor.paste(text),
            _ => {}
        }
    }
    fields
        .into_iter()
        .filter_map(|mut f| {
            let rest = f.editor.buf.text();
            if !rest.is_empty() {
                f.lines.push(rest);
            }
            let text = f.lines.join("\n");
            (!text.trim().is_empty()).then_some(TextWindow {
                window: f.window,
                text,
            })
        })
        .collect()
}

/// The full app id `given` refers to, among the `known` ones (all apps with
/// sessions, plus those with keys or pastes in the queried window): an exact match (ignoring
/// case) wins; otherwise the one id containing it. Several candidates, or
/// none, are an error, so text from different apps is never mixed.
fn resolve_app(given: &str, known: &[String]) -> Result<String> {
    let g = given.to_lowercase();
    if let Some(exact) = known.iter().find(|a| a.to_lowercase() == g) {
        return Ok(exact.clone());
    }
    let mut hits: Vec<&String> = known
        .iter()
        .filter(|a| a.to_lowercase().contains(&g))
        .collect();
    hits.sort();
    hits.dedup();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] if known.is_empty() => {
            bail!("no app matches {given:?}: no typing has been captured in any app yet")
        }
        [] => {
            let mut all: Vec<&String> = known.iter().collect();
            all.sort();
            all.dedup();
            let list: Vec<&str> = all.iter().map(|s| s.as_str()).collect();
            bail!("no app matches {given:?} (known apps: {})", list.join(", "))
        }
        many => {
            let list: Vec<&str> = many.iter().map(|s| s.as_str()).collect();
            bail!(
                "{given:?} matches several apps: {}; be more specific",
                list.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Millis = 1_800_000_000_000;

    fn flags(at: Option<&str>, from: Option<&str>, to: Option<&str>) -> TimeFlags {
        TimeFlags {
            at: at.map(str::to_string),
            from: from.map(str::to_string),
            to: to.map(str::to_string),
        }
    }

    #[test]
    fn windows() {
        let w = |f, c| resolve(&f, c, NOW).unwrap();
        let ten = DEFAULT_WINDOW_MS;
        let h = 3_600_000;
        // No flags.
        assert_eq!(
            w(flags(None, None, None), Command::Text).from,
            Some(NOW - ten)
        );
        assert_eq!(w(flags(None, None, None), Command::Search).from, None);
        // --at: the 10 minutes ending at T, for both.
        for c in [Command::Text, Command::Search] {
            let x = w(flags(Some("-1h"), None, None), c);
            assert_eq!(
                (x.from, x.to, x.at),
                (Some(NOW - h - ten), Some(NOW - h), Some(NOW - h))
            );
        }
        // --from alone: up to now.
        let x = w(flags(None, Some("-2h"), None), Command::Search);
        assert_eq!((x.from, x.to), (Some(NOW - 2 * h), Some(NOW)));
        // --to alone.
        assert_eq!(
            w(flags(None, None, Some("-1h")), Command::Text).from,
            Some(NOW - h - ten)
        );
        assert_eq!(
            w(flags(None, None, Some("-1h")), Command::Search).from,
            None
        );
        // Both.
        let x = w(flags(None, Some("-2h"), Some("-1h")), Command::Text);
        assert_eq!((x.from, x.to), (Some(NOW - 2 * h), Some(NOW - h)));
    }

    #[test]
    fn bad_windows_are_errors() {
        assert!(resolve(&flags(Some("-1h"), Some("-2h"), None), Command::Text, NOW).is_err());
        assert!(resolve(&flags(None, Some("-1h"), Some("-2h")), Command::Text, NOW).is_err());
        assert!(resolve(&flags(None, Some("yesterday"), None), Command::Text, NOW).is_err());
    }

    #[test]
    fn snippets() {
        let t = "The migration is blocked on the schema change. Dana asked whether we can ship search on Friday.";
        assert_eq!(
            snippet(t, &["DANA"]),
            "…ration is blocked on the schema change. Dana asked whether we can ship search on Fri…"
        );
        assert_eq!(snippet("short\n\ntext", &["text"]), "short text");
        assert_eq!(snippet("no match here", &["zzz"]), "no match here");
    }

    #[test]
    fn app_names_resolve_to_full_ids() {
        let known: Vec<String> = [
            "org.kde.kate",
            "google-chrome",
            "code",
            "com.microsoft.VSCode",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(resolve_app("kate", &known).unwrap(), "org.kde.kate");
        assert_eq!(resolve_app("CHROME", &known).unwrap(), "google-chrome");
        // Exact beats substring: "code" is an app, and part of another.
        assert_eq!(resolve_app("code", &known).unwrap(), "code");
        assert_eq!(
            resolve_app("vscode", &known).unwrap(),
            "com.microsoft.VSCode"
        );
        let e = resolve_app("o", &known).unwrap_err().to_string();
        assert!(e.contains("several apps"), "{e}");
        let e = resolve_app("firefox", &known).unwrap_err().to_string();
        assert!(
            e.contains("no app matches") && e.contains("org.kde.kate"),
            "{e}"
        );
        let e = resolve_app("kate", &[]).unwrap_err().to_string();
        assert!(e.contains("no typing has been captured"), "{e}");
    }

    use crate::event::Mod;

    /// Events of the detailed example in the README: two Kate windows (the
    /// first one edited in two separate sessions) and a Konsole prompt.
    struct Typing(Vec<(Millis, EventKind)>);
    impl Typing {
        fn key(&mut self, ts: Millis, window: &str, key: &str, mods: &[Mod]) {
            self.0.push((
                ts,
                EventKind::Key {
                    key: key.into(),
                    mods: mods.to_vec(),
                    repeat: false,
                    window: window.into(),
                },
            ));
        }
        /// Types `s` (⏎ Enter, ⌫ Backspace, ⇥ Tab) from `ts`, 100 ms apart.
        fn typ(&mut self, ts: Millis, window: &str, s: &str) -> Millis {
            let mut t = ts;
            for c in s.chars() {
                let k = match c {
                    '⏎' => "Enter".to_string(),
                    '⌫' => "Backspace".to_string(),
                    '⇥' => "Tab".to_string(),
                    c => c.to_string(),
                };
                self.key(t, window, &k, &[]);
                t += 100;
            }
            t
        }
        fn paste(&mut self, ts: Millis, window: &str, text: &str) {
            self.0.push((
                ts,
                EventKind::Paste {
                    text: Some(text.into()),
                    window: window.into(),
                },
            ));
        }
    }

    const CTRL: &[Mod] = &[Mod::Ctrl];
    const NOTES: &str = "notes.md — Kate";

    /// Kate: notes.md (two sessions, 3 minutes apart) and todo.md. Returns
    /// the events and the time the second notes.md session starts.
    fn kate_example() -> (Vec<(Millis, EventKind)>, Millis) {
        let mut k = Typing(Vec::new());
        k.typ(NOW, NOTES, "Shopping list:⏎- milk⏎- breadd⌫");
        k.typ(
            NOW + 60_000,
            "todo.md — Kate",
            "TODO: call Dana⏎⇥send the \"Q3\" report⏎path: C:\\temp",
        );
        let s2 = NOW + 180_000;
        // While unsaved, Kate's title is "notes.md * — Kate": same window.
        let dirty = "notes.md * — Kate";
        k.key(s2, dirty, "Home", CTRL);
        k.key(s2 + 100, dirty, "Down", &[]);
        k.key(s2 + 200, dirty, "End", &[]);
        k.key(s2 + 300, dirty, "Backspace", CTRL);
        let t = k.typ(s2 + 400, dirty, "oat milk");
        k.key(t, dirty, "End", CTRL);
        let t = k.typ(t + 100, dirty, "⏎- קפה");
        k.key(t, dirty, "v", CTRL);
        k.paste(t, dirty, " ☕ x2");
        k.key(t + 100, dirty, "z", CTRL); // undoes the paste
        k.typ(t + 200, dirty, " x2");
        (k.0, s2)
    }

    #[test]
    fn text_reconstructs_each_window_across_sessions() {
        let (events, _) = kate_example();
        assert_eq!(
            reconstruct_windows(classify("org.kde.kate"), &events),
            vec![
                TextWindow {
                    window: NOTES.into(),
                    text: "Shopping list:\n- oat milk\n- bread\n- קפה x2".into(),
                },
                TextWindow {
                    window: "todo.md — Kate".into(),
                    text: "TODO: call Dana\n\tsend the \"Q3\" report\npath: C:\\temp".into(),
                },
            ]
        );
    }

    #[test]
    fn text_starts_empty_at_the_window_start() {
        // Only the second notes.md session is inside the window: the edits
        // that reached into earlier text have nothing to act on.
        let (events, s2) = kate_example();
        let later: Vec<_> = events.into_iter().filter(|(ts, _)| *ts >= s2).collect();
        assert_eq!(
            reconstruct_windows(classify("org.kde.kate"), &later),
            vec![TextWindow {
                window: NOTES.into(),
                text: "oat milk\n- קפה x2".into(),
            }]
        );
    }

    #[test]
    fn text_follows_the_terminal_adapter() {
        let mut k = Typing(Vec::new());
        let t = k.typ(NOW, "~", "git stauts⌫⌫⌫tus⏎");
        k.key(t, "~", "Up", &[]); // recalls "git status"
        let t = k.typ(t + 100, "~", " -s⏎rm -rf build");
        k.key(t, "~", "c", CTRL); // abandons the line
        let t = k.typ(t + 100, "~", "echo \"שלום\" > out.txt⏎");
        // Plain Ctrl+V does not paste in a terminal; an empty Enter adds nothing.
        k.key(t, "~", "v", CTRL);
        k.paste(t, "~", "not pasted");
        k.typ(t + 100, "~", "⏎");
        assert_eq!(
            reconstruct_windows(classify("org.kde.konsole"), &k.0),
            vec![TextWindow {
                window: "~".into(),
                text: "git status\ngit status -s\nrm -rf build\necho \"שלום\" > out.txt".into(),
            }]
        );
    }

    #[test]
    fn like_patterns_escape_wildcards() {
        assert_eq!(like_contains("a%b_c\\"), "%a\\%b\\_c\\\\%");
    }
}
