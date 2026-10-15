mod admin;
mod app;
mod auth;
mod backend;
mod backup;
mod cli;
mod config;
mod db;
mod hook;
mod names;
mod proto;
mod protocol;
mod serve;
mod service;
mod view;

use std::fs::OpenOptions;
use std::process::ExitCode;
use std::sync::Mutex;

use clap::Parser;
use tracing_subscriber::EnvFilter;

/// Log to ~/bbs.log, or to stderr when `BBS_LOG_STDERR=1` (containers). Logging is
/// best-effort: failure to open the file never blocks the CLI.
fn init_logging() {
    let filter = EnvFilter::try_from_env("BBS_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter);
    if std::env::var_os("BBS_LOG_STDERR").is_some_and(|v| v == "1") {
        fmt.with_writer(std::io::stderr).init();
        return;
    }
    let Ok(home) = app::home_dir() else { return };
    let Ok(file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("bbs.log"))
    else {
        return;
    };
    fmt.with_writer(Mutex::new(file)).init();
}

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();
    let cli = cli::Cli::parse();
    match app::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            eprintln!("bbs: {err:#}");
            ExitCode::FAILURE
        }
    }
}
