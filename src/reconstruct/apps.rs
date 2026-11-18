//! Tier 1: a small, explicit table of app classes and their conventions.
//!
//! The class decides whether Enter commits a single-line field (a shell
//! prompt, a chat compose box, a browser's address/search bar) or inserts a
//! newline (editors and everything unknown), and which shortcuts mean what.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppClass {
    /// Shell prompts: readline editing, history, Enter runs the line.
    Terminal,
    /// Address and search bars, web forms: Enter submits, Shift+Enter is a newline.
    Browser,
    /// Chat compose boxes: Enter sends, Shift+Enter is a newline.
    Chat,
    /// App launchers and run dialogs (KRunner): a search field, Enter runs.
    Launcher,
    /// Code and text editors: multi-line.
    Editor,
    /// Anything not in the table: multi-line, common text-field shortcuts.
    Generic,
}

impl AppClass {
    /// Whether Enter commits (closes the session and clears the field).
    pub fn single_line(self) -> bool {
        matches!(
            self,
            AppClass::Terminal | AppClass::Browser | AppClass::Chat | AppClass::Launcher
        )
    }

    /// The reconstruction tier this class gets: 1 when per-app rules apply.
    pub fn tier(self) -> i64 {
        match self {
            AppClass::Generic => 0,
            _ => 1,
        }
    }
}

const TERMINALS: &[&str] = &[
    "ghostty",
    "konsole",
    "yakuake",
    "terminal", // org.gnome.Terminal, xfce4-terminal, ...
    "kitty",
    "alacritty",
    "wezterm",
    "foot",
    "xterm",
    "urxvt",
    "tilix",
    "terminator",
    "terminology",
    "ptyxis",  // Ubuntu's default terminal (org.gnome.Ptyxis)
    "console", // GNOME Console (org.gnome.Console)
    "kgx",     // GNOME Console's older id
    "warp",
];
const BROWSERS: &[&str] = &[
    "chrome",
    "chromium",
    "firefox",
    "brave",
    "edge",
    "vivaldi",
    "opera",
    "librewolf",
    "zen",
    "epiphany",
    "falkon",
];
const CHATS: &[&str] = &[
    "slack",
    "discord",
    "telegram",
    "telegramdesktop",
    "signal",
    "element",
    "whatsapp",
    "teams",
    "zoom",
    "skype",
    "mattermost",
    "rocketchat",
];
const LAUNCHERS: &[&str] = &[
    "krunner",
    "ulauncher",
    "albert",
    "rofi",
    "wofi",
    "fuzzel",
    "tofi",
    "kickoff",
    "cerebro",
];
const EDITORS: &[&str] = &[
    "code",
    "vscode",
    "codium",
    "kate",
    "kwrite",
    "gedit",
    "texteditor", // org.gnome.TextEditor
    "sublime_text",
    "sublime",
    "zed",
    "jetbrains",
    "idea",
    "pycharm",
    "clion",
    "goland",
    "rustrover",
    "libreoffice",
    "obsidian",
    "joplin",
    "notes",
    "mousepad",
    "geany",
    "kdevelop",
    "ghostwriter",
];

/// Classifies an app id (a desktop file name such as `com.mitchellh.ghostty`,
/// `google-chrome`, `org.kde.kate`). The id is split into lowercase tokens on
/// `.`, `-` and `_`, and matched token by token, so `code` matches
/// `code-url-handler` but not `vscode-like-tool`.
pub fn classify(app: &str) -> AppClass {
    let lower = app.to_ascii_lowercase();
    let tokens: Vec<&str> = lower.split(['.', '-', '_']).collect();
    let has = |table: &[&str]| tokens.iter().any(|t| table.contains(t));
    // Order matters only for ids that would match two tables.
    if has(TERMINALS) {
        AppClass::Terminal
    } else if has(EDITORS) {
        AppClass::Editor
    } else if has(LAUNCHERS) {
        AppClass::Launcher
    } else if has(CHATS) {
        AppClass::Chat
    } else if has(BROWSERS) {
        AppClass::Browser
    } else {
        AppClass::Generic
    }
}

