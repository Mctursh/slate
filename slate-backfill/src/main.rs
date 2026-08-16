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

use std::{fs::File, io::Read, str::FromStr};

use anyhow::Context;
use clap::Parser;
use slate_common::config::Config;
use slate_replay::{
    backfill::backfill,
    block::{Block, current_slot, fetch_block, fetch_confirmed_slots, sanitize},
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

    // Preflight: fail fast on cheap local checks and unreachable services before
    // fetching a single block, so a typo or a down dependency costs seconds rather
    // than an hour into a run.
    if args.from >= args.to {
        anyhow::bail!(
            "--from ({}) must be below --to ({}); the range (from, to] would be empty",
            args.from,
            args.to
        );
    }
    let head =
        current_slot(&args.rpc).with_context(|| format!("RPC unreachable at {}", args.rpc))?;
    if args.to > head {
        anyhow::bail!(
            "--to ({}) is past the chain head ({head}); pick slots that exist",
            args.to
        );
    }

    if args.dry_run {
        let blocks = fetch_range(&args)?;
        return dry_run_report(&blocks);
    }

    // A real run validates the program, snapshot, config, and ClickHouse before the
    // expensive fetch, so none of them can fail an hour in.
    let program_str = args
        .program
        .as_ref()
        .context("--program is required for a real run")?;
    let program = Pubkey::from_str(program_str)
        .with_context(|| format!("invalid program pubkey {program_str}"))?;
    let snapshot_path = args
        .snapshot
        .as_ref()
        .context("<snapshot> is required for a real run")?;
    check_snapshot_file(snapshot_path)?;
    let cfg = Config::load(&args.config)?;
    check_clickhouse(&cfg.clickhouse.url)?;
    println!("preflight ok: RPC, program, snapshot, config, and ClickHouse all check out");

    let blocks = fetch_range(&args)?;
    let snapshot =
        File::open(snapshot_path).with_context(|| format!("opening snapshot {snapshot_path}"))?;

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

/// Fetch every confirmed block in `(from, to]`. Blocking, so it must run before
/// any tokio runtime exists (`fetch_block` panics nested inside one).
fn fetch_range(args: &Args) -> anyhow::Result<Vec<Block>> {
    let slots = fetch_confirmed_slots(&args.rpc, args.from + 1, args.to)?;
    println!(
        "fetching {} blocks in ({}, {}]",
        slots.len(),
        args.from,
        args.to
    );
    slots
        .into_iter()
        .map(|slot| fetch_block(&args.rpc, slot))
        .collect()
}

/// Confirm the snapshot exists and starts with the zstd frame magic, so a wrong
/// path or a non-`.tar.zst` file fails now instead of deep in the loader.
fn check_snapshot_file(path: &str) -> anyhow::Result<()> {
    let mut file = File::open(path).with_context(|| format!("cannot open snapshot {path}"))?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)
        .with_context(|| format!("snapshot {path} is too small to be a .tar.zst"))?;
    // zstd frame magic number, little-endian 0xFD2FB528.
    if magic != [0x28, 0xB5, 0x2F, 0xFD] {
        anyhow::bail!("snapshot {path} is not a zstd stream; expected a .tar.zst full snapshot");
    }
    Ok(())
}

/// Probe ClickHouse's HTTP `/ping` endpoint so a down or misconfigured store
/// fails before the range is fetched and replayed.
fn check_clickhouse(url: &str) -> anyhow::Result<()> {
    let ping = format!("{}/ping", url.trim_end_matches('/'));
    reqwest::blocking::get(&ping)
        .with_context(|| format!("ClickHouse unreachable at {url}"))?
        .error_for_status()
        .with_context(|| format!("ClickHouse at {url} responded with an error"))?;
    Ok(())
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
