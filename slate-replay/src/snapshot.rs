//! Snapshot loader: read a full-snapshot `.tar.zst` and pull out the accounts to
//! seed the replay bank.
//!
//! Manifest-free by design. We walk the `accounts/<slot>.<id>` AppendVec files
//! directly and keep the highest-slot value per pubkey. The 136-byte per-account
//! record layout is stable across agave versions; the manifest bincode (bank
//! fields) is what drifts, so we never parse it. The snapshot's slot comes from
//! the archive filename, not the manifest.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    path::Path,
};

use anyhow::{Context, Result};
use solana_account::{Account, AccountSharedData, ReadableAccount};
use solana_pubkey::Pubkey;

use crate::ReplayBank;

/// Fixed per-account header: `StoredMeta` (write_version u64, data_len u64,
/// pubkey), then `AccountMeta` (lamports u64, rent_epoch u64, owner, executable +
/// pad), then the now-obsolete 32-byte account hash. Matches agave
/// `STORE_META_OVERHEAD`.
const STORE_META_OVERHEAD: usize = 136;
const OFF_DATA_LEN: usize = 8;
const OFF_PUBKEY: usize = 16;
const OFF_LAMPORTS: usize = 48;
const OFF_RENT_EPOCH: usize = 56;
const OFF_OWNER: usize = 64;
const OFF_EXECUTABLE: usize = 96;
/// Sanity cap so a corrupt or slack record can't make us allocate wildly.
/// Solana's max account data is 10 MiB.
const MAX_ACCOUNT_DATA: usize = 10 * 1024 * 1024;

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn read_pubkey(bytes: &[u8], at: usize) -> Pubkey {
    Pubkey::new_from_array(bytes[at..at + 32].try_into().unwrap())
}

/// Walk one AppendVec's bytes into (pubkey, account) pairs. Stops at the end or at
/// the first record that doesn't fit within `bytes` (trailing slack past the
/// file's valid length, which we don't get from the manifest).
fn parse_append_vec(bytes: &[u8]) -> Vec<(Pubkey, AccountSharedData)> {
    let mut accounts = Vec::new();
    let mut offset = 0usize;
    // Checked arithmetic throughout: `bytes` is an untrusted downloaded snapshot,
    // so a corrupt record must never overflow (which would panic in debug or wrap
    // in release) — it just ends the walk. Every `offset + OFF_*` read below is
    // safely within `header_end`, so only the record-boundary math needs guarding.
    while let Some(header_end) = offset.checked_add(STORE_META_OVERHEAD) {
        if header_end > bytes.len() {
            break;
        }
        let data_len = read_u64(bytes, offset + OFF_DATA_LEN) as usize;
        if data_len > MAX_ACCOUNT_DATA {
            break;
        }
        let Some(end) = header_end.checked_add(data_len) else {
            break;
        };
        if end > bytes.len() {
            break;
        }
        let pubkey = read_pubkey(bytes, offset + OFF_PUBKEY);
        let account = AccountSharedData::from(Account {
            lamports: read_u64(bytes, offset + OFF_LAMPORTS),
            rent_epoch: read_u64(bytes, offset + OFF_RENT_EPOCH),
            owner: read_pubkey(bytes, offset + OFF_OWNER),
            executable: bytes[offset + OFF_EXECUTABLE] != 0,
            data: bytes[header_end..end].to_vec(),
        });
        accounts.push((pubkey, account));
        // Records are 8-byte aligned; stop if the alignment step would overflow.
        let Some(next) = end.checked_next_multiple_of(8) else {
            break;
        };
        offset = next;
    }
    accounts
}

/// The slot an `accounts/<slot>.<id>` file belongs to, or `None` if `path` isn't
/// one of those.
fn account_file_slot(path: &Path) -> Option<u64> {
    if path.parent()?.file_name()? != "accounts" {
        return None;
    }
    path.file_name()?.to_str()?.split('.').next()?.parse().ok()
}

