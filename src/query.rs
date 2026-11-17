//! `brain query`: `search` over sessions and `text` over the raw log.
//!
//! Both run while `brain run` writes, on a read-only connection that sees one
//! consistent snapshot of the index (WAL). `search` matches session text;
//! `text` returns an app's reconstructed text as it stood at the end of a
//! window, replaying sessions still running then from the log
//! ([`crate::sessions::text_at`]).

use crate::datadir::DataDir;
use crate::reconstruct::apps::classify;
use crate::sessions;
use crate::store;
use crate::timefmt::{self, Millis};
use anyhow::{Result, bail};
use serde::Serialize;

/// Length of the window `--at T` (and `text` without flags) covers.
pub const DEFAULT_WINDOW_MS: Millis = 10 * 60 * 1000;
/// Snippet context on each side of the first match, in characters.
const SNIPPET_CONTEXT: usize = 40;

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

/// One session's part of `text`.
#[derive(Debug, Serialize)]
pub struct TextSession {
    pub session_id: String,
    pub window: String,
    pub started_at: String,
    /// `None` if the session was still open at the end of the window.
    pub ended_at: Option<String>,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct TextOutput {
    pub app: String,
    pub at: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// The sessions' texts, in order, one per line.
    pub text: String,
    pub sessions: Vec<TextSession>,
}

/// The reconstructed text of one app as it stood at the end of the window:
/// every session of the app active in the window, each as it was at that
/// moment (a session still running is replayed up to it). In editors and
/// other multi-line apps a later session continues the previous one's
/// document, so only the latest session per window is kept; single-line
/// fields (commands, searches, messages) are all kept. `app` may be part of
/// the app id; see [`resolve_app`].
pub fn text(dir: &DataDir, app: &str, flags: &TimeFlags, now: Millis) -> Result<TextOutput> {
    let w = resolve(flags, Command::Text, now)?;
    let (from, to) = (w.from.unwrap_or(Millis::MIN), w.to.unwrap_or(now));
    let conn = store::open_reader(&dir.db_path())?;
    let known: Vec<String> = conn
        .prepare("SELECT DISTINCT app FROM sessions")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let full = resolve_app(app, &known)?;
    // Sessions overlapping the window (open ones "end" now).
    let rows: Vec<(i64, String, String, Option<String>)> = conn
        .prepare(
            "SELECT id, window, started_at, ended_at FROM sessions
             WHERE app = ?1 AND started_at <= ?2 AND COALESCE(ended_at, ?3) >= ?4
             ORDER BY started_at, id",
        )?
        .query_map(
            rusqlite::params![
                full,
                timefmt::format_ms(to),
                timefmt::format_ms(now),
                timefmt::format_ms(from)
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    let mut parts: Vec<TextSession> = Vec::new();
    for (id, window, started_at, ended_at) in rows {
        let text = sessions::text_at(&conn, &dir.log_path(), id, to)?;
        let ended = ended_at.filter(|e| timefmt::parse_rfc3339_ms(e).is_ok_and(|e| e <= to));
        if !classify(&full).single_line() {
            // The document's latest state supersedes its earlier sessions.
            parts.retain(|p| p.window != window);
        }
        if !text.trim().is_empty() {
            parts.push(TextSession {
                session_id: id.to_string(),
                window,
                started_at: timefmt::to_local(&started_at),
                ended_at: ended.map(|e| timefmt::to_local(&e)),
                text,
            });
        }
    }
    Ok(TextOutput {
        app: full,
        at: fmt_opt(w.at),
        from: fmt_opt(w.from),
        to: fmt_opt(w.to),
        text: parts
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        sessions: parts,
    })
}

/// The full app id `given` refers to, among the `known` ones (all apps with
/// sessions): an exact match (ignoring case) wins; otherwise the one id
/// containing it. Several candidates, or none, are an error, so text from
/// different apps is never mixed.
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

    #[test]
    fn like_patterns_escape_wildcards() {
        assert_eq!(like_contains("a%b_c\\"), "%a\\%b\\_c\\\\%");
    }
}
