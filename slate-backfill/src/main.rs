use std::{fs::File, io::Read, path::PathBuf, str::FromStr, sync::Arc};

use anyhow::Context;
use clap::Parser;
use slate_common::config::Config;
use slate_replay::{
    backfill::{AccountStoreChoice, backfill},
    block::{Block, current_slot, fetch_block, fetch_confirmed_slots, sanitize},
    snapshot::{read_manifest_hashes, read_manifest_lt_hash},
    source::{BlockSource, RpcBlockSource},
};
use slate_store::ClickHouseClient;
use solana_pubkey::Pubkey;

#[derive(Parser)]
#[command(about = "Reconstruct a program's historical account state by replaying a slot range")]
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
    /// Account store: `memory` (RAM, small ranges) or `disk` (redb, large ranges).
    #[arg(long, default_value = "memory")]
    store: String,
    /// Disk store's redb page-cache size in bytes, its whole RAM budget. Default 8 GiB.
    #[arg(long, default_value_t = 8 * 1024 * 1024 * 1024)]
    cache_size: usize,
    /// Path for the disk store's redb file.
    #[arg(long, default_value = "slate-accounts.redb")]
    store_path: String,
    /// Block cache path (redb). Point runs of the same cluster at one file to skip
    /// re-fetching on retries. Omit to disable.
    #[arg(long)]
    block_cache: Option<String>,
    /// Replay this many slots per chunk before flushing writes to ClickHouse and
    /// clearing the log. Bounds write-log RAM over long ranges.
    #[arg(long, default_value_t = 2000)]
    chunk_slots: usize,
    /// After replaying, diff the reconstructed end-state against this snapshot (the real
    /// snapshot at --to) byte-for-byte over the footprint, the data-fidelity proof the
    /// per-tx oracle can't give. Optional. Prints the diff and exits non-zero if the
    /// end-state isn't byte-exact, so a silent divergence can't pass.
    #[arg(long)]
    verify_boundary: Option<String>,
    /// How many blocks to fetch concurrently. 1 (serial) suits a rate-limited RPC like
    /// Helius; raise it (16-32) for an unmetered local yellowstone-faithful.
    #[arg(long, default_value_t = 1)]
    fetch_concurrency: usize,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Preflight: fail fast on cheap local checks before fetching a single block.
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

    // Validate program, snapshot, config, and ClickHouse before the expensive fetch.
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

    // Bootstrap the bank-hash roll from the manifest (lattice + bank hash at s_snap) so SlotHashes rolls real hashes.
    let manifest = read_manifest_hashes(
        File::open(snapshot_path).with_context(|| format!("opening snapshot {snapshot_path}"))?,
        args.from,
    )
    .context("reading the snapshot manifest bank hash")?;
    let lt_hash = read_manifest_lt_hash(
        File::open(snapshot_path).with_context(|| format!("opening snapshot {snapshot_path}"))?,
        args.from,
    )
    .context("reading the snapshot manifest lattice hash")?
    .context("snapshot has no accounts_lt_hash (a pre-lattice snapshot?)")?;
    let bootstrap = Some((lt_hash, manifest.bank_hash));

    let snapshot =
        File::open(snapshot_path).with_context(|| format!("opening snapshot {snapshot_path}"))?;

    let account_store = match args.store.as_str() {
        "memory" => AccountStoreChoice::Memory,
        "disk" => AccountStoreChoice::Disk {
            path: args.store_path.clone().into(),
            cache_bytes: args.cache_size,
        },
        other => anyhow::bail!("--store must be `memory` or `disk`, got `{other}`"),
    };

    // Seed, replay, persist; async because the store writes are.
    tokio::runtime::Runtime::new()?.block_on(async {
        let store = ClickHouseClient::with_config(
            &cfg.clickhouse.url,
            &cfg.clickhouse.database,
            &cfg.clickhouse.user,
            &cfg.clickhouse.password,
        );
        // RPC-backed source; the trait is the seam, other providers drop in without touching backfill.
        let source: Arc<dyn BlockSource> =
            Arc::new(RpcBlockSource::new(&args.rpc).with_concurrency(args.fetch_concurrency));
        // Optional byte-exact end-state check against the snapshot at --to.
        let verify_end: Option<Box<dyn Read>> = match &args.verify_boundary {
            Some(path) => Some(Box::new(
                File::open(path).with_context(|| format!("opening boundary snapshot {path}"))?,
            )),
            None => None,
        };
        let result = backfill(
            snapshot,
            args.from,
            source,
            args.block_cache.as_ref().map(PathBuf::from),
            args.from,
            args.to,
            &program,
            &store,
            bootstrap,
            account_store,
            args.chunk_slots,
            verify_end,
        )
        .await?;
        match &result.replay.halt {
            None => println!(
                "done: {} blocks replayed and persisted; coverage ({}, {}]",
                result.replay.blocks_completed, args.from, args.to
            ),
            Some((slot, block_replay)) => {
                let detail = block_replay
                    .halt
                    .as_ref()
                    .map(|h| format!("tx {}: {}", h.tx_index, h.reason))
                    .unwrap_or_else(|| "unknown reason".into());
                println!(
                    "halted at slot {slot} after {} completed blocks, on {detail}; \
                     coverage recorded up to the last good slot",
                    result.replay.blocks_completed
                );
            }
        }
        // Boundary verdict: hard failure (non-zero exit) on any mismatch so a divergence can't slip through silently.
        if let Some(diff) = &result.boundary {
            println!("{}", diff.summary());
            for m in diff.mismatches.iter().take(20) {
                println!("  mismatch {} {:?}", m.pubkey, m.kind);
            }
            if !diff.is_exact() {
                anyhow::bail!(
                    "boundary diff: {} mismatch(es); reconstructed end-state is NOT byte-exact vs the snapshot at --to",
                    diff.mismatches.len()
                );
            }
            println!("boundary diff: end-state is byte-exact against the snapshot at --to");
        }
        anyhow::Ok(())
    })
}

// Fetch confirmed blocks in (from, to]; blocking, so it must run before any tokio runtime (fetch_block panics nested).
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

// Check the snapshot exists and has the zstd magic, so a wrong path fails now, not deep in the loader.
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

// Ping ClickHouse so a down store fails before the range is fetched.
fn check_clickhouse(url: &str) -> anyhow::Result<()> {
    let ping = format!("{}/ping", url.trim_end_matches('/'));
    reqwest::blocking::get(&ping)
        .with_context(|| format!("ClickHouse unreachable at {url}"))?
        .error_for_status()
        .with_context(|| format!("ClickHouse at {url} responded with an error"))?;
    Ok(())
}

// Report the range and whether every tx sanitizes, no snapshot/execution; a range that would halt shows up here first.
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
