use anyhow::{Result, bail};
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
    /// Capture keystrokes and focus changes until stopped (Ctrl+C / SIGTERM).
    Run {
        /// Test hook: type a built-in corpus at RATE keys/s instead of reading
        /// input devices (no root or desktop needed).
        #[arg(long, value_name = "RATE", hide = true)]
        synthetic: Option<f64>,
        /// Reconstruct VS Code sessions by replaying their keystrokes into a
        /// sandboxed, invisible VS Code (no network, capped CPU/memory).
        /// Falls back to generic reconstruction when it cannot.
        #[arg(long)]
        vscode_replay: bool,
    },
    /// Query what has been captured. Safe to run while `brain run` is running.
    Query {
        #[command(subcommand)]
        query: Query,
    },
}

#[derive(Subcommand)]
enum Query {
    /// Full-text search over writing sessions.
    Search {
        /// One or more words; all must appear in the same session (case-insensitive).
        query: String,
        #[arg(long, value_name = "A")]
        app: Option<String>,
        #[arg(long, value_name = "T")]
        from: Option<String>,
        #[arg(long, value_name = "T")]
        to: Option<String>,
        #[arg(long, value_name = "N", default_value_t = 20)]
        limit: usize,
    },
    /// The raw typed character stream of an app over a time window.
    Text {
        #[arg(long, value_name = "A")]
        app: String,
        /// End of a 10-minute window (default: now).
        #[arg(long, value_name = "T", conflicts_with_all = ["from", "to"])]
        at: Option<String>,
        #[arg(long, value_name = "T")]
        from: Option<String>,
        #[arg(long, value_name = "T")]
        to: Option<String>,
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
        Command::Query { .. } => {
            data_dir.ensure_exists()?;
            bail!("`brain query` is not implemented yet (stage 2)")
        }
    }
}