/// Load live accounts from a snapshot archive, keyed by pubkey with the slot it
/// was last written. Highest-slot value wins; a zero-lamport account at its
/// highest slot has been deleted and is dropped.
///
/// Two independent keep-filters, unioned in one pass:
/// - `footprint` — load only these pubkeys (for the mainnet snapshot's millions of
///   accounts, the range's read-set). It is the union of EVERY tx's account keys,
///   NOT owner/program-filtered: our program's txs read accounts other txs write,
///   so owner-scoping the seed would yield stale reads.
/// - `keep_owned_by` — ALSO keep every account this program owns, regardless of
///   footprint. That is the S_snap baseline the persist layer needs (the narrow
///   store), captured in the same scan. Owner-filtering here is only ever for what
///   we persist, never a narrowing of the seed.
///
/// An account is kept if it is in the footprint OR owned by `keep_owned_by`.
pub fn load_accounts<R: Read>(
    reader: R,
    footprint: Option<&HashSet<Pubkey>>,
    keep_owned_by: Option<&Pubkey>,
) -> Result<HashMap<Pubkey, (AccountSharedData, u64)>> {
    let decoder = zstd::Decoder::new(reader).context("open zstd stream")?;
    let mut archive = tar::Archive::new(decoder);
    let mut accounts: HashMap<Pubkey, (AccountSharedData, u64)> = HashMap::new();

    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("tar entry")?;
        let path = entry.path().context("entry path")?.into_owned();
        let Some(slot) = account_file_slot(&path) else {
            continue;
        };
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).context("read account file")?;
        for (pubkey, account) in parse_append_vec(&bytes) {
            // Keep during parse (bounds memory) anything in the footprint (seed) or
            // owned by the baseline program (persist).
            let keep = footprint.is_none_or(|f| f.contains(&pubkey))
                || keep_owned_by.is_some_and(|owner| account.owner() == owner);
            if !keep {
                continue;
            }
            match accounts.get(&pubkey) {
                Some((_, prev)) if *prev >= slot => {}
                _ => {
                    accounts.insert(pubkey, (account, slot));
                }
            }
        }
    }

    // Drop dead accounts. A zero-lamport account is purged on-chain, so a read of
    // it returns the default (system-owned, empty), NOT the stale owner/data the
    // AppendVec record may still carry — seeding that record would diverge from the
    // chain. Highest-slot-wins ran first, so this also correctly deletes an account
    // that was funded at an earlier slot and closed at a later one. Matches how a
    // validator treats dead accounts on snapshot load.
    accounts.retain(|_, (account, _)| account.lamports() > 0);
    Ok(accounts)
}

/// Convenience wrapper: load from a file path.
pub fn load_accounts_from_file(
    path: &Path,
    footprint: Option<&HashSet<Pubkey>>,
) -> Result<HashMap<Pubkey, (AccountSharedData, u64)>> {
    load_accounts(
        File::open(path).with_context(|| format!("open snapshot {path:?}"))?,
        footprint,
        None,
    )
}

/// Seed a [`ReplayBank`] from a snapshot. `footprint`, when given, is the set of
/// pubkeys to load — the union of the range's transaction account keys from the
/// pre-scan, NOT an owner filter (see [`load_accounts`]). `None` loads everything,
/// which is fine for a small snapshot.
pub fn seed_bank_from_snapshot<R: Read>(
    reader: R,
    footprint: Option<&HashSet<Pubkey>>,
) -> Result<ReplayBank> {
    let mut bank = ReplayBank::default();
    for (pubkey, (account, slot)) in load_accounts(reader, footprint, None)? {
        bank.insert(pubkey, account, slot);
    }
    Ok(bank)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real solana-test-validator full snapshot (slot 200), embedded so the test
    // stays offline. Genesis builtins + sysvars + the validator's own accounts.
    const SNAPSHOT: &[u8] = include_bytes!("test_snapshot.tar.zst");

    #[test]
    fn loads_accounts_from_a_real_snapshot() {
        let accounts = load_accounts(SNAPSHOT, None, None).unwrap();

        // Genesis alone seeds well over a hundred builtin/sysvar accounts.
        assert!(
            accounts.len() > 50,
            "expected a populated snapshot, got {}",
            accounts.len()
        );

        // The Clock sysvar is present in every snapshot, sysvar-owned, funded.
        let clock: Pubkey = "SysvarC1ock11111111111111111111111111111111"
            .parse()
            .unwrap();
        let (clock_account, _) = accounts.get(&clock).expect("clock sysvar present");
        assert!(clock_account.lamports() > 0);
        assert_eq!(*clock_account.owner(), solana_sdk_ids::sysvar::id());
        assert!(!clock_account.data().is_empty());

        // Nothing zero-lamport survived the filter.
        assert!(accounts.values().all(|(a, _)| a.lamports() > 0));
    }
}
