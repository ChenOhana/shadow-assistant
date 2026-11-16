//! VS Code window titles: which app ids are VS Code, the dirty marker, and
//! which file on disk a title refers to.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// App ids VS Code reports under KDE/Wayland (desktop file name, or the
/// resource class when there is none), plus the OSS builds and Flatpak id.
const VSCODE_APP_IDS: &[&str] = &[
    "code",
    "code-url-handler",
    "code-insiders",
    "visual-studio-code",
    "com.visualstudio.code",
    "com.microsoft.vscode", // Microsoft's .deb/.rpm package on KDE Wayland
    "com.visualstudio.code.oss",
    "code-oss",
    "codium",
    "vscodium",
    "com.vscodium.codium",
];

/// App-name suffixes VS Code puts last in its title, longest first so
/// "Visual Studio Code - Insiders" wins over "Visual Studio Code".
const APP_NAMES: &[&str] = &[
    "Visual Studio Code - Insiders",
    "Visual Studio Code",
    "Code - OSS",
    "VSCodium",
];

/// VS Code's `${dirty}` marker.
const DIRTY_MARKER: char = '●';

/// Directories never descended into when looking for a file by name.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "dist", "build"];
const MAX_DEPTH: usize = 8;
const MAX_ENTRIES: usize = 20_000;

pub fn is_vscode(app: &str) -> bool {
    let app = app.to_ascii_lowercase();
    VSCODE_APP_IDS.contains(&app.as_str())
}

pub fn title_is_dirty(window_title: &str) -> bool {
    window_title.trim_start().starts_with(DIRTY_MARKER)
}

/// A VS Code title split into its parts.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedTitle {
    /// First segment: `${activeEditorShort}` or, with a custom
    /// `window.title`, possibly a path.
    pub editor: String,
    /// Second segment, normally `${rootName}`.
    pub root: Option<String>,
    /// All segments (editor first), for finding an absolute path anywhere.
    pub segments: Vec<String>,
}

