//! Snapshot loader: read a full-snapshot `.tar.zst` and pull out the accounts to
//! seed the replay bank.
//!
//! Manifest-free by design. We walk the `accounts/<slot>.<id>` AppendVec files
//! directly and keep the highest-slot value per pubkey. The 136-byte per-account
//! record layout (StoredMeta + AccountMeta + obsolete hash) is stable across agave
//! versions: verified byte-identical from 1.18 through 3.1, which brackets the 2.x
//! that wrote the epoch-808 snapshot. The archiver writes each storage at its
//! `current_len` (agave-snapshots `archive.rs` sets the tar entry to the storage
//! reader's length, not the on-disk capacity), so a snapshot file has no trailing
//! slack to bound. The manifest bincode (bank fields) is what drifts, so we never
//! parse it; the snapshot's slot comes from the archive filename.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    path::Path,
};

use anyhow::{Context, Result};
use solana_account::{Account, AccountSharedData, ReadableAccount};
use solana_hash::Hash;
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
/// the first record that doesn't fit within `bytes`. The archiver trims each
/// storage to `current_len`, so a real snapshot file ends exactly on a record
/// boundary with no slack; the doesn't-fit and 10-MiB checks are a defensive guard
/// for a truncated or corrupt download, not the normal stop condition.
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

/// The bank-hash anchors read from a snapshot's manifest bank fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestHashes {
    /// The snapshot slot.
    pub slot: u64,
    /// `bank_hash(slot)` — the state-commitment hash, the keystone verification
    /// target for a computed bank hash.
    pub bank_hash: Hash,
    /// `slot - 1`.
    pub parent_slot: u64,
    /// `bank_hash(slot - 1)`. Equals the newest SlotHashes entry the snapshot
    /// carries, an independent cross-check that the parse found the right fields.
    pub parent_hash: Hash,
}

/// Read the bank hash and parent bank hash from a full snapshot's manifest
/// (`snapshots/<slot>/<slot>`, the first non-account entry). The bank fields hold
/// `hash`(=bank hash), `parent_hash`, `parent_slot` as three consecutive fields,
/// but they sit behind the variable-length `blockhash_queue` and `ancestors`, which
/// are painful to parse. So instead of parsing from the start, we locate the
/// `parent_slot` value (= `snapshot_slot - 1`) and forward-parse the fixed scalar
/// layout that follows it, requiring it to land exactly on the `slot` field
/// (= `snapshot_slot`). A coincidental byte match won't have a self-consistent
/// layout that lands on `slot`, so it's rejected; `bank_hash`/`parent_hash` are the
/// two 32-byte fields immediately before `parent_slot`. Only the manifest front is
/// read — the ~1 GiB stakes/epoch-stakes tail is never pulled into memory.
pub fn read_manifest_hashes<R: Read>(reader: R, snapshot_slot: u64) -> Result<ManifestHashes> {
    let parent_slot = snapshot_slot
        .checked_sub(1)
        .context("snapshot at slot 0 has no parent bank hash")?;

    let decoder = zstd::Decoder::new(reader).context("open zstd stream")?;
    let mut archive = tar::Archive::new(decoder);
    let manifest_path = format!("snapshots/{snapshot_slot}/{snapshot_slot}");

    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("tar entry")?;
        let is_manifest = entry
            .path()
            .context("entry path")?
            .to_str()
            .is_some_and(|p| p == manifest_path);
        if !is_manifest {
            continue;
        }
        // The bank hash is a few KB in (past blockhash_queue/ancestors); 8 MiB is far
        // more than enough and keeps the huge stakes tail out of memory.
        let mut front = Vec::new();
        (&mut entry)
            .take(8 * 1024 * 1024)
            .read_to_end(&mut front)
            .context("read manifest front")?;
        return parse_manifest_hashes(&front, snapshot_slot, parent_slot);
    }
    anyhow::bail!("manifest {manifest_path} not found in snapshot")
}

/// Locate the bank fields in the manifest `front` by anchoring on `parent_slot` and
/// requiring the scalar layout after it to land on `slot`. See [`read_manifest_hashes`].
fn parse_manifest_hashes(front: &[u8], slot: u64, parent_slot: u64) -> Result<ManifestHashes> {
    let ps_le = parent_slot.to_le_bytes();
    let mut result: Option<ManifestHashes> = None;
    for o in 64..front.len().saturating_sub(8) {
        if front[o..o + 8] != ps_le || forward_parse_slot(front, o) != Some(slot) {
            continue;
        }
        let hashes = ManifestHashes {
            slot,
            bank_hash: Hash::new_from_array(front[o - 64..o - 32].try_into().unwrap()),
            parent_slot,
            parent_hash: Hash::new_from_array(front[o - 32..o].try_into().unwrap()),
        };
        if result.replace(hashes).is_some() {
            anyhow::bail!("ambiguous bank fields in manifest (two candidates landed on slot {slot})");
        }
    }
    result.context("bank fields not found in manifest front (snapshot format drift?)")
}

