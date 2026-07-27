//! snapshot loader — load a Solana snapshot's accounts (filtered by owner) into Slate as a
//! baseline. A dev/bootstrap tool; the live path is the `live` binary.
//!
//!   cargo run -p slate-ingest -- <snapshot_accounts_dir> [--owner <base58>] [--config slate.toml]

use std::str::FromStr;

use clap::Parser;
use slate_common::config::Config;
use slate_ingest::read_snapshot_accounts;
use slate_store::ClickHouseClient;
use solana_pubkey::Pubkey;

#[derive(Parser)]
struct Args {
    /// Directory of snapshot account files (named `<slot>.<id>`).
    snapshot_dir: String,
    /// Owner (program) pubkey to filter to, base58. Defaults to the System program.
    #[arg(long, default_value = "11111111111111111111111111111111")]
    owner: String,
    /// Path to the config file.
    #[arg(long, default_value = "slate.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    let owner = Pubkey::from_str(&args.owner)?.to_bytes();

    let store = ClickHouseClient::with_config(
        &cfg.clickhouse.url,
        &cfg.clickhouse.database,
        &cfg.clickhouse.user,
        &cfg.clickhouse.password,
    );

    let s_snap = read_snapshot_accounts(&store, &args.snapshot_dir, &owner).await?;
    println!("snapshot slot is {s_snap}");
    Ok(())
}
