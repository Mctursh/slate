//! End-to-end backfill: seed a range's footprint from a snapshot, replay the
//! blocks rolling the bank forward, and persist the indexed program's accounts.
//! This is the one path that ties the pipeline together; feed it a snapshot, the
//! range's blocks, the program to index, and a store.

use std::{collections::HashSet, io::Read, sync::Arc};

use anyhow::Result;
use slate_store::ClickHouseClient;
use solana_hash::Hash;
use solana_lattice_hash::lt_hash::LtHash;
use solana_pubkey::Pubkey;

use crate::{
    RangeReplay, ReplayBank, Replayer, WriteRecord,
    block::{self, Block},
    boundary,
    build_feature_set, compat, persist, register_builtins, snapshot,
    source::BlockSource,
    store::{AccountStore, DiskStore, MemStore},
};

/// How to back the replay bank's account universe for a run.
pub enum AccountStoreChoice {
    /// Everything in RAM (a HashMap). Fine for small ranges and tests.
    Memory,
    /// redb on disk at `path`, with `cache_bytes` of page cache — the whole RAM budget.
    /// For ranges whose footprint won't fit in memory.
    Disk {
        path: std::path::PathBuf,
        cache_bytes: usize,
    },
}

/// What a backfill run produced: how far the replay got, and — if a boundary snapshot was
/// given — the byte-exact diff of the reconstructed end-state against it.
pub struct BackfillReport {
    pub replay: RangeReplay,
    pub boundary: Option<boundary::DiffReport>,
}

