//! The `brain` command line: `brain run` captures until stopped, `brain query`
//! answers from the captured data and prints JSON. The arguments are defined
//! in [`cli`]; this file only dispatches them and reports errors (on stderr,
//! exit code 1). The work is in the library ([`brain::capture`],
//! [`brain::query`]).

mod cli;

use anyhow::Result;
use brain::datadir::DataDir;
use clap::Parser;
use cli::{Cli, Command, Query};
use std::process::ExitCode;

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
                    Query::Sessions {
                        app,
                        window,
                        time,
                        limit,
                        offset,
                    } => serde_json::to_string_pretty(&brain::query::sessions(
                        &data_dir,
                        app.as_deref(),
                        window.as_deref(),
                        &time.into(),
                        limit,
                        offset,
                        now,
                    )?)?,
                    Query::Apps { time } => serde_json::to_string_pretty(&brain::query::apps(
                        &data_dir,
                        &time.into(),
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
