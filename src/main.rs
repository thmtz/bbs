mod app;
mod cli;
mod db;
mod hook;
mod names;
mod view;

use std::fs::OpenOptions;
use std::process::ExitCode;
use std::sync::Mutex;

use clap::Parser;
use tracing_subscriber::EnvFilter;

/// Log to ~/bbs.log. Logging is best-effort: failure to open the file never blocks the CLI.
fn init_logging() {
    let Ok(home) = app::home_dir() else { return };
    let Ok(file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("bbs.log"))
    else {
        return;
    };
    let filter = EnvFilter::try_from_env("BBS_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .with_env_filter(filter)
        .init();
}

fn main() -> ExitCode {
    init_logging();
    let cli = cli::Cli::parse();
    match app::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            eprintln!("bbs: {err:#}");
            ExitCode::FAILURE
        }
    }
}