/// Backfill the range `(s_snap, to]` for `program`, seeded from a snapshot taken at
/// slot `s_snap`, pulling blocks from `source` a chunk at a time:
///
/// 1. **footprint pass** — stream the range once, accumulating every account the
///    blocks reference (the wide seed, not owner-scoped); one snapshot pass then
///    loads those plus every account the program owns (the S_snap baseline);
/// 2. build the exact per-slot feature set from the seeded feature accounts;
/// 3. **replay pass** — stream the range again, replaying each chunk and rolling the
///    bank forward;
/// 4. persist the S_snap baseline, then the fully-completed blocks' program-owned
///    changes on top — the narrow store — and cover `[s_snap, last-completed-slot]`.
///
/// Blocks are never all held at once — each pass fetches a chunk, uses it, and drops
/// it — so the range length is bounded by the account store, not by RAM for blocks.
/// The baseline is what makes an untouched account queryable: without it a covered
/// as-of read of an account the range never touched would wrongly say "does not
/// exist". A halted block is unreliable, so its partial writes are dropped and it
/// isn't counted as covered. The returned [`RangeReplay`] reports how far it got.
pub async fn backfill(
    snapshot: impl Read,
    s_snap: u64,
    source: Arc<dyn BlockSource>,
    from: u64,
    to: u64,
    program: &Pubkey,
    store: &ClickHouseClient,
    bootstrap: Option<(LtHash, Hash)>,
    account_store: AccountStoreChoice,
    chunk_slots: usize,
    verify_end: Option<Box<dyn Read>>,
) -> Result<BackfillReport> {
    let chunk_slots = chunk_slots.max(1);
    // The confirmed slots to replay, `(from, to]`. Just u64s — bounded no matter how
    // long the range — so we hold the whole list; the blocks themselves never all fit.
    let slots = {
        let src = Arc::clone(&source);
        tokio::task::spawn_blocking(move || src.confirmed_slots(from, to)).await??
    };

    // Footprint pass: stream the range once, accumulating the accounts the blocks
    // reference. Never more than a chunk of blocks resident at a time.
    let mut footprint = HashSet::new();
    for chunk in slots.chunks(chunk_slots) {
        let blocks = fetch_chunk(&source, chunk).await?;
        block::extend_footprint(&mut footprint, &blocks);
    }
    block::footprint_fixed(&mut footprint);
    // Also seed the programData (bytecode) accounts of every upgradeable program the
    // range invokes: they're PDAs of the program ids, never declared keys, so the
    // footprint alone misses them and the SVM can't load the programs.
    let programdata = block::programdata_addresses(&footprint);
    footprint.extend(programdata);
    // Seed the SlotHashes sysvar from the snapshot too. Programs read it for on-chain
    // randomness — its entries are real bank hashes — and it's read via syscall, never
    // passed as an account, so the footprint never captures it. The snapshot's value
    // (bank hashes up to s_snap) is exactly what the first replayed block (s_snap+1)
    // must see; without it an empty SlotHashes makes such a program panic reading a
    // nonexistent entry.
    footprint.insert(solana_sdk_ids::sysvar::slot_hashes::id());

    // Remember the store backing, so a boundary diff (if requested) loads the end
    // snapshot into the same kind of store — disk for a big window, RAM otherwise.
    let end_store_mode = match &account_store {
        AccountStoreChoice::Memory => None,
        AccountStoreChoice::Disk { path, cache_bytes } => {
            Some((path.with_extension("end.redb"), *cache_bytes))
        }
    };

    // Seed the bank from the snapshot into the chosen store: a RAM map, or redb on disk.
    let (mut bank, baseline) = match account_store {
        AccountStoreChoice::Memory => {
            let accounts = snapshot::load_accounts(snapshot, Some(&footprint), Some(program))?;
            let baseline = persist::baseline_rows(&accounts, program, s_snap);
            let mut bank = ReplayBank::default();
            for (pubkey, (account, slot)) in &accounts {
                bank.insert(*pubkey, account.clone(), *slot);
            }
            (bank, baseline)
        }
        AccountStoreChoice::Disk { path, cache_bytes } => {
            let mut disk = crate::store::DiskStore::create(&path, cache_bytes)?;
            let (written, owned) =
                snapshot::stream_into_store(snapshot, &mut disk, Some(&footprint), Some(program))?;
            eprintln!("seeded {written} accounts into disk store {}", path.display());
            // The program-owned accounts collected during the seed ARE the S_snap
            // baseline — same set the memory path derives from load_accounts.
            let baseline = persist::baseline_rows(&owned, program, s_snap);
            (ReplayBank::with_store(Box::new(disk)), baseline)
        }
    };
    // Start the bank-hash roll from the manifest's lattice + bank hash at s_snap, so
    // SlotHashes rolls forward with real bank hashes as the range replays.
    if let Some((lt_hash, bank_hash)) = bootstrap {
        bank.bootstrap_bankhash(lt_hash, bank_hash);
    }

    // Baseline first, then replay pass: stream each chunk, roll the bank forward, and
    // persist the chunk's program writes, clearing the log so it never grows with the
    // range length.
    store.insert_accounts(&baseline).await?;

    // Track the last successfully-replayed slot as we go. With a getBlocks-less source the
    // candidate `slots` include skipped slots, so covered_hi can't be recovered by indexing
    // `slots` — it comes from the fetched blocks themselves.
    let mut covered_hi = s_snap;
    let result = if let Some(&first_slot) = slots.first() {
        let epoch = first_slot / 432_000;
        let feature_set = build_feature_set(&bank, first_slot);
        let replayer = Replayer::new_with_feature_set(first_slot, epoch, feature_set);
        register_builtins(&mut bank, &replayer.processor);
        // Re-supply native programs agave deleted after their core-BPF migration
        // that this slot range predates (e.g. Stake). Gated per-program on the
        // migration feature, so it's a no-op once the migration is active on chain.
        compat::register_removed_builtins(&mut bank, &replayer.processor, replayer.feature_set());

        let mut completed = 0usize;
        let mut halt = None;
        for chunk in slots.chunks(chunk_slots) {
            let blocks = fetch_chunk(&source, chunk).await?;
            let chunk_replay = replayer.replay_range(&mut bank, &blocks);
            let done = chunk_replay.blocks_completed;
            if done > 0 {
                covered_hi = blocks[done - 1].slot;
            }
            completed += done;
            // Drain and persist this chunk's program writes, up to the last good slot.
            let changes: Vec<WriteRecord> = bank
                .take_writes()
                .into_iter()
                .filter(|w| w.slot <= covered_hi)
                .collect();
            store
                .insert_accounts(&persist::program_account_rows(&changes, program))
                .await?;
            if let Some(h) = chunk_replay.halt {
                halt = Some(h);
                break;
            }
        }
        RangeReplay {
            blocks_completed: completed,
            halt,
        }
    } else {
        RangeReplay {
            blocks_completed: 0,
            halt: None,
        }
    };

    store.record_coverage(s_snap, covered_hi).await?;

    // Boundary diff (verification only — logs, doesn't gate the run): prove the
    // reconstructed end-state matches the real snapshot at the last replayed slot,
    // byte-for-byte over the footprint. Every account a tx wrote is in the footprint, so
    // this catches any write the replay got wrong that the oracle (lamports/status only)
    // couldn't see.
    let boundary = if let Some(end_snapshot) = verify_end {
        bank.flush();
        let mut end_store: Box<dyn AccountStore> = match &end_store_mode {
            None => Box::new(MemStore::default()),
            Some((path, cache_bytes)) => Box::new(DiskStore::create(path, *cache_bytes)?),
        };
        let (loaded, _) =
            snapshot::stream_into_store(end_snapshot, &mut *end_store, Some(&footprint), None)?;
        eprintln!("loaded {loaded} end-snapshot accounts for the boundary diff");
        Some(boundary::boundary_diff(bank.store(), &footprint, &*end_store))
    } else {
        None
    };

    Ok(BackfillReport {
        replay: result,
        boundary,
    })
}

