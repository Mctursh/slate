//! slate-backfill: reconstruct one program's historical account state.
//!
//! Given a full snapshot and a slot range, replay every block in the range
//! through the SVM (seeded from the snapshot) and write the program's per-slot
//! account history into ClickHouse. This is the one-command entry point over the
//! `slate_replay::backfill` engine.
//!
//! `--dry-run` skips the snapshot and execution: it fetches, parses, and
//! sanitizes the range and reports what it looks like, so a target range can be
//! validated before committing to a large snapshot download.

use std::{fs::File, str::FromStr};

use anyhow::Context;
use clap::Parser;
use slate_common::config::Config;
use slate_replay::{
    backfill::backfill,
    block::{Block, fetch_block, fetch_confirmed_slots, sanitize},
};
use slate_store::ClickHouseClient;
use solana_pubkey::Pubkey;

#[derive(Parser)]
#[command(
    about = "Reconstruct a program's historical account state by replaying a slot range"
)]
struct Args {
    /// Path to the full snapshot the range starts from (omit with --dry-run).
    #[arg(required_unless_present = "dry_run")]
    snapshot: Option<String>,
    /// Slot the snapshot was taken at. Replay covers (from, to].
    #[arg(long)]
    from: u64,
    /// Last slot to replay, inclusive.
    #[arg(long)]
    to: u64,
    /// Program whose accounts to index, base58 (omit with --dry-run).
    #[arg(long, required_unless_present = "dry_run")]
    program: Option<String>,
    /// JSON-RPC URL to fetch blocks from (getBlocks + getBlock).
    #[arg(long)]
    rpc: String,
    /// Slate config file, for the ClickHouse connection.
    #[arg(long, default_value = "slate.toml")]
    config: String,
    /// Fetch, parse, and sanitize the range without a snapshot or execution, then
    /// report what it looks like. A preflight to run before downloading a snapshot.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

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

    if args.dry_run {
        return dry_run_report(&blocks);
    }

    let snapshot_path = args
        .snapshot
        .context("<snapshot> is required for a real run")?;
    let program_str = args
        .program
        .context("--program is required for a real run")?;
    let program = Pubkey::from_str(&program_str)
        .with_context(|| format!("invalid program pubkey {program_str}"))?;
    let cfg = Config::load(&args.config)?;
    let snapshot = File::open(&snapshot_path)
        .with_context(|| format!("opening snapshot {snapshot_path}"))?;

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

/// Report what the fetched range looks like and whether every transaction
/// sanitizes, without a snapshot or any execution. This exercises the whole
/// pre-execution path (getBlock parse + v0/ALT resolution + sanitize) on real
/// blocks, so a range that would halt the real run shows up here first.
fn dry_run_report(blocks: &[Block]) -> anyhow::Result<()> {
    let mut total = 0usize;
    let (mut v0_alt, mut failed, mut token) = (0usize, 0usize, 0usize);
    let mut sanitize_failures = Vec::new();

    for block in blocks {
        for (i, tx) in block.transactions.iter().enumerate() {
            total += 1;
            let loaded = &tx.meta.loaded_addresses;
            if !loaded.writable.is_empty() || !loaded.readonly.is_empty() {
                v0_alt += 1;
            }
            if tx.meta.err.is_some() {
                failed += 1;
            }
            if !tx.meta.post_token_balances.is_empty() {
                token += 1;
            }
            if let Err(e) = sanitize(&tx.transaction, loaded) {
                sanitize_failures.push(format!("slot {} tx {i}: {e}", block.slot));
            }
        }
    }

    println!("dry run: {} blocks, {total} transactions", blocks.len());
    println!("  v0+ALT (lookup-loaded): {v0_alt}");
    println!("  chain-failed:           {failed}");
    println!("  token-touching:         {token}");
    if sanitize_failures.is_empty() {
        println!("  sanitize: all {total} transactions sanitized cleanly");
    } else {
        println!("  sanitize: {} FAILED", sanitize_failures.len());
        for f in &sanitize_failures {
            println!("    {f}");
        }
    }
    Ok(())
}
