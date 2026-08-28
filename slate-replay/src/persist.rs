use std::collections::HashMap;

use slate_store::{AccountUpdateInsert, ClickHouseClient, StoreResult};
use solana_account::{AccountSharedData, ReadableAccount};
use solana_pubkey::Pubkey;

use crate::WriteRecord;

// Owner-filter the write log to the indexed program; one row per (account, slot) for per-slot history.
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

// Program-owned accounts stamped at s_snap (write version 0): the baseline so an untouched account isn't read as "does not exist".
pub fn baseline_rows(
    accounts: &HashMap<Pubkey, (AccountSharedData, u64)>,
    owner: &Pubkey,
    s_snap: u64,
) -> Vec<AccountUpdateInsert> {
    accounts
        .iter()
        .filter(|(_, (account, _))| account.owner() == owner)
        .map(|(pubkey, (account, _))| AccountUpdateInsert {
            pubkey: pubkey.to_bytes(),
            slot: s_snap,
            write_version: 0,
            owner: account.owner().to_bytes(),
            lamports: account.lamports(),
            executable: account.executable() as u8,
            rent_epoch: account.rent_epoch(),
            data: account.data().to_vec(),
            txn_signature: None,
        })
        .collect()
}

// Persist program writes and record [lo, hi] covered so as-of reads there are Exact, not Uncertain.
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
            // Not owned by the target, must be dropped.
            WriteRecord {
                slot: 5,
                write_version: 2,
                pubkey: Pubkey::new_unique(),
                account: account(other, 20, b""),
            },
            // Same pk changing again at a later slot, kept as its own row.
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

    #[test]
    fn baseline_rows_are_owner_filtered_and_stamped_at_snapshot() {
        let owner = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let pk = Pubkey::new_unique();

        let mut accounts = HashMap::new();
        // Snapshot slot 150, but the baseline must be stamped at S_snap (200).
        accounts.insert(pk, (account(owner, 42, b"x"), 150u64));
        // Wrong owner, dropped.
        accounts.insert(Pubkey::new_unique(), (account(other, 99, b"y"), 150u64));

        let rows = baseline_rows(&accounts, &owner, 200);
        assert_eq!(rows.len(), 1, "only the program's accounts");
        assert_eq!(rows[0].pubkey, pk.to_bytes());
        assert_eq!(
            rows[0].slot, 200,
            "stamped at S_snap, not the record's slot 150"
        );
        assert_eq!(rows[0].write_version, 0);
        assert_eq!(rows[0].lamports, 42);
        assert_eq!(rows[0].owner, owner.to_bytes());
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
            // Different owner, should never land in the store.
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