/// Parse the `SerializableVersionedBank` scalar fields between `parent_slot` (at
/// `o`) and `slot`, returning the `slot` value, or `None` if the walk leaves the
/// buffer or hits an implausible field. Landing on the right `slot` is what confirms
/// `o` really is the `parent_slot` field and not a coincidental byte match.
fn forward_parse_slot(b: &[u8], o: usize) -> Option<u64> {
    let read_u64 = |p: usize| b.get(p..p + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()));
    let mut p = o + 8; // past parent_slot
    let hard_forks = read_u64(p)?; // Vec<(Slot, usize)> length
    if hard_forks > 1024 {
        return None; // wildly implausible ⇒ not the real field
    }
    p += 8 + (hard_forks as usize).checked_mul(16)?; // len prefix + entries
    // transaction_count, tick_height, signature_count, capitalization, max_tick_height
    p = p.checked_add(8 * 5)?;
    match b.get(p)? {
        0 => p += 1,     // hashes_per_tick: None
        1 => p += 1 + 8, // Some(u64)
        _ => return None,
    }
    // ticks_per_slot(8) + ns_per_slot(u128=16) + genesis_creation_time(8)
    // + slots_per_year(8) + accounts_data_len(8)
    p = p.checked_add(8 + 16 + 8 + 8 + 8)?;
    read_u64(p) // slot
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

    #[test]
    fn reads_bank_hashes_from_the_manifest() {
        // The embedded snapshot is at slot 200.
        let mh = read_manifest_hashes(SNAPSHOT, 200).expect("read manifest hashes");
        assert_eq!(mh.slot, 200);
        assert_eq!(mh.parent_slot, 199);
        assert_ne!(mh.bank_hash, Hash::default(), "bank hash should be populated");
        assert_ne!(mh.parent_hash, Hash::default(), "parent hash should be populated");
    }

    #[test]
    fn manifest_parent_hash_matches_the_snapshot_slothashes() {
        // Two independent reads must agree: the manifest's parent_hash field and the
        // newest entry of the SlotHashes sysvar account (both are bank_hash(slot-1)).
        let mh = read_manifest_hashes(SNAPSHOT, 200).expect("read manifest hashes");
        let accounts = load_accounts(SNAPSHOT, None, None).unwrap();
        let slot_hashes_id: Pubkey = "SysvarS1otHashes111111111111111111111111111"
            .parse()
            .unwrap();
        let (sh, _) = accounts
            .get(&slot_hashes_id)
            .expect("SlotHashes sysvar present in the snapshot");
        // SlotHashes data = bincode Vec<(Slot, Hash)>, newest first: u64 len, then
        // entries of (u64 slot, 32-byte hash).
        let data = sh.data();
        let len = u64::from_le_bytes(data[0..8].try_into().unwrap());
        assert!(len > 0, "SlotHashes should carry entries");
        let newest_slot = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let newest_hash = Hash::new_from_array(data[16..48].try_into().unwrap());
        assert_eq!(newest_slot, mh.parent_slot, "newest SlotHashes slot = parent slot");
        assert_eq!(
            newest_hash, mh.parent_hash,
            "newest SlotHashes bank hash must equal the manifest parent_hash"
        );
    }

    #[test]
    #[ignore = "needs the local mainnet snapshot at /Users/mctursh/slate-data"]
    fn reads_the_real_mainnet_bank_hash() {
        let path = "/Users/mctursh/slate-data/\
                    snapshot-349047024-Cv8fHRuDLaRVhB8YTXGMxbMpZBC1BDGpN5MN99GFGqUv.tar.zst";
        let f = File::open(path).expect("open the mainnet snapshot");
        let mh = read_manifest_hashes(f, 349047024).expect("read manifest hashes");
        let expected: Hash = "Cv87aY5YPjpDpWfEzbikfxyhthNmfYSJ1rZdbJfQ8gm6".parse().unwrap();
        assert_eq!(mh.bank_hash, expected, "mainnet bank_hash(s_snap)");
        assert_eq!(mh.parent_slot, 349047023);
    }
}
