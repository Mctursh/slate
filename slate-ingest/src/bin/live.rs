//! live — bootstrap the baseline then capture the live stream into Slate.
//!
//! Config comes from a TOML file (default `slate.toml`, override with `--config`). The gRPC auth
//! token is read from `GRPC_TOKEN` so the secret stays out of the config file.
//!
//!   GRPC_TOKEN=<token> cargo run -p slate-ingest --bin live -- --config slate.toml

use anyhow::Context;
use clap::Parser;
use slate_common::config::Config;
use slate_ingest::baseline::fetch_program_baseline;
use slate_ingest::capture::Capturer;
use slate_ingest::yellowstone::{self, IngestConfig};
use slate_store::ClickHouseClient;

#[derive(Parser)]
struct Args {
    /// Path to the config file.
    #[arg(long, default_value = "slate.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    let ingest = cfg.ingest.context("config is missing the [ingest] section (required by `live`)")?;

    // Auth token: from config ([ingest].x-token), overridden by GRPC_TOKEN env if set (CI/containers).
    let token = std::env::var("GRPC_TOKEN").ok().or_else(|| ingest.x_token.clone());

    let stream_cfg = IngestConfig {
        endpoint: ingest.grpc_endpoint.clone(),
        x_token: token.clone(),
        owners: vec![ingest.program.clone()],
    };

    // Baseline RPC: use the configured one, else default to Helius devnet built from GRPC_TOKEN.
    let rpc_url = ingest.baseline_rpc.clone().unwrap_or_else(|| {
        format!(
            "https://devnet.helius-rpc.com/?api-key={}",
            token.as_deref().unwrap_or_default()
        )
    });

    let store = ClickHouseClient::with_config(
        &cfg.clickhouse.url,
        &cfg.clickhouse.database,
        &cfg.clickhouse.user,
        &cfg.clickhouse.password,
    );

    // Baseline: fetch the program's full account set now, stamp it as the floor.
    let s_snap = fetch_program_baseline(&store, &rpc_url, &ingest.program).await?;
    println!("baseline loaded at slot {s_snap}");

    // Stream forward from just after the baseline — contiguous, no gap.
    let mut capturer = Capturer::from_baseline(store, s_snap);
    yellowstone::run(&stream_cfg, &mut capturer, Some(s_snap + 1)).await
}
