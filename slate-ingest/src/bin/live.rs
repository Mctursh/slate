use anyhow::Context;
use clap::Parser;
use slate_common::config::Config;
use slate_ingest::baseline::fetch_program_baseline;
use slate_ingest::capture::Capturer;
use slate_ingest::yellowstone::{self, IngestConfig};
use slate_store::ClickHouseClient;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "slate.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    let ingest = cfg.ingest.context("config is missing the [ingest] section (required by `live`)")?;

    let token = std::env::var("GRPC_TOKEN").ok().or_else(|| ingest.x_token.clone());

    let stream_cfg = IngestConfig {
        endpoint: ingest.grpc_endpoint.clone(),
        x_token: token.clone(),
        owners: vec![ingest.program.clone()],
        max_decoding_bytes: ingest.grpc_max_decoding_bytes,
    };

    let store = ClickHouseClient::with_config(
        &cfg.clickhouse.url,
        &cfg.clickhouse.database,
        &cfg.clickhouse.user,
        &cfg.clickhouse.password,
    );

    let s_snap = fetch_program_baseline(&store, &ingest.baseline_rpc, &ingest.program).await?;
    println!("baseline loaded at slot {s_snap}");

    let mut capturer = Capturer::from_baseline(store, s_snap);
    yellowstone::run(&stream_cfg, &mut capturer, Some(s_snap + 1)).await
}