/// Fetch one chunk's blocks off the async worker — the source's `fetch` is blocking
/// I/O, so running it on the runtime's blocking pool keeps the persist of the previous
/// chunk from stalling behind the network.
async fn fetch_chunk(source: &Arc<dyn BlockSource>, slots: &[u64]) -> Result<Vec<Block>> {
    let src = Arc::clone(source);
    let slots = slots.to_vec();
    tokio::task::spawn_blocking(move || src.fetch(&slots)).await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BlockTx, LoadedAddresses, TxMeta};
    use crate::source::VecBlockSource;
    use solana_account::ReadableAccount;
    use solana_hash::Hash;
    use solana_instruction::{AccountMeta, Instruction};
    use solana_message::{Message, VersionedMessage};
    use solana_signature::Signature;
    use solana_transaction::versioned::VersionedTransaction;

    // The real test-validator snapshot the loader was developed against (slot 200).
    const SNAPSHOT: &[u8] = include_bytes!("test_snapshot.tar.zst");

    // End-to-end against a local ClickHouse (slate_test db). Run with:
    //   cargo test -p slate-replay backfills_a_range -- --ignored
    #[tokio::test]
    #[ignore = "needs a local ClickHouse (slate_test db)"]
    async fn backfills_a_range_from_snapshot_into_the_store() {
        let system = solana_sdk_ids::system_program::id();

        // src = richest dataless system wallet (we spend from it); `untouched` =
        // the next one, which the transfers never touch — it proves the baseline
        // covers an account the range didn't change.
        let accounts = snapshot::load_accounts(SNAPSHOT, None, None).unwrap();
        let mut wallets: Vec<(Pubkey, u64)> = accounts
            .iter()
            .filter(|(_, (a, _))| *a.owner() == system && a.data().is_empty())
            .map(|(k, (a, _))| (*k, a.lamports()))
            .collect();
        wallets.sort_by_key(|&(_, bal)| std::cmp::Reverse(bal));
        let (src, src_balance) = wallets[0];
        let (untouched, untouched_balance) = wallets[1];
        let mid = Pubkey::new_unique();
        let dst = Pubkey::new_unique();
        let (a1, a2, fee) = (2_000_000u64, 1_000_000u64, 5_000u64);
        let s_snap = 200u64; // the fixture snapshot's slot
        let s = 201u64; // first replayed block, just after the snapshot

        // A one-transfer block; account order is [from, to, system].
        let transfer =
            |from: &Pubkey, to: &Pubkey, amount: u64, from_pre: u64, slot: u64| -> Block {
                let mut data = vec![2u8, 0, 0, 0]; // System Transfer
                data.extend_from_slice(&amount.to_le_bytes());
                let ix = Instruction {
                    program_id: system,
                    accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
                    data,
                };
                let message = Message::new_with_blockhash(&[ix], Some(from), &Hash::default());
                Block {
                    slot,
                    parent_slot: slot - 1,
                    blockhash: Hash::default(),
                    previous_blockhash: Hash::default(),
                    block_time: 1_700_000_000,
                    transactions: vec![BlockTx {
                        transaction: VersionedTransaction {
                            signatures: vec![Signature::default()],
                            message: VersionedMessage::Legacy(message),
                        },
                        meta: TxMeta {
                            err: None,
                            fee,
                            compute_units_consumed: 150,
                            pre_balances: vec![from_pre, 0, 1],
                            post_balances: vec![from_pre - amount - fee, amount, 1],
                            loaded_addresses: LoadedAddresses::default(),
                            post_token_balances: vec![],
                        },
                    }],
                    fee_reward: None,
                }
            };

        // src -> mid at slot s, then mid -> dst at s+1 (rolls the bank forward).
        let blocks = [
            transfer(&src, &mid, a1, src_balance, s),
            transfer(&mid, &dst, a2, a1, s + 1),
        ];

        let store = ClickHouseClient::with_database("http://localhost:8123", "slate_test");
        let source = Arc::new(VecBlockSource::new(blocks.to_vec()));
        let outcome = backfill(
            SNAPSHOT,
            s_snap,
            source,
            s_snap,
            s + 1,
            &system,
            &store,
            None,
            AccountStoreChoice::Memory,
            2000,
            None,
        )
        .await
        .expect("backfill");
        assert!(
            outcome.replay.is_complete(),
            "backfill halted: {:?}",
            outcome.replay.halt
        );

        // Per-slot history landed in the store: mid = a1 at slot s, then
        // a1 - a2 - fee at s+1; dst = a2 at s+1; the range reads covered.
        let mid_at_s = store
            .get_account_info(&mid.to_bytes(), s)
            .await
            .unwrap()
            .expect("mid present at s");
        assert_eq!(mid_at_s.lamports, a1);

        let mid_at_end = store
            .get_account_info(&mid.to_bytes(), s + 1)
            .await
            .unwrap()
            .expect("mid present at s+1");
        assert_eq!(mid_at_end.lamports, a1 - a2 - fee);

        let dst_at_end = store
            .get_account_info(&dst.to_bytes(), s + 1)
            .await
            .unwrap()
            .expect("dst present at s+1");
        assert_eq!(dst_at_end.lamports, a2);

        // The baseline: an account the range never touched is still there at S_snap.
        let untouched_baseline = store
            .get_account_info(&untouched.to_bytes(), s_snap)
            .await
            .unwrap()
            .expect("untouched account present from the baseline");
        assert_eq!(untouched_baseline.lamports, untouched_balance);

        // Baseline + change layered for src: its S_snap value, then post-transfer.
        let src_baseline = store
            .get_account_info(&src.to_bytes(), s_snap)
            .await
            .unwrap()
            .expect("src baseline present");
        assert_eq!(src_baseline.lamports, src_balance);
        let src_after = store
            .get_account_info(&src.to_bytes(), s + 1)
            .await
            .unwrap()
            .expect("src present after the transfer");
        assert_eq!(src_after.lamports, src_balance - a1 - fee);

        // Coverage spans the baseline slot through the last replayed slot.
        assert!(store.is_covered(s_snap, s + 1).await.unwrap());
    }

    // The boundary diff must be byte-exact when nothing replays: seed the fixture, replay
    // an empty range, diff the seeded end-state against the SAME snapshot. Exercises the
    // whole wiring — end-store load, footprint filter, store access, verdict — against a
    // real snapshot with a known-exact answer, so a plumbing bug shows here, not 6h into
    // the 50k run.
    #[tokio::test]
    #[ignore = "needs a local ClickHouse (slate_test db)"]
    async fn boundary_diff_is_exact_when_nothing_replays() {
        let system = solana_sdk_ids::system_program::id();
        let store = ClickHouseClient::with_database("http://localhost:8123", "slate_test");
        // Empty range (from == to == s_snap → no confirmed slots), verify against the seed.
        let source = Arc::new(VecBlockSource::new(vec![]));
        let outcome = backfill(
            SNAPSHOT,
            200,
            source,
            200,
            200,
            &system,
            &store,
            None,
            AccountStoreChoice::Memory,
            2000,
            Some(Box::new(SNAPSHOT)),
        )
        .await
        .expect("backfill");

        let boundary = outcome.boundary.expect("boundary diff ran");
        assert!(boundary.checked > 0, "should have checked the seeded footprint");
        assert!(
            boundary.is_exact(),
            "no replay → end-state == seed == snapshot, expected byte-exact, got {} mismatch(es): {:?}",
            boundary.mismatches.len(),
            &boundary.mismatches[..boundary.mismatches.len().min(5)]
        );
    }
}
