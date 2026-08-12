//! Persist replayed account state to slate-store (ClickHouse).
//!
//! This is the narrow-store side of the pipeline. Replay is wide — it touches
//! every account a block references so reads are correct — but we only *store* the
//! accounts owned by the program being indexed. So the persistence step owner-
//! filters the replay's write log down to that program and writes one row per
//! (account, slot) it changed, which is exactly what an as-of-slot query needs.

use slate_store::{AccountUpdateInsert, ClickHouseClient, StoreResult};
use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;

use crate::WriteRecord;

/// Turn the replay's write log into store rows for accounts owned by `owner` (the
/// program being indexed), dropping every other write. Each surviving write
/// becomes one row: the account's state at the slot it changed, so the store holds
/// per-slot history rather than only the final value.
pub fn program_account_rows(writes: &[WriteRecord], owner: &Pubkey) -> Vec<AccountUpdateInsert> {
    writes
        .iter()
        .filter(|w| w.account.owner() == owner)
        .map(|w| AccountUpdateInsert {
            pubkey: w.pubkey.to_bytes(),
            slot: w.slot,
            write_version: w.write_version,
            owner: w.account.owner().to_bytes(),
            lamports: w.account.lamports(),
            executable: w.account.executable() as u8,
            rent_epoch: w.account.rent_epoch(),
            data: w.account.data().to_vec(),
            txn_signature: None,
        })
        .collect()
}

/// Persist the program-owned writes to `store` and record `[lo, hi]` as covered so
/// as-of queries in that range read as Exact rather than Uncertain.
pub async fn persist_program_accounts(
    store: &ClickHouseClient,
    writes: &[WriteRecord],
    owner: &Pubkey,
    lo: u64,
    hi: u64,
) -> StoreResult<()> {
    let rows = program_account_rows(writes, owner);
    store.insert_accounts(&rows).await?;
    store.record_coverage(lo, hi).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_account::{Account, AccountSharedData};

    fn account(owner: Pubkey, lamports: u64, data: &[u8]) -> AccountSharedData {
        AccountSharedData::from(Account {
            lamports,
            data: data.to_vec(),
            owner,
            executable: false,
            rent_epoch: 0,
        })
    }

    #[test]
    fn rows_are_owner_filtered_and_per_slot() {
        let target = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let pk = Pubkey::new_unique();
        let writes = vec![
            WriteRecord {
                slot: 5,
                write_version: 1,
                pubkey: pk,
                account: account(target, 10, b""),
            },
            // Not owned by the target — must be dropped.
            WriteRecord {
                slot: 5,
                write_version: 2,
                pubkey: Pubkey::new_unique(),
                account: account(other, 20, b""),
            },
            // Same pk changing again at a later slot — kept as its own row.
            WriteRecord {
                slot: 7,
                write_version: 3,
                pubkey: pk,
                account: account(target, 30, b""),
            },
        ];

        let rows = program_account_rows(&writes, &target);
        assert_eq!(rows.len(), 2, "only target-owned writes survive");
        assert_eq!(rows[0].slot, 5);
        assert_eq!(rows[0].lamports, 10);
        assert_eq!(rows[0].owner, target.to_bytes());
        assert_eq!(rows[1].slot, 7, "one row per slot the account changed");
        assert_eq!(rows[1].lamports, 30);
    }

    // End-to-end against a local ClickHouse (the `slate_test` db). Run with:
    //   cargo test -p slate-replay --ignored persists_program_accounts_end_to_end
    #[tokio::test]
    #[ignore = "needs a local ClickHouse (slate_test db)"]
    async fn persists_program_accounts_end_to_end() {
        let store = ClickHouseClient::with_database("http://localhost:8123", "slate_test");
        let owner = Pubkey::new_unique();
        let pk = Pubkey::new_unique();
        let dropped = Pubkey::new_unique();

        let writes = vec![
            WriteRecord {
                slot: 100,
                write_version: 1,
                pubkey: pk,
                account: account(owner, 5, b"v1"),
            },
            WriteRecord {
                slot: 200,
                write_version: 2,
                pubkey: pk,
                account: account(owner, 9, b"v2"),
            },
            // Different owner — should never land in the store.
            WriteRecord {
                slot: 150,
                write_version: 3,
                pubkey: dropped,
                account: account(Pubkey::new_unique(), 1, b"nope"),
            },
        ];

        persist_program_accounts(&store, &writes, &owner, 100, 200)
            .await
            .expect("persist");

        // Per-slot history: as-of 150 sees the slot-100 value, as-of 250 the slot-200 one.
        let at_150 = store
            .get_account_info(&pk.to_bytes(), 150)
            .await
            .unwrap()
            .expect("pk present at 150");
        assert_eq!(at_150.lamports, 5);
        assert_eq!(at_150.data, b"v1");

        let at_250 = store
            .get_account_info(&pk.to_bytes(), 250)
            .await
            .unwrap()
            .expect("pk present at 250");
        assert_eq!(at_250.lamports, 9);
        assert_eq!(at_250.data, b"v2");

        // The wrong-owner write was filtered out entirely.
        assert!(
            store
                .get_account_info(&dropped.to_bytes(), 300)
                .await
                .unwrap()
                .is_none(),
            "non-target account must not be persisted"
        );

        // The range reads as covered.
        assert!(store.is_covered(120, 180).await.unwrap());
    }
}
