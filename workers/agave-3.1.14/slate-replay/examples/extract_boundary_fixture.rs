//! Builds a boundary fixture from a pre-boundary store, without replaying anything.
//!
//! Usage:
//!   extract_boundary_fixture <store.redb> <snapshot.tar.zst> <snapshot_slot> <boundary_slot>
//!                            <rpc_url> <out.slfix>
//!
//! Refuses to write unless the fixture replays to the hash mainnet's own votes committed to.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use slate_format::fixture::{Fixture, RewardInputsRecord};
use slate_replay::fixture_capture::replay_fixture;
use slate_replay::source::{BlockSource, RpcBlockSource};
use slate_replay::store::{AccountStore, DiskStore};
use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;

const VOTE_LOOKAHEAD: u64 = 64;

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() != 6 {
        bail!(
            "usage: <store.redb> <snapshot.tar.zst> <snapshot_slot> <boundary_slot> <rpc_url> <out.slfix>"
        );
    }
    let (store_path, snap_path, out_path) = (&a[0], &a[1], &a[5]);
    let snapshot_slot: u64 = a[2].parse().context("snapshot_slot")?;
    let boundary_slot: u64 = a[3].parse().context("boundary_slot")?;
    let rpc = &a[4];

    let store = DiskStore::create(store_path, 2 * 1024 * 1024 * 1024)?;
    let raw = store
        .read_checkpoint()
        .context("store has no checkpoint; it was never replayed into")?;
    let checkpoint =
        slate_format::Checkpoint::decode(&raw).map_err(|e| anyhow::anyhow!("checkpoint: {e}"))?;
    let roll = checkpoint
        .roll
        .as_ref()
        .context("checkpoint has no bank-hash roll state")?;
    if checkpoint.slot + 1 != boundary_slot {
        bail!(
            "store is checkpointed at {}, so it is not the state just before {boundary_slot}",
            checkpoint.slot
        );
    }
    eprintln!(
        "checkpoint at {}: capitalization {}, {} stake delegations",
        checkpoint.slot,
        checkpoint.capitalization,
        checkpoint.stake_keys.len()
    );

    let manifest = slate_replay::snapshot::read_manifest_fields(
        std::fs::File::open(snap_path).with_context(|| format!("opening {snap_path}"))?,
        snapshot_slot,
    )
    .context("reading manifest fields")?;
    let inflation = manifest
        .inflation
        .context("snapshot manifest carries no inflation")?;
    let (vote_accounts, _) = slate_replay::snapshot::read_manifest_stakes_cache(
        std::fs::File::open(snap_path)?,
        snapshot_slot,
    )
    .context("reading manifest stakes cache")?;
    eprintln!("manifest: {} vote accounts", vote_accounts.len());

    let source = RpcBlockSource::new(rpc.clone());
    // confirmed_slots is (from, to], so start one below to include the boundary slot itself.
    let slots = source
        .confirmed_slots(boundary_slot - 1, boundary_slot + VOTE_LOOKAHEAD)
        .context("listing confirmed slots")?;
    eprintln!(
        "{} blocks in {boundary_slot}..={}",
        slots.len(),
        boundary_slot + VOTE_LOOKAHEAD
    );
    let blocks = source.fetch(&slots).context("fetching blocks")?;
    let boundary_block = blocks
        .iter()
        .find(|b| b.slot == boundary_slot)
        .context("the boundary slot has no block")?;
    let vote_hash = blocks
        .iter()
        .flat_map(slate_replay::block::vote_confirmations)
        .find(|(s, _)| *s == boundary_slot)
        .map(|(_, h)| h)
        .with_context(|| {
            format!("no vote confirming {boundary_slot} within {VOTE_LOOKAHEAD} slots")
        })?;
    eprintln!("consensus vote for {boundary_slot}: {vote_hash}");

    let mut keys: HashSet<Pubkey> =
        slate_replay::block::footprint(std::slice::from_ref(boundary_block));
    keys.extend(agave_feature_set::FEATURE_NAMES.keys().copied());
    keys.extend(
        checkpoint
            .stake_keys
            .iter()
            .map(|k| Pubkey::new_from_array(*k)),
    );
    keys.extend(vote_accounts.iter().copied());
    // Migration targets. footprint_fixed seeds the source buffers but not these: a real replay
    // always has them because transactions touch them, a lone boundary block may not.
    keys.insert(slate_replay::compat::core_bpf::SPL_TOKEN_PROGRAM_ID);
    keys.insert(solana_sdk_ids::stake::id());
    let programdata = slate_replay::block::programdata_addresses(&keys);
    keys.extend(programdata);

    // The store holds only what the replay touched. The stake and vote accounts a crossing needs
    // are seeded from the snapshot at boundary time, so anything absent has to come from there.
    // Everything the store holds, not a subset: the bank at the boundary carries the whole
    // replayed footprint, and an account written there but seeded absent mixes out as zero.
    let mut resolved: Vec<(Pubkey, (solana_account::AccountSharedData, u64))> = Vec::new();
    let mut in_store: HashSet<Pubkey> = HashSet::new();
    let mut dead_in_store: HashSet<Pubkey> = HashSet::new();
    store.scan(&mut |key, account| {
        in_store.insert(key);
        if account.lamports() == 0 {
            dead_in_store.insert(key);
            return;
        }
        resolved.push((key, (account.clone(), checkpoint.slot)));
    });
    // A dead row may be a stale tombstone, not a real close: re-resolve it against the snapshot.
    let mut missing: HashSet<Pubkey> = keys.difference(&in_store).copied().collect();
    missing.extend(dead_in_store.iter().copied());
    eprintln!(
        "{} live accounts from the store, {} dead rows to re-resolve, {} to load from the snapshot",
        resolved.len(),
        dead_in_store.len(),
        missing.len()
    );
    // stake_keys marks a crossing, which keeps every stake and vote account the manifest cache missed.
    let mut crossing_stakes: HashSet<Pubkey> = HashSet::new();
    if !missing.is_empty() {
        let from_snapshot = slate_replay::snapshot::load_accounts_with_stakes(
            std::fs::File::open(snap_path)?,
            Some(&missing),
            None,
            Some(&mut crossing_stakes),
        )
        .context("loading the crossing's accounts from the snapshot")?;
        eprintln!("{} accounts found in the snapshot", from_snapshot.len());
        let mut revived = 0usize;
        for (key, (account, slot)) in from_snapshot {
            if dead_in_store.contains(&key) {
                // Strictly newer only: never resurrect a close the replay itself performed.
                let store_slot = store.get(&key).map(|(_, s)| s).unwrap_or(0);
                if slot <= store_slot {
                    continue;
                }
                revived += 1;
            } else if in_store.contains(&key) {
                continue;
            }
            resolved.push((key, (account, slot)));
        }
        eprintln!("{revived} dead store rows outranked by a newer snapshot record");
    }

    let mut stake_keys: HashSet<Pubkey> = checkpoint
        .stake_keys
        .iter()
        .map(|k| Pubkey::new_from_array(*k))
        .collect();
    stake_keys.extend(crossing_stakes.iter().copied());
    // The bank resolves delegations by walking this set, so it must name every stake account held.
    let from_resolved = resolved
        .iter()
        .filter(|(_, (a, _))| *a.owner() == solana_sdk_ids::stake::id() && a.lamports() > 0)
        .map(|(k, _)| *k)
        .collect::<HashSet<Pubkey>>();
    let added = from_resolved.difference(&stake_keys).count();
    stake_keys.extend(from_resolved);
    eprintln!(
        "{} stake keys from the checkpoint, {} after the snapshot crossing, {} more held by the bank",
        checkpoint.stake_keys.len(),
        stake_keys.len() - added,
        added
    );

    resolved.sort_unstable_by_key(|(k, _)| *k);
    let accounts: Vec<([u8; 32], Vec<u8>)> = resolved
        .into_iter()
        .map(|(key, (account, slot))| {
            (
                key.to_bytes(),
                slate_format::encode_account(
                    slot,
                    account.lamports(),
                    account.rent_epoch(),
                    account.executable(),
                    &account.owner().to_bytes(),
                    account.data(),
                ),
            )
        })
        .collect();
    eprintln!("collected {} accounts", accounts.len());

    let fixture = Fixture {
        slot: boundary_slot,
        parent_bank_hash: roll.bank_hash,
        parent_lt_hash: roll.lt_hash.clone(),
        expected_bank_hash: vote_hash.to_bytes(),
        block: bincode::serialize(boundary_block)?,
        accounts,
        reward_inputs: Some(RewardInputsRecord {
            inflation_initial: inflation.initial,
            inflation_terminal: inflation.terminal,
            inflation_taper: inflation.taper,
            inflation_foundation: inflation.foundation,
            inflation_foundation_term: inflation.foundation_term,
            capitalization: checkpoint.capitalization,
            slots_per_year: manifest.slots_per_year,
            vote_accounts: vote_accounts.iter().map(|k| k.to_bytes()).collect(),
        }),
        stake_delegations: stake_keys.iter().map(Pubkey::to_bytes).collect(),
        capitalization: Some(checkpoint.capitalization),
    };

    let computed = replay_fixture(&fixture).context("replaying the fixture we just built")?;
    if computed.to_bytes() != fixture.expected_bank_hash {
        bail!("refusing to write: fixture replays to {computed}, vote says {vote_hash}");
    }

    std::fs::write(out_path, fixture.encode())?;
    eprintln!("wrote {out_path} (vote-confirmed {vote_hash})");
    Ok(())
}
