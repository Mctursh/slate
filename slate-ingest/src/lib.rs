//! slate-ingest — reads Solana snapshots (and later the live account stream) into the store.
//!
//! ## First task: read a snapshot's accounts
//!
//! You are NOT parsing the binary format. `solana-accounts-db`'s `AccountsFile` decodes it.
//! Lift two things from cloudbreak `crates/snapshot`:
//!
//! 1. Unpack the archive → a list of account files.
//!    Reference: `sidecar.rs::unpack_compressed_snapshot` → `Vec<AccountFileData>`, where
//!    `AccountFileData { path, size, slot, write_version }`.
//!    Simplest start: untar the `.tar.zst` yourself (`zstd` decode then `tar` extract); the
//!    account files land under `accounts/`, named `<slot>.<id>` — the slot is in the filename.
//!
//! 2. Iterate the accounts in each file.
//!    Reference: `lt_hash.rs` lines ~63–90:
//!    ```ignore
//!    use solana_accounts_db::accounts_file::{AccountsFile, StorageAccess};
//!    let af = AccountsFile::new_for_startup(&path, size, StorageAccess::default())?;
//!    let mut offsets = Vec::new();
//!    af.scan_accounts_without_data(|offset, _| offsets.push(offset))?;
//!    for offset in offsets {
//!        af.get_stored_account_callback(offset, |account| {
//!            // account.pubkey(), account.owner, account.lamports(), account.data(), ...
//!        });
//!    }
//!    ```
//!
//! Goal of step 1: print `(pubkey, lamports, owner, data.len())` for each account in a small
//! `solana-test-validator` snapshot. No ClickHouse yet — that comes after you can read.

// TODO(you): implement the snapshot reader here.

use std::fs::read_dir;

use anyhow::anyhow;
use slate_store::{AccountUpdateInsert, ClickHouseClient};
use solana_account::ReadableAccount;
use solana_accounts_db::accounts_file::{AccountsFile, StorageAccess};

pub async fn read_snapshot_accounts(dir: &str) -> Result<(), anyhow::Error> {
    let store = ClickHouseClient::new("http://localhost:8123");
    let mut accounts: Vec<AccountUpdateInsert> = Vec::new();
    for entry in read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name() else {
            continue;
        };
        let Some(name) = name.to_str() else { continue };
        let slot = get_slot_from_filename(name)?;
        // AppendVec DELETES its backing file on Drop (remove_file_on_drop defaults to true, and
        // there's no public opt-out). So read a throwaway copy: the copy gets deleted on drop,
        // the real snapshot file survives, and the loader stays non-destructive + re-runnable.
        let tmp = std::env::temp_dir().join(name);
        std::fs::copy(&path, &tmp)?;
        let size = std::fs::metadata(&tmp)?.len() as usize;
        let af = AccountsFile::new_for_startup(&tmp, size, StorageAccess::default())?;

        let mut offsets = Vec::new();

        af.scan_accounts_without_data(|offset, _| offsets.push(offset))?;
        for offset in offsets {
            af.get_stored_account_callback(offset, |account| {
                let account_entry = AccountUpdateInsert {
                    pubkey: account.pubkey.to_bytes(),
                    slot,
                    write_version: 0,
                    owner: account.owner.to_bytes(),
                    lamports: account.lamports,
                    executable: account.executable() as u8,
                    rent_epoch: account.rent_epoch,
                    data: account.data().to_vec(),
                };
                accounts.push(account_entry);
            });
        }
    }
    store.insert_accounts(&accounts).await?;

    Ok(())
}

fn get_slot_from_filename(filename: &str) -> Result<u64, anyhow::Error> {
    let (slot, _id) = filename
        .split_once(".")
        .ok_or_else(|| anyhow::anyhow!("account file not <slot>.<id>: {filename}"))?;
    slot.parse::<u64>()
        .map_err(|_| anyhow!("account file slot not a u64: {filename}"))
}
