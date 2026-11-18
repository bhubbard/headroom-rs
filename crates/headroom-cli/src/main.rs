//! `headroom` binary entrypoint.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use headroom_cli::{dispatch, Cli};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("headroom error: {err:#}");
            ExitCode::from(1)
        }
    }
}
