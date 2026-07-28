pub mod baseline;
pub mod capture;
pub mod validate;
pub mod yellowstone;

use std::fs::read_dir;

use anyhow::anyhow;
use slate_store::{AccountUpdateInsert, ClickHouseClient};
use solana_account::ReadableAccount;
use solana_accounts_db::accounts_file::{AccountsFile, StorageAccess};

pub async fn read_snapshot_accounts(
    store: &ClickHouseClient,
    dir: &str,
    owner: &[u8; 32],
) -> Result<u64, anyhow::Error> {
    let mut s_snap: u64 = 0;
    for entry in read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        s_snap = s_snap.max(get_slot_from_filename(name)?);
    }
    if s_snap == 0 {
        return Ok(0);
    }

    for entry in read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        // AppendVec deletes its backing file on Drop -> read a throwaway copy.
        let tmp = std::env::temp_dir().join(name);
        std::fs::copy(&path, &tmp)?;
        let size = std::fs::metadata(&tmp)?.len() as usize;
        let af = AccountsFile::new_for_startup(&tmp, size, StorageAccess::default())?;

        let mut offsets = Vec::new();
        af.scan_accounts_without_data(|offset, _| offsets.push(offset))?;

        let mut batch: Vec<AccountUpdateInsert> = Vec::new();
        for offset in offsets {
            af.get_stored_account_callback(offset, |account| {
                if account.owner.to_bytes() != *owner {
                    return;
                }
                batch.push(AccountUpdateInsert {
                    pubkey: account.pubkey.to_bytes(),
                    slot: s_snap,
                    write_version: 0,
                    owner: account.owner.to_bytes(),
                    lamports: account.lamports,
                    executable: account.executable() as u8,
                    rent_epoch: account.rent_epoch,
                    data: account.data().to_vec(),
                });
            });
        }
        if !batch.is_empty() {
            store.insert_accounts(&batch).await?;
        }
    }

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
