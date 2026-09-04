use std::{collections::HashSet, io::Read, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use slate_store::ClickHouseClient;
use solana_hash::Hash;
use solana_lattice_hash::lt_hash::LtHash;
use solana_pubkey::Pubkey;

use crate::{
    RangeReplay, ReplayBank, Replayer, WriteRecord,
    block::{self, Block},
    boundary, build_feature_set, compat, persist, register_builtins, snapshot,
    source::{BlockSource, CachingBlockSource},
    store::{AccountStore, DiskStore, MemStore},
};

pub enum AccountStoreChoice {
    Memory,
    Disk {
        path: std::path::PathBuf,
        cache_bytes: usize,
    },
}

pub struct BackfillReport {
    pub replay: RangeReplay,
    pub boundary: Option<boundary::DiffReport>,
}

#[allow(clippy::too_many_arguments)]
pub async fn backfill(
    snapshot: impl Read,
    s_snap: u64,
    source: Arc<dyn BlockSource>,
    block_cache: Option<PathBuf>,
    from: u64,
    to: u64,
    program: &Pubkey,
    store: &ClickHouseClient,
    bootstrap: Option<(LtHash, Hash)>,
    account_store: AccountStoreChoice,
    chunk_slots: usize,
    verify_end: Option<Box<dyn Read>>,
    resume: bool,
) -> Result<BackfillReport> {
    let chunk_slots = chunk_slots.max(1);
    let source: Arc<dyn BlockSource> = match block_cache {
        None => source,
        Some(path) => Arc::new(CachingBlockSource::new(source, path)?),
    };
    // Slots are just u64s (bounded), so hold them all; the blocks themselves never all fit.
    let slots = {
        let src = Arc::clone(&source);
        tokio::task::spawn_blocking(move || src.confirmed_slots(from, to)).await??
    };

    // Footprint pass: stream the range once to build the seed set. A resume skips the seed, so this only runs then if the boundary diff needs it to filter the end snapshot.
    let mut footprint = HashSet::new();
    if !resume || verify_end.is_some() {
        for chunk in slots.chunks(chunk_slots) {
            let blocks = fetch_chunk(&source, chunk).await?;
            block::extend_footprint(&mut footprint, &blocks);
        }
        block::footprint_fixed(&mut footprint);
        // programData PDAs and SlotHashes are read, not declared as keys, so the footprint misses them.
        let programdata = block::programdata_addresses(&footprint);
        footprint.extend(programdata);
        footprint.insert(solana_sdk_ids::sysvar::slot_hashes::id());
    }

    // Remember the store backing so a boundary diff loads the end snapshot into the same kind.
    let end_store_mode = match &account_store {
        AccountStoreChoice::Memory => None,
        AccountStoreChoice::Disk { path, cache_bytes } => {
            Some((path.with_extension("end.redb"), *cache_bytes))
        }
    };

    // Fresh run: seed the bank from the snapshot and roll from the manifest hashes. Resume: reopen the store and roll from its checkpoint. Third value is the slot to resume after (S_snap for a fresh run).
    let (mut bank, baseline, resume_from) = if resume {
        let AccountStoreChoice::Disk { path, cache_bytes } = account_store else {
            anyhow::bail!("--resume requires --store disk");
        };
        let mut disk = crate::store::DiskStore::create(&path, cache_bytes)?;
        let (slot, roll) = disk
            .read_checkpoint()
            .context("--resume: the store has no checkpoint to resume from")?;
        disk.set_checkpoint_mode(true);
        let mut bank = ReplayBank::with_store(Box::new(disk));
        // Empty roll = the original run had the bank-hash roll off; keep it off.
        if !roll.is_empty() {
            let (lt_hash, bank_hash) = crate::bankhash::deserialize_roll_state(&roll)
                .context("--resume: checkpoint roll state is corrupt")?;
            bank.bootstrap_bankhash(lt_hash, bank_hash);
        }
        eprintln!("resuming after checkpoint at slot {slot}");
        (bank, Vec::new(), slot)
    } else {
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
                let (written, owned) = snapshot::stream_into_store(
                    snapshot,
                    &mut disk,
                    Some(&footprint),
                    Some(program),
                )?;
                eprintln!("seeded {written} accounts into disk store {}", path.display());
                // Seed's done (auto-flushed in batches); from here hold writes so only checkpoint_flush commits.
                disk.set_checkpoint_mode(true);
                let baseline = persist::baseline_rows(&owned, program, s_snap);
                (ReplayBank::with_store(Box::new(disk)), baseline)
            }
        };
        if let Some((lt_hash, bank_hash)) = bootstrap {
            bank.bootstrap_bankhash(lt_hash, bank_hash);
        }
        (bank, baseline, s_snap)
    };

    // Fresh run: checkpoint at s_snap right after seeding, so a crash before chunk 1's checkpoint still resumes (skips the ~expensive re-seed) instead of finding no checkpoint.
    if !resume {
        bank.checkpoint(s_snap)?;
    }

    // Fresh run inserts the S_snap baseline; a resume already has it.
    if !baseline.is_empty() {
        store.insert_accounts(&baseline).await?;
    }

    // On resume, replay only the slots past the checkpoint; earlier ones are already persisted.
    let replay_slots: Vec<u64> = slots.into_iter().filter(|&s| s > resume_from).collect();
    let mut covered_hi = resume_from;
    let result = if let Some(&first_slot) = replay_slots.first() {
        let epoch = first_slot / 432_000;
        let feature_set = build_feature_set(&bank, first_slot);
        let replayer = Replayer::new_with_feature_set(first_slot, epoch, feature_set);
        register_builtins(&mut bank, &replayer.processor);
        // Compat: re-supply native builtins agave deleted post core-BPF migration (e.g. Stake), gated per feature so it's a no-op once active.
        compat::register_removed_builtins(&mut bank, &replayer.processor, replayer.feature_set());

        let mut completed = 0usize;
        let mut halt = None;
        for chunk in replay_slots.chunks(chunk_slots) {
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
            // Checkpoint clean chunks only. On a halt the roller sits one slot past covered_hi, so we skip it and let the halting chunk's buffered writes drop unflushed; resume re-runs that chunk.
            bank.checkpoint(covered_hi)?;
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

    // Boundary diff (verification only): byte-exact end-state vs the real snapshot over the footprint. Skipped on a halt: the end-state is incomplete, and skipping it avoids touching the store past the last checkpoint.
    let boundary = if let (Some(end_snapshot), None) = (verify_end, &result.halt) {
        bank.flush();
        let mut end_store: Box<dyn AccountStore> = match &end_store_mode {
            None => Box::new(MemStore::default()),
            Some((path, cache_bytes)) => Box::new(DiskStore::create(path, *cache_bytes)?),
        };
        let (loaded, _) =
            snapshot::stream_into_store(end_snapshot, &mut *end_store, Some(&footprint), None)?;
        eprintln!("loaded {loaded} end-snapshot accounts for the boundary diff");
        Some(boundary::boundary_diff(
            bank.store(),
            &footprint,
            &*end_store,
        ))
    } else {
        None
    };

    Ok(BackfillReport {
        replay: result,
        boundary,
    })
}

// Run the source's blocking fetch on the blocking pool so the previous chunk's persist doesn't stall behind the network.
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

        // src = richest dataless wallet (we spend from it); untouched = the next one, proving the baseline covers it.
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
            None,
            s_snap,
            s + 1,
            &system,
            &store,
            None,
            AccountStoreChoice::Memory,
            2000,
            None,
            false,
        )
        .await
        .expect("backfill");
        assert!(
            outcome.replay.is_complete(),
            "backfill halted: {:?}",
            outcome.replay.halt
        );

        // Per-slot history landed: mid = a1 at s, then a1 - a2 - fee at s+1; dst = a2 at s+1.
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

    // Boundary diff must be byte-exact when nothing replays: seed, replay empty, diff against the same snapshot.
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
            None,
            200,
            200,
            &system,
            &store,
            None,
            AccountStoreChoice::Memory,
            2000,
            Some(Box::new(SNAPSHOT)),
            false,
        )
        .await
        .expect("backfill");

        let boundary = outcome.boundary.expect("boundary diff ran");
        assert!(
            boundary.checked > 0,
            "should have checked the seeded footprint"
        );
        assert!(
            boundary.is_exact(),
            "no replay → end-state == seed == snapshot, expected byte-exact, got {} mismatch(es): {:?}",
            boundary.mismatches.len(),
            &boundary.mismatches[..boundary.mismatches.len().min(5)]
        );
    }

    // Resume: run the first half fresh, --resume the rest from the checkpoint; the final per-slot
    // state must match a straight run. Disk store, since resume needs a checkpoint to reopen.
    #[tokio::test]
    #[ignore = "needs a local ClickHouse (slate_test db)"]
    async fn resume_continues_from_a_checkpoint() {
        let system = solana_sdk_ids::system_program::id();
        let accounts = snapshot::load_accounts(SNAPSHOT, None, None).unwrap();
        let (src, src_balance) = accounts
            .iter()
            .filter(|(_, (a, _))| *a.owner() == system && a.data().is_empty())
            .map(|(k, (a, _))| (*k, a.lamports()))
            .max_by_key(|&(_, bal)| bal)
            .expect("a fundable wallet");
        let (w1, w2, w3) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let fee = 5_000u64;

        let transfer = |from: &Pubkey, to: &Pubkey, amount: u64, from_pre: u64, slot: u64| -> Block {
            let mut data = vec![2u8, 0, 0, 0];
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

        // One hop per slot: src -> w1 (201), w1 -> w2 (202), w2 -> w3 (203).
        let blocks = vec![
            transfer(&src, &w1, 3_000_000, src_balance, 201),
            transfer(&w1, &w2, 2_000_000, 3_000_000, 202),
            transfer(&w2, &w3, 1_000_000, 2_000_000, 203),
        ];

        let store = ClickHouseClient::with_database("http://localhost:8123", "slate_test");
        let path = std::env::temp_dir().join("slate_resume_test.redb");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("end.redb"));
        let disk = || AccountStoreChoice::Disk {
            path: path.clone(),
            cache_bytes: 32 * 1024 * 1024,
        };
        let source = || Arc::new(VecBlockSource::new(blocks.clone())) as Arc<dyn BlockSource>;

        // Fresh run over (200, 202]: replays 201, 202 and checkpoints each (chunk_slots = 1).
        backfill(
            SNAPSHOT, 200, source(), None, 200, 202, &system, &store, None, disk(), 1, None, false,
        )
        .await
        .expect("fresh run");

        // --resume over (200, 203]: picks up from the checkpoint at 202 and replays only 203.
        let out = backfill(
            SNAPSHOT, 200, source(), None, 200, 203, &system, &store, None, disk(), 1, None, true,
        )
        .await
        .expect("resume run");
        assert!(out.replay.is_complete(), "resume halted: {:?}", out.replay.halt);

        // w3 is funded only at 203, past the checkpoint: proves the resume replayed on.
        let w3_end = store
            .get_account_info(&w3.to_bytes(), 203)
            .await
            .unwrap()
            .expect("w3 present at 203");
        assert_eq!(w3_end.lamports, 1_000_000);
        // w1's slot-202 value from the fresh run survived the reopen.
        let w1_end = store
            .get_account_info(&w1.to_bytes(), 203)
            .await
            .unwrap()
            .expect("w1 present");
        assert_eq!(w1_end.lamports, 3_000_000 - 2_000_000 - fee);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("end.redb"));
    }
}
