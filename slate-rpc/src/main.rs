use clap::Parser;
use slate_common::config::Config;
use slate_rpc::{Rpc, SlateRpcServer};
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

    let store = ClickHouseClient::with_config(
        &cfg.clickhouse.url,
        &cfg.clickhouse.database,
        &cfg.clickhouse.user,
        &cfg.clickhouse.password,
    );

    let server = jsonrpsee::server::ServerBuilder::default()
        .build(cfg.rpc.bind.as_str())
        .await?;
    let handle = server.start(Rpc { store }.into_rpc());

    handle.stopped().await;
    Ok(())
}
