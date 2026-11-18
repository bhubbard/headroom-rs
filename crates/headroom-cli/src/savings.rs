//! `headroom savings` — display lifetime/session savings and Unicode progress bars.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Args;
use headroom_proxy::observability::ledger::Ledger;
use serde_json::Value;

use crate::workspace::default_stats_path;

/// Arguments for `headroom savings`.
#[derive(Debug, Args)]
pub struct SavingsArgs {
    /// Proxy port to query `/stats` when `--stats-path` is not forced.
    #[arg(long, default_value_t = 8787)]
    pub port: u16,

    /// Read statistics directly from a `native_stats.json` file.
    #[arg(long)]
    pub stats_path: Option<PathBuf>,

    /// Display per-model lifetime breakdown.
    #[arg(long, default_value_t = false)]
    pub by_model: bool,

    /// Print raw `/stats` JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Render a fixed-width Unicode progress bar using `█` and `░`.
#[must_use]
pub fn render_bar(percent: f64, width: usize) -> String {
    let clamped = if percent.is_finite() {
        percent.clamp(0.0, 100.0)
    } else {
        0.0
    };
    let filled = ((clamped / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

async fn load_stats(args: &SavingsArgs) -> Result<Value> {
    if let Some(ref path) = args.stats_path {
        return Ok(Ledger::load(path.clone()).stats_payload(true));
    }

    let url = format!("http://127.0.0.1:{}/stats", args.port);
    if let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    {
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                if let Ok(val) = resp.json::<Value>().await {
                    return Ok(val);
                }
            }
        }
    }

    match default_stats_path() {
        Some(path) => Ok(Ledger::load(path).stats_payload(true)),
        None => Ok(Ledger::in_memory().stats_payload(true)),
    }
}

/// Execute `headroom savings`.
pub async fn run(args: SavingsArgs) -> Result<()> {
    let stats = load_stats(&args).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
        return Ok(());
    }

    let lifetime = &stats["lifetime"];
    let session = &stats["session"];

    let l_req = lifetime["requests"].as_u64().unwrap_or(0);
    let l_comp = lifetime["compressed"].as_u64().unwrap_or(0);
    let l_saved = lifetime["tokens_saved"].as_u64().unwrap_or(0);
    let l_pct = lifetime["compression_percent"].as_f64().unwrap_or(0.0);
    let l_usd = lifetime["savings_usd"].as_f64().unwrap_or(0.0);
    let l_cache_usd = lifetime["cache_savings_usd"].as_f64().unwrap_or(0.0);

    let s_req = session["requests"].as_u64().unwrap_or(0);
    let s_saved = session["tokens_saved"].as_u64().unwrap_or(0);
    let s_pct = session["compression_percent"].as_f64().unwrap_or(0.0);

    println!("Headroom Token & Cost Savings");
    println!(
        "  Lifetime: {} req ({} compressed) | {} tokens saved | compression [{bar}] {l_pct:.1}% | \
         saved ${:.4} (comp ${l_usd:.4}, cache ${l_cache_usd:.4})",
        l_req,
        l_comp,
        l_saved,
        l_usd + l_cache_usd,
        bar = render_bar(l_pct, 20)
    );
    println!(
        "  Session : {} req | {} tokens saved | compression [{bar}] {s_pct:.1}%",
        s_req,
        s_saved,
        bar = render_bar(s_pct, 20)
    );

    if args.by_model {
        if let Some(models) = stats.get("lifetime_by_model").and_then(Value::as_object) {
            if !models.is_empty() {
                println!("  By Model:");
                for (model, row) in models {
                    let reqs = row["requests"].as_u64().unwrap_or(0);
                    let saved = row["tokens_saved"].as_u64().unwrap_or(0);
                    let pct = row["compression_percent"].as_f64().unwrap_or(0.0);
                    println!(
                        "    {:<32} {:>6} req | {:>9} saved | [{}] {pct:.1}%",
                        model,
                        reqs,
                        saved,
                        render_bar(pct, 16)
                    );
                }
            }
        }
    }

    Ok(())
}
