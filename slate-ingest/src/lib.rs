pub mod capture;
pub mod yellowstone;

use std::fs::read_dir;

use anyhow::anyhow;
use slate_store::{AccountUpdateInsert, ClickHouseClient};
use solana_account::ReadableAccount;
use solana_accounts_db::accounts_file::{AccountsFile, StorageAccess};

pub async fn read_snapshot_accounts(dir: &str) -> Result<u64, anyhow::Error> {
    let store = ClickHouseClient::new("http://localhost:8123");
    let mut accounts: Vec<AccountUpdateInsert> = Vec::new();
    let mut s_snap: u64 = u64::default();
    for entry in read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name() else {
            continue;
        };
        let Some(name) = name.to_str() else { continue };
        let slot = get_slot_from_filename(name)?;
        s_snap = s_snap.max(slot);
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
    if accounts.is_empty() {
        return Ok(0);
    }
    for a in &mut accounts {
        a.slot = s_snap
    }
    store.insert_accounts(&accounts).await?;
    store.record_coverage(s_snap, s_snap).await?;
    Ok(s_snap)
}

fn get_slot_from_filename(filename: &str) -> Result<u64, anyhow::Error> {
    let (slot, _id) = filename
        .split_once(".")
        .ok_or_else(|| anyhow::anyhow!("account file not <slot>.<id>: {filename}"))?;
    slot.parse::<u64>()
        .map_err(|_| anyhow!("account file slot not a u64: {filename}"))
}
