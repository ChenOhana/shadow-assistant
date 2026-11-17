use anyhow::Result;
use brain::datadir::DataDir;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

/// A shadow assistant: captures what you type and makes it searchable.
#[derive(Parser)]
#[command(name = "brain", version, about)]
struct Cli {
    /// Directory holding the capture log and the index.
    #[arg(
        long,
        global = true,
        value_name = "DIR",
        default_value = "./brain-data"
    )]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Capture keystrokes and focus changes, group them into writing sessions
    /// with reconstructed, searchable text, until stopped (Ctrl+C / SIGTERM).
    Run {
        /// Test hook: type a built-in corpus at RATE keys/s instead of reading
        /// input devices (no root or desktop needed).
        #[arg(long, value_name = "RATE", hide = true)]
        synthetic: Option<f64>,
        /// Reconstruct VS Code sessions with the real editor: a git-style diff
        /// of the file when it was saved after the session, otherwise a
        /// best-effort replay of the keystrokes into a sandboxed, invisible
        /// VS Code (no network, capped CPU/memory). Keeps the generic
        /// reconstruction when neither works.
        #[arg(long)]
        vscode_replay: bool,
    },
    /// Query what has been captured; prints JSON, with times in local time.
    /// Safe to run while `brain run` is running.
    Query {
        #[command(subcommand)]
        query: Query,
    },
}

/// Accepted time forms, shown in `--help`.
const TIME_HELP: &str = "T is relative (-30s, -15m, -2h, -1d, -1h30m, now), \
RFC 3339 (2026-11-16T14:00:00Z, 2026-11-16T16:00:00+02:00), or local time \
(2026-11-16 14:00[:SS], 2026-11-16 for midnight, 14:00 for today).";

#[derive(clap::Args)]
struct TimeArgs {
    /// The 10 minutes ending at T.
    #[arg(long, value_name = "T", allow_hyphen_values = true, conflicts_with_all = ["from", "to"])]
    at: Option<String>,
    /// Start of the window (with no --to: up to now).
    #[arg(long, value_name = "T", allow_hyphen_values = true)]
    from: Option<String>,
    /// End of the window.
    #[arg(long, value_name = "T", allow_hyphen_values = true)]
    to: Option<String>,
}

impl From<TimeArgs> for brain::query::TimeFlags {
    fn from(t: TimeArgs) -> Self {
        Self {
            at: t.at,
            from: t.from,
            to: t.to,
        }
    }
}

#[derive(Subcommand)]
enum Query {
    /// Full-text search over writing sessions, newest first. Every word must
    /// appear in the same session, anywhere inside a word, in any case.
    /// Without time flags it searches all sessions; --to alone means up to T.
    #[command(after_help = TIME_HELP)]
    Search {
        /// One or more words.
        query: String,
        /// Only apps whose id contains A (case-insensitive), e.g. chrome.
        #[arg(long, value_name = "A")]
        app: Option<String>,
        #[command(flatten)]
        time: TimeArgs,
        /// At most N results.
        #[arg(long, value_name = "N", default_value_t = 20)]
        limit: usize,
    },
    /// What was typed into an app in the time range, reconstructed (edits
    /// applied) per window title with the app's rules; each window's text
    /// starts empty at the start of the range. Without time flags it covers
    /// the last 10 minutes; --to alone means the 10 minutes ending at T.
    #[command(after_help = TIME_HELP)]
    Text {
        /// The app: its full id or a part of it, case-insensitive (e.g. kate
        /// for org.kde.kate). It must match exactly one app; the output names
        /// it in full.
        #[arg(long, value_name = "A")]
        app: String,
        #[command(flatten)]
        time: TimeArgs,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("brain: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    let data_dir = DataDir::new(cli.data_dir);
    match cli.command {
        Command::Run {
            synthetic,
            vscode_replay,
        } => {
            let opts = brain::capture::RunOptions {
                synthetic_rate: synthetic,
                vscode_replay,
            };
            brain::capture::run(&data_dir, &opts)
        }
        Command::Query { query } => {
            data_dir.ensure_exists()?;
            let now = brain::timefmt::now_ms();
            let json =
                match query {
                    Query::Search {
                        query,
                        app,
                        time,
                        limit,
                    } => serde_json::to_string_pretty(&brain::query::search(
                        &data_dir,
                        &query,
                        app.as_deref(),
                        &time.into(),
                        limit,
                        now,
                    )?)?,
                    Query::Text { app, time } => serde_json::to_string_pretty(
                        &brain::query::text(&data_dir, &app, &time.into(), now)?,
                    )?,
                };
            println!("{json}");
            Ok(())
        }
    }
}
