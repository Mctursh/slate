//! slate-backfill: reconstruct one program's historical account state.
//!
//! Given a full snapshot and a slot range, replay every block in the range
//! through the SVM (seeded from the snapshot) and write the program's per-slot
//! account history into ClickHouse. This is the one-command entry point over the
//! `slate_replay::backfill` engine.

use std::{fs::File, str::FromStr};

use anyhow::Context;
use clap::Parser;
use slate_common::config::Config;
use slate_replay::{
    backfill::backfill,
    block::{Block, fetch_block, fetch_confirmed_slots},
};
use slate_store::ClickHouseClient;
use solana_pubkey::Pubkey;

#[derive(Parser)]
#[command(
    about = "Reconstruct a program's historical account state by replaying a slot range"
)]
struct Args {
    /// Path to the full snapshot the range starts from.
    snapshot: String,
    /// Slot the snapshot was taken at. Replay covers (from, to].
    #[arg(long)]
    from: u64,
    /// Last slot to replay, inclusive.
    #[arg(long)]
    to: u64,
    /// Program whose accounts to index (base58 pubkey).
    #[arg(long)]
    program: String,
    /// JSON-RPC URL to fetch blocks from (getBlocks + getBlock).
    #[arg(long)]
    rpc: String,
    /// Slate config file, for the ClickHouse connection.
    #[arg(long, default_value = "slate.toml")]
    config: String,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    let program = Pubkey::from_str(&args.program)
        .with_context(|| format!("invalid program pubkey {}", args.program))?;

    // Fetch the range's blocks up front, before any async runtime exists:
    // fetch_block uses a blocking HTTP client that panics nested inside tokio.
    // getBlocks first, so we only request slots that actually produced a block.
    let slots = fetch_confirmed_slots(&args.rpc, args.from + 1, args.to)?;
    println!(
        "fetching {} blocks in ({}, {}]",
        slots.len(),
        args.from,
        args.to
    );
    let blocks = slots
        .into_iter()
        .map(|slot| fetch_block(&args.rpc, slot))
        .collect::<anyhow::Result<Vec<Block>>>()?;

    let snapshot = File::open(&args.snapshot)
        .with_context(|| format!("opening snapshot {}", args.snapshot))?;

    // Seed, replay, and persist. This part is async because the store writes are.
    tokio::runtime::Runtime::new()?.block_on(async {
        let store = ClickHouseClient::with_config(
            &cfg.clickhouse.url,
            &cfg.clickhouse.database,
            &cfg.clickhouse.user,
            &cfg.clickhouse.password,
        );
        let result = backfill(snapshot, args.from, &blocks, &program, &store).await?;
        match &result.halt {
            None => println!(
                "done: {} blocks replayed and persisted; coverage ({}, {}]",
                result.blocks_completed, args.from, args.to
            ),
            Some((slot, _)) => println!(
                "halted at slot {slot} after {} completed blocks; \
                 coverage recorded up to the last good slot",
                result.blocks_completed
            ),
        }
        anyhow::Ok(())
    })
}