/// The window part of a session key. Editors mark unsaved documents in the
/// title (`● main.rs - proj - Visual Studio Code`, `notes.txt * — Kate`), and
/// that marker flips on the first keystroke. Left in, it would split every
/// editing session in two, so it is removed.
pub fn normalize_window(title: &str) -> String {
    // Leading status glyphs: VS Code's unsaved dot, and the spinners and
    // markers that terminal programs animate in the title (Claude Code
    // flips ◐/◑ every second, others use braille spinners or ✳). Left in,
    // each frame would start a new session.
    let mut t = title
        .trim()
        .trim_start_matches(|c: char| is_status_glyph(c) || c.is_whitespace());
    if let Some(rest) = t.strip_prefix("* ") {
        t = rest.trim_start();
    }
    let mut out = t.replace(" [modified]", "").replace(" [Modified]", "");
    // A standalone "*" word (Kate/KDE: "file.txt * — Kate").
    while let Some(i) = out.find(" * ") {
        out.replace_range(i..i + 2, "");
    }
    if let Some(stripped) = out.strip_suffix(" *") {
        out = stripped.to_string();
    }
    out
}

/// Symbol characters used as status markers at the start of window titles:
/// geometric shapes (● ◐ ◑ ◒ ◓ ■ ▶), dingbats and miscellaneous symbols
/// (✳ ✻ ⚡ ★), and braille (spinner frames ⠋ ⠙ ⠹).
fn is_status_glyph(c: char) -> bool {
    matches!(c as u32, 0x25A0..=0x25FF | 0x2600..=0x27BF | 0x2800..=0x28FF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_real_app_ids() {
        assert_eq!(classify("com.mitchellh.ghostty"), AppClass::Terminal);
        assert_eq!(classify("org.kde.konsole"), AppClass::Terminal);
        assert_eq!(classify("org.gnome.Terminal"), AppClass::Terminal);
        assert_eq!(classify("org.gnome.Ptyxis"), AppClass::Terminal);
        assert_eq!(classify("org.gnome.Console"), AppClass::Terminal);
        // KDE's script console is not a shell: "interactiveconsole" is one token.
        assert_eq!(
            classify("org.kde.plasma-interactiveconsole"),
            AppClass::Generic
        );
        assert_eq!(classify("google-chrome"), AppClass::Browser);
        assert_eq!(classify("firefox_firefox"), AppClass::Browser);
        assert_eq!(classify("org.kde.kate"), AppClass::Editor);
        assert_eq!(classify("code"), AppClass::Editor);
        assert_eq!(classify("code-url-handler"), AppClass::Editor);
        assert_eq!(classify("com.slack.Slack"), AppClass::Chat);
        assert_eq!(classify("org.telegram.desktop"), AppClass::Chat);
        assert_eq!(classify("krunner"), AppClass::Launcher);
        assert_eq!(classify("org.kde.krunner"), AppClass::Launcher);
        assert_eq!(classify("org.kde.dolphin"), AppClass::Generic);
        assert_eq!(classify("unknown"), AppClass::Generic);
        assert_eq!(classify("synthetic"), AppClass::Generic);
        assert_eq!(
            classify("decoder"),
            AppClass::Generic,
            "token match, not substring"
        );
    }

    #[test]
    fn single_line_classes() {
        assert!(AppClass::Terminal.single_line());
        assert!(AppClass::Browser.single_line());
        assert!(AppClass::Chat.single_line());
        assert!(AppClass::Launcher.single_line());
        assert!(!AppClass::Editor.single_line());
        assert!(!AppClass::Generic.single_line());
    }

    #[test]
    fn strips_unsaved_markers() {
        assert_eq!(
            normalize_window("● main.rs - brain - Visual Studio Code"),
            "main.rs - brain - Visual Studio Code"
        );
        assert_eq!(
            normalize_window("events.jsonl * — Kate"),
            "events.jsonl — Kate"
        );
        assert_eq!(
            normalize_window("notes.txt [modified] — KWrite"),
            "notes.txt — KWrite"
        );
        assert_eq!(normalize_window("* draft.md"), "draft.md");
        assert_eq!(normalize_window("draft.md *"), "draft.md");
        assert_eq!(
            normalize_window("New tab - Google Chrome"),
            "New tab - Google Chrome"
        );
        assert_eq!(normalize_window("a * b"), "a b");
        // Animated title prefixes of terminal programs.
        for t in [
            "◐ Home assignment",
            "◑ Home assignment",
            "⠹ Home assignment",
            "✳ Home assignment",
        ] {
            assert_eq!(normalize_window(t), "Home assignment");
        }
        assert_eq!(normalize_window("~"), "~");
        assert_eq!(normalize_window("brain run"), "brain run");
        assert_eq!(normalize_window(""), "");
    }
}