/// Splits a VS Code title on its separator (" - " by default, " — " is also
/// accepted), dropping the dirty marker and the trailing app name. Returns
/// `None` if the title does not end with a VS Code app name.
pub(crate) fn parse_title(title: &str) -> Option<ParsedTitle> {
    let mut t = title.trim();
    if let Some(rest) = t.strip_prefix(DIRTY_MARKER) {
        t = rest.trim_start();
    }
    let mut body = None;
    for name in APP_NAMES {
        for sep in [" - ", " — "] {
            if let Some(b) = t.strip_suffix(name).and_then(|b| b.strip_suffix(sep)) {
                body = Some(b);
                break;
            }
        }
        if body.is_some() {
            break;
        }
    }
    let body = body?;
    let segments: Vec<String> = body
        .split(" - ")
        .flat_map(|s| s.split(" — "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let editor = segments.first()?.clone();
    Some(ParsedTitle {
        editor,
        root: segments.get(1).cloned(),
        segments,
    })
}

/// Absolute path of the file being edited, from the window title; see the
/// module docs of `vscode` for the two supported title shapes.
pub fn resolve_file(window_title: &str) -> Option<PathBuf> {
    let parsed = parse_title(window_title)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    // (1) A custom window.title that contains a full path.
    for seg in &parsed.segments {
        if let Some(p) = expand_path(seg, home.as_deref())
            && p.is_file()
        {
            return Some(p);
        }
    }
    // (2) The default "<file> - <rootName> - Visual Studio Code".
    let name = parsed.editor.as_str();
    if name.contains('/') || name == "." || name == ".." {
        return None;
    }
    let config = vscode_config_dir()?;
    let history = RecentHistory::load(&config);
    let mut found: BTreeSet<PathBuf> = BTreeSet::new();
    match &parsed.root {
        Some(root) => {
            for folder in history.folders.iter().filter(|f| {
                f.file_name().and_then(|n| n.to_str()) == Some(root.as_str()) && f.is_dir()
            }) {
                find_named(folder, name, &mut found);
                if found.len() > 1 {
                    return None;
                }
            }
        }
        None => {
            // A single file opened without a folder.
            for f in history
                .files
                .iter()
                .filter(|f| f.file_name().and_then(|n| n.to_str()) == Some(name) && f.is_file())
            {
                found.insert(f.clone());
            }
        }
    }
    if found.len() == 1 {
        found.into_iter().next()
    } else {
        None
    }
}

fn expand_path(seg: &str, home: Option<&Path>) -> Option<PathBuf> {
    if seg.starts_with('/') {
        Some(PathBuf::from(seg))
    } else if let Some(rest) = seg.strip_prefix("~/") {
        Some(home?.join(rest))
    } else {
        None
    }
}

fn vscode_config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let dir = base.join("Code");
    dir.is_dir().then_some(dir)
}

/// Folders and files VS Code has recently had open, from every place it
/// records them (the format has moved between releases).
#[derive(Default, Debug)]
pub(crate) struct RecentHistory {
    pub folders: BTreeSet<PathBuf>,
    pub files: BTreeSet<PathBuf>,
}

impl RecentHistory {
    pub(crate) fn load(config: &Path) -> Self {
        let mut h = RecentHistory::default();
        let gs = config.join("User/globalStorage");
        if let Some(v) = read_json(&gs.join("storage.json")) {
            h.add_storage_json(&v);
        }
        if let Some(list) = read_state_db(&gs.join("state.vscdb")) {
            h.add_recent_list(&list);
        }
        // Every folder ever opened has a workspaceStorage entry.
        if let Ok(rd) = std::fs::read_dir(config.join("User/workspaceStorage")) {
            for e in rd.flatten().take(5000) {
                if let Some(v) = read_json(&e.path().join("workspace.json"))
                    && let Some(p) = v
                        .get("folder")
                        .and_then(|f| f.as_str())
                        .and_then(file_uri_to_path)
                {
                    h.folders.insert(p);
                }
            }
        }
        h
    }

    fn add_storage_json(&mut self, v: &serde_json::Value) {
        if let Some(folders) = v
            .pointer("/backupWorkspaces/folders")
            .and_then(|f| f.as_array())
        {
            for f in folders {
                if let Some(p) = f
                    .get("folderUri")
                    .and_then(|u| u.as_str())
                    .and_then(file_uri_to_path)
                {
                    self.folders.insert(p);
                }
            }
        }
        if let Some(ws) = v.get("windowsState") {
            let mut windows: Vec<&serde_json::Value> = Vec::new();
            if let Some(w) = ws.get("lastActiveWindow") {
                windows.push(w);
            }
            if let Some(arr) = ws.get("openedWindows").and_then(|o| o.as_array()) {
                windows.extend(arr);
            }
            for w in windows {
                if let Some(p) = w
                    .get("folder")
                    .and_then(|u| u.as_str())
                    .and_then(file_uri_to_path)
                {
                    self.folders.insert(p);
                }
            }
        }
        if let Some(list) = v.get("history.recentlyOpenedPathsList") {
            self.add_recent_list(list);
        }
    }

    fn add_recent_list(&mut self, list: &serde_json::Value) {
        let Some(entries) = list.get("entries").and_then(|e| e.as_array()) else {
            return;
        };
        for e in entries {
            if let Some(p) = e
                .get("folderUri")
                .and_then(|u| u.as_str())
                .and_then(file_uri_to_path)
            {
                self.folders.insert(p);
            } else if let Some(p) = e
                .get("fileUri")
                .and_then(|u| u.as_str())
                .and_then(file_uri_to_path)
            {
                self.files.insert(p);
            }
        }
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_state_db(path: &Path) -> Option<serde_json::Value> {
    if !path.is_file() {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(200));
    let raw: String = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = 'history.recentlyOpenedPathsList'",
            [],
            |r| r.get(0),
        )
        .ok()?;
    serde_json::from_str(&raw).ok()
}

/// `file:///home/a%20b/x` → `/home/a b/x`. Other schemes (remote, vscode-vfs) → None.
pub(crate) fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    if !rest.starts_with('/') {
        return None; // file://host/... is not local
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// Adds every regular file named `name` under `root` to `found`, with a
/// bounded walk (depth, entry count), never following symlinks. Stops early
/// once two matches are known (the result is ambiguous then anyway).
fn find_named(root: &Path, name: &str, found: &mut BTreeSet<PathBuf>) {
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES || found.len() > 1 {
                return;
            }
            let Ok(ft) = e.file_type() else { continue };
            let fname = e.file_name();
            if ft.is_dir() {
                if depth < MAX_DEPTH && !SKIP_DIRS.iter().any(|s| fname == *s) {
                    stack.push((e.path(), depth + 1));
                }
            } else if ft.is_file() && fname == name {
                found.insert(e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vscode_app_ids() {
        assert!(is_vscode("code"));
        assert!(is_vscode("Code"));
        assert!(is_vscode("code-url-handler"));
        assert!(is_vscode("com.visualstudio.code"));
        assert!(is_vscode("com.microsoft.VSCode"));
        assert!(!is_vscode("org.kde.kate"));
        assert!(!is_vscode("google-chrome"));
    }

    #[test]
    fn dirty_marker() {
        assert!(title_is_dirty("● main.rs - brain - Visual Studio Code"));
        assert!(!title_is_dirty("main.rs - brain - Visual Studio Code"));
    }

    #[test]
    fn parses_default_titles() {
        let p = parse_title("● main.rs - brain - Visual Studio Code").unwrap();
        assert_eq!(p.editor, "main.rs");
        assert_eq!(p.root.as_deref(), Some("brain"));
        let p = parse_title("main.rs — brain — Visual Studio Code").unwrap();
        assert_eq!(p.editor, "main.rs");
        assert_eq!(p.root.as_deref(), Some("brain"));
        let p = parse_title("notes.md - Visual Studio Code").unwrap();
        assert_eq!(p.editor, "notes.md");
        assert_eq!(p.root, None);
        let p = parse_title("a.rs - proj - Work - Visual Studio Code - Insiders").unwrap();
        assert_eq!(p.segments, vec!["a.rs", "proj", "Work"]);
        assert!(parse_title("New tab - Google Chrome").is_none());
        assert!(parse_title("Visual Studio Code").is_none());
    }

    #[test]
    fn resolves_absolute_path_titles() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("x.rs");
        std::fs::write(&f, "fn main() {}").unwrap();
        let title = format!("● {} - proj - Visual Studio Code", f.display());
        assert_eq!(resolve_file(&title), Some(f.clone()));
        let missing = format!("{}/nope.rs - Visual Studio Code", tmp.path().display());
        assert_eq!(resolve_file(&missing), None);
    }

    #[test]
    fn file_uris() {
        assert_eq!(
            file_uri_to_path("file:///home/u/my%20proj"),
            Some(PathBuf::from("/home/u/my proj"))
        );
        assert_eq!(file_uri_to_path("vscode-remote://ssh/x"), None);
        assert_eq!(file_uri_to_path("file://host/x"), None);
    }

    #[test]
    fn history_from_storage_json() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"backupWorkspaces":{"folders":[{"folderUri":"file:///a/proj"}]},
                "windowsState":{"lastActiveWindow":{"folder":"file:///b/other"},"openedWindows":[]},
                "history.recentlyOpenedPathsList":{"entries":[{"fileUri":"file:///c/n.md"},{"folderUri":"file:///d/x"}]}}"#,
        )
        .unwrap();
        let mut h = RecentHistory::default();
        h.add_storage_json(&v);
        let folders: Vec<_> = h.folders.iter().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(folders, vec!["/a/proj", "/b/other", "/d/x"]);
        assert!(h.files.contains(Path::new("/c/n.md")));
    }

    #[test]
    fn finds_unique_file_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("src/deep/lib.rs"), "").unwrap();
        std::fs::write(root.join("target/debug/lib.rs"), "").unwrap(); // skipped dir
        let mut found = BTreeSet::new();
        find_named(root, "lib.rs", &mut found);
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            vec![root.join("src/deep/lib.rs")]
        );
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        let mut found = BTreeSet::new();
        find_named(root, "lib.rs", &mut found);
        assert_eq!(found.len(), 2); // ambiguous
    }
}
