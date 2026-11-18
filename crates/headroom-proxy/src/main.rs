//! headroom-proxy: transparent reverse proxy binary for LLM APIs.

#![forbid(unsafe_code)]

use clap::Parser;
use headroom_proxy::config::CliArgs;
use headroom_proxy::{init_tracing, run_server, BindPolicyError, Config};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = CliArgs::parse();
    let config = Config::from_cli(args);

    init_tracing(&config.log_level);

    if let Err(err) = run_server(config).await {
        if let Some(bind_err) = err.downcast_ref::<BindPolicyError>() {
            eprintln!("headroom-proxy: {bind_err}");
            std::process::exit(2);
        }
        return Err(err);
    }

    Ok(())
}
