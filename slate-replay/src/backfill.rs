//! End-to-end backfill: seed a range's footprint from a snapshot, replay the
//! blocks rolling the bank forward, and persist the indexed program's accounts.
//! This is the one path that ties the pipeline together; feed it a snapshot, the
//! range's blocks, the program to index, and a store.

use std::io::Read;

use anyhow::Result;
use slate_store::ClickHouseClient;
use solana_hash::Hash;
use solana_lattice_hash::lt_hash::LtHash;
use solana_pubkey::Pubkey;

use crate::{
    RangeReplay, ReplayBank, Replayer, WriteRecord,
    block::{self, Block},
    build_feature_set, persist, register_builtins, snapshot,
};

/// Backfill `blocks` (contiguous, in one epoch) for `program`, seeded from a
/// snapshot taken at slot `s_snap`:
///
/// 1. one snapshot pass loads the range's footprint (the wide seed, not owner-
///    scoped, so every account a block touches is present) plus every account the
///    program owns (the S_snap baseline);
/// 2. build the exact per-slot feature set from the seeded feature accounts;
/// 3. replay every block, rolling the bank forward;
/// 4. persist the S_snap baseline, then the fully-completed blocks' program-owned
///    changes on top — the narrow store — and cover `[s_snap, last-completed-slot]`.
///
/// The baseline is what makes an untouched account queryable: without it a covered
/// as-of read of an account the range never touched would wrongly say "does not
/// exist". A halted block is unreliable, so its partial writes are dropped and it
/// isn't counted as covered. The returned [`RangeReplay`] reports how far it got.
pub async fn backfill(
    snapshot: impl Read,
    s_snap: u64,
    blocks: &[Block],
    program: &Pubkey,
    store: &ClickHouseClient,
    bootstrap: Option<(LtHash, Hash)>,
) -> Result<RangeReplay> {
    // Wide seed (footprint) ∪ baseline (program-owned), in a single snapshot scan.
    let mut footprint = block::footprint(blocks);
    // Also seed the programData (bytecode) accounts of every upgradeable program
    // the range invokes: they're PDAs of the program ids, never declared keys, so
    // the footprint alone misses them and the SVM can't load the programs.
    let programdata = block::programdata_addresses(&footprint);
    footprint.extend(programdata);
    // Seed the SlotHashes sysvar from the snapshot too. Programs read it for on-chain
    // randomness — its entries are real bank hashes — and it's read via syscall, never
    // passed as an account, so the footprint never captures it. The snapshot's value
    // (bank hashes up to s_snap) is exactly what the first replayed block (s_snap+1)
    // must see; without it an empty SlotHashes makes such a program panic reading a
    // nonexistent entry.
    footprint.insert(solana_sdk_ids::sysvar::slot_hashes::id());
    let accounts = snapshot::load_accounts(snapshot, Some(&footprint), Some(program))?;
    let baseline = persist::baseline_rows(&accounts, program, s_snap);

    let mut bank = ReplayBank::default();
    for (pubkey, (account, slot)) in &accounts {
        bank.insert(*pubkey, account.clone(), *slot);
    }
    // Start the bank-hash roll from the manifest's lattice + bank hash at s_snap, so
    // SlotHashes rolls forward with real bank hashes as the range replays.
    if let Some((lt_hash, bank_hash)) = bootstrap {
        bank.bootstrap_bankhash(lt_hash, bank_hash);
    }

    let result = if let Some(first) = blocks.first() {
        let epoch = first.slot / 432_000;
        let feature_set = build_feature_set(&bank, first.slot);
        let replayer = Replayer::new_with_feature_set(first.slot, epoch, feature_set);
        register_builtins(&mut bank, &replayer.processor);
        replayer.replay_range(&mut bank, blocks)
    } else {
        RangeReplay {
            blocks_completed: 0,
            halt: None,
        }
    };

    let covered_hi = if result.blocks_completed > 0 {
        blocks[result.blocks_completed - 1].slot
    } else {
        s_snap
    };
    let changes: Vec<WriteRecord> = bank
        .writes()
        .iter()
        .filter(|w| w.slot <= covered_hi)
        .cloned()
        .collect();

    // Baseline first, then the changes layered on top, then one coverage segment.
    store.insert_accounts(&baseline).await?;
    store
        .insert_accounts(&persist::program_account_rows(&changes, program))
        .await?;
    store.record_coverage(s_snap, covered_hi).await?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BlockTx, LoadedAddresses, TxMeta};
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
                }
            };

        // src -> mid at slot s, then mid -> dst at s+1 (rolls the bank forward).
        let blocks = [
            transfer(&src, &mid, a1, src_balance, s),
            transfer(&mid, &dst, a2, a1, s + 1),
        ];

        let store = ClickHouseClient::with_database("http://localhost:8123", "slate_test");
        let outcome = backfill(SNAPSHOT, s_snap, &blocks, &system, &store, None)
            .await
            .expect("backfill");
        assert!(outcome.is_complete(), "backfill halted: {:?}", outcome.halt);

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
}
