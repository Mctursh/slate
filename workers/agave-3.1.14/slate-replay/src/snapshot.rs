use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    path::Path,
};

use anyhow::{Context, Result};
use slate_hash::LtHash;
use solana_account::{Account, AccountSharedData, ReadableAccount};
use solana_hash::Hash;
use solana_inflation::Inflation;
use solana_pubkey::Pubkey;

use crate::{ReplayBank, store::AccountStore};

// Per-account header: StoredMeta (write_version, data_len, pubkey) + AccountMeta (lamports, rent_epoch, owner, executable+pad) + obsolete 32B hash. Layout byte-identical agave 1.18, 3.1, which brackets the 2.x that wrote the epoch-808 snapshot.
const STORE_META_OVERHEAD: usize = 136;
const OFF_DATA_LEN: usize = 8;
const OFF_PUBKEY: usize = 16;
const OFF_LAMPORTS: usize = 48;
const OFF_RENT_EPOCH: usize = 56;
const OFF_OWNER: usize = 64;
const OFF_EXECUTABLE: usize = 96;
// Sanity cap so a corrupt record can't make us allocate wildly; Solana's max account data is 10 MiB.
const MAX_ACCOUNT_DATA: usize = 10 * 1024 * 1024;

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn read_pubkey(bytes: &[u8], at: usize) -> Pubkey {
    Pubkey::new_from_array(bytes[at..at + 32].try_into().unwrap())
}

// The archiver trims each storage to current_len, so a real file ends on a record boundary; the doesn't-fit and 10-MiB checks guard a truncated/corrupt download, not the normal stop.
pub fn parse_append_vec(bytes: &[u8]) -> Vec<(Pubkey, AccountSharedData)> {
    let mut accounts = Vec::new();
    let mut offset = 0usize;
    // Checked arithmetic: `bytes` is an untrusted download, so a corrupt record ends the walk instead of overflowing.
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

fn account_file_slot(path: &Path) -> Option<u64> {
    if path.parent()?.file_name()? != "accounts" {
        return None;
    }
    path.file_name()?.to_str()?.split('.').next()?.parse().ok()
}

// Highest-slot-wins; a zero-lamport account at its top slot is deleted on-chain, so dropped.
// footprint is the seed read-set, NOT owner-filtered (owner-scoping yields stale reads); keep_owned_by also keeps every program-owned account as the persist baseline. Kept if in footprint OR owned by keep_owned_by.
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
            // Filter during parse to bound memory: keep footprint (seed) or program-owned (persist).
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

    // Drop dead (zero-lamport) accounts: on-chain they're purged and read as the default, so seeding the stale AppendVec record would diverge from the chain.
    accounts.retain(|_, (account, _)| account.lamports() > 0);
    Ok(accounts)
}

// Disk-store path for ranges too big for load_accounts's HashMap; same filter + highest-slot-wins, but dedup is against the store (a get per account).
// Zero-lamport records are kept as tombstones carrying their slot so a later lower-slot record can't resurrect a closed account; the read path filters them back out.
#[allow(clippy::type_complexity)]
pub fn stream_into_store<R: Read>(
    reader: R,
    store: &mut dyn AccountStore,
    footprint: Option<&HashSet<Pubkey>>,
    keep_owned_by: Option<&Pubkey>,
    stake_keys: Option<&mut HashSet<Pubkey>>,
) -> Result<(usize, HashMap<Pubkey, (AccountSharedData, u64)>)> {
    let decoder = zstd::Decoder::new(reader).context("open zstd stream")?;
    let mut archive = tar::Archive::new(decoder);
    let mut writes = 0usize;
    let mut owned: HashMap<Pubkey, (AccountSharedData, u64)> = HashMap::new();
    // A boundary needs every stake account, not just the ones the range's blocks touch.
    let mut stake_keys = stake_keys;

    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("tar entry")?;
        let path = entry.path().context("entry path")?.into_owned();
        let Some(slot) = account_file_slot(&path) else {
            continue;
        };
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).context("read account file")?;
        for (pubkey, account) in parse_append_vec(&bytes) {
            let is_owned = keep_owned_by.is_some_and(|owner| account.owner() == owner);
            // A crossing needs every stake AND vote account: a validator that never votes inside the
            // window is absent from the footprint, and every delegation behind it would be dropped.
            let crossing = stake_keys.is_some();
            let is_stake = crossing && *account.owner() == solana_sdk_ids::stake::id();
            let is_vote = crossing && *account.owner() == solana_sdk_ids::vote::id();
            let keep =
                is_owned || is_stake || is_vote || footprint.is_none_or(|f| f.contains(&pubkey));
            if !keep {
                continue;
            }
            if let Some(keys) = stake_keys.as_deref_mut()
                && is_stake
            {
                keys.insert(pubkey);
            }
            match store.get(&pubkey) {
                Some((_, prev)) if prev >= slot => {}
                _ => {
                    // Track program-owned live accounts as the baseline; drop any now closed or reowned.
                    if is_owned && account.lamports() > 0 {
                        owned.insert(pubkey, (account.clone(), slot));
                    } else {
                        owned.remove(&pubkey);
                    }
                    store.put(pubkey, account, slot);
                    writes += 1;
                }
            }
        }
    }
    // Commit the final partial buffer so the whole seed is on disk.
    store.flush();
    Ok((writes, owned))
}

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

// footprint is the pre-scan read-set (NOT an owner filter); None loads everything, fine for a small snapshot.
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ManifestFields {
    pub slot: u64,
    // bank_hash(slot): the keystone verification target for a computed bank hash.
    pub bank_hash: Hash,
    pub parent_slot: u64,
    // bank_hash(slot-1); equals the snapshot's newest SlotHashes entry, an independent cross-check the parse found the right fields.
    pub parent_hash: Hash,
    pub capitalization: u64,
    pub slots_per_year: f64,
    pub epoch: u64,
    pub block_height: u64,
    // Read, never defaulted: mainnet runs foundation 0.0 / term 0.0 where Inflation::default() is
    // 0.05 / 7.0, and validator(year) = total(year) - foundation(year), so the default underpays by 5%.
    // None on format drift; the bank-hash path doesn't need it, the reward math must require it.
    pub inflation: Option<Inflation>,
}

// The manifest bincode drifts across versions, so we never decode it; the bank fields also sit behind variable-length blockhash_queue/ancestors. Instead we anchor on parent_slot (= slot-1) and forward-parse the fixed scalar layout, requiring it to land on slot (a coincidental byte match won't).
// bank_hash/parent_hash are the two 32B fields just before parent_slot; only the manifest front is read, never the ~1 GiB stakes tail.
pub fn read_manifest_fields<R: Read>(reader: R, snapshot_slot: u64) -> Result<ManifestFields> {
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
        // The bank hash is a few KB in; 8 MiB is far more than enough and keeps the huge stakes tail out of memory.
        let mut front = Vec::new();
        (&mut entry)
            .take(8 * 1024 * 1024)
            .read_to_end(&mut front)
            .context("read manifest front")?;
        return parse_manifest_fields(&front, snapshot_slot, parent_slot);
    }
    anyhow::bail!("manifest {manifest_path} not found in snapshot")
}

fn parse_manifest_fields(front: &[u8], slot: u64, parent_slot: u64) -> Result<ManifestFields> {
    let ps_le = parent_slot.to_le_bytes();
    let mut result: Option<ManifestFields> = None;
    for o in 64..front.len().saturating_sub(8) {
        if front[o..o + 8] != ps_le {
            continue;
        }
        let Some(scalars) = forward_parse_scalars(front, o) else {
            continue;
        };
        if scalars.slot != slot {
            continue;
        }
        let fields = ManifestFields {
            slot,
            bank_hash: Hash::new_from_array(front[o - 64..o - 32].try_into().unwrap()),
            parent_slot,
            parent_hash: Hash::new_from_array(front[o - 32..o].try_into().unwrap()),
            capitalization: scalars.capitalization,
            slots_per_year: scalars.slots_per_year,
            epoch: scalars.epoch,
            block_height: scalars.block_height,
            inflation: scalars.inflation,
        };
        if result.replace(fields).is_some() {
            anyhow::bail!(
                "ambiguous bank fields in manifest (two candidates landed on slot {slot})"
            );
        }
    }
    result.context("bank fields not found in manifest front (snapshot format drift?)")
}

// agave loads its StakesCache from the manifest rather than scanning accounts (Stakes::new iterates
// stakes.stake_delegations), so this is the authoritative delegation key set at the snapshot slot.
pub fn read_manifest_stakes_cache<R: Read>(
    reader: R,
    snapshot_slot: u64,
) -> Result<(HashSet<Pubkey>, HashMap<Pubkey, Pubkey>)> {
    let parent_slot = snapshot_slot.checked_sub(1).context("slot 0")?;
    let decoder = zstd::Decoder::new(reader).context("open zstd")?;
    let mut archive = tar::Archive::new(decoder);
    let manifest_path = format!("snapshots/{snapshot_slot}/{snapshot_slot}");
    for entry in archive.entries().context("tar")? {
        let mut entry = entry.context("entry")?;
        if entry.path().context("path")?.to_str() != Some(manifest_path.as_str()) {
            continue;
        }
        let mut buf = Vec::new();
        (&mut entry)
            .take(768 * 1024 * 1024)
            .read_to_end(&mut buf)
            .context("read")?;
        return parse_stakes_cache(&buf, snapshot_slot, parent_slot);
    }
    anyhow::bail!("manifest not found")
}

pub fn read_manifest_stake_delegations<R: Read>(
    reader: R,
    snapshot_slot: u64,
) -> Result<HashSet<Pubkey>> {
    let parent_slot = snapshot_slot
        .checked_sub(1)
        .context("snapshot at slot 0 has no parent")?;
    let decoder = zstd::Decoder::new(reader).context("open zstd stream")?;
    let mut archive = tar::Archive::new(decoder);
    let manifest_path = format!("snapshots/{snapshot_slot}/{snapshot_slot}");

    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("tar entry")?;
        if entry.path().context("entry path")?.to_str() != Some(manifest_path.as_str()) {
            continue;
        }
        let mut buf = Vec::new();
        (&mut entry)
            .take(768 * 1024 * 1024)
            .read_to_end(&mut buf)
            .context("read manifest")?;
        return parse_stakes_cache(&buf, snapshot_slot, parent_slot)
            .map(|(_, d)| d.into_keys().collect());
    }
    anyhow::bail!("manifest {manifest_path} not found")
}

fn parse_stakes_cache(
    b: &[u8],
    slot: u64,
    parent_slot: u64,
) -> Result<(HashSet<Pubkey>, HashMap<Pubkey, Pubkey>)> {
    let ps = parent_slot.to_le_bytes();
    let anchor = (64..b.len().saturating_sub(8))
        .find(|&o| b[o..o + 8] == ps && forward_parse_scalars(b, o).is_some_and(|s| s.slot == slot))
        .context("bank fields not found")?;
    let slot_pos = slot_position(b, anchor).context("slot position")?;

    let read_u64 = |p: usize| -> Option<u64> {
        b.get(p..p + 8)
            .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
    };
    let mut p = slot_pos + SLOT_TO_INFLATION + 48;

    let votes = read_u64(p).context("vote_accounts len")?;
    anyhow::ensure!(votes < 1_000_000, "implausible vote count {votes}");
    p += 8;
    let mut vote_keys = HashSet::with_capacity(votes as usize);
    for _ in 0..votes {
        vote_keys.insert(Pubkey::try_from(b.get(p..p + 32).context("vote key")?)?);
        p += 32 + 8 + 8;
        let data_len = read_u64(p).context("vote data len")? as usize;
        anyhow::ensure!(
            data_len < 10 * 1024 * 1024,
            "implausible vote data {data_len}"
        );
        p += 8 + data_len + 32 + 1 + 8;
    }

    let count = read_u64(p).context("stake_delegations len")?;
    anyhow::ensure!(count < 10_000_000, "implausible delegation count {count}");
    p += 8;
    let mut delegations = HashMap::with_capacity(count as usize);
    for _ in 0..count {
        let key = Pubkey::try_from(b.get(p..p + 32).context("delegation key")?)?;
        let voter = Pubkey::try_from(b.get(p + 32..p + 64).context("voter")?)?;
        delegations.insert(key, voter);
        p += 32 + 64;
    }
    Ok((vote_keys, delegations))
}

fn slot_position(b: &[u8], o: usize) -> Option<usize> {
    let read_u64 = |p: usize| {
        b.get(p..p + 8)
            .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
    };
    let mut p = o + 8;
    let hard_forks = read_u64(p)?;
    p += 8 + (hard_forks as usize).checked_mul(16)?;
    p = p.checked_add(8 * 5)?;
    match b.get(p)? {
        0 => p += 1,
        1 => p += 1 + 8,
        _ => return None,
    }
    p.checked_add(8 + 16 + 8 + 8 + 8)
}

struct Scalars {
    slot: u64,
    epoch: u64,
    block_height: u64,
    capitalization: u64,
    slots_per_year: f64,
    inflation: Option<Inflation>,
}

// Bytes from `slot` to `inflation`, in declaration order: epoch(8) block_height(8) collector_id(32)
// collector_fees(8) fee_calculator(8) fee_rate_governor(41) collected_rent(8) rent_collector epoch_schedule.
const EPOCH_SCHEDULE_LEN: usize = 8 + 8 + 1 + 8 + 8;
const RENT_LEN: usize = 8 + 8 + 1;
const RENT_COLLECTOR_LEN: usize = 8 + EPOCH_SCHEDULE_LEN + 8 + RENT_LEN;
const SLOT_TO_INFLATION: usize =
    8 + 8 + 32 + 8 + 8 + 41 + 8 + RENT_COLLECTOR_LEN + EPOCH_SCHEDULE_LEN;

// Walk the SerializableVersionedBank scalars from parent_slot to slot; landing on the right slot confirms `o` is parent_slot, not a coincidental byte match. Inflation sits past slot behind fixed-size structs, so it is range-checked rather than trusted.
fn forward_parse_scalars(b: &[u8], o: usize) -> Option<Scalars> {
    let read_u64 = |p: usize| {
        b.get(p..p + 8)
            .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
    };
    let read_f64 = |p: usize| {
        b.get(p..p + 8)
            .map(|s| f64::from_le_bytes(s.try_into().unwrap()))
    };
    let mut p = o + 8; // past parent_slot
    let hard_forks = read_u64(p)?; // Vec<(Slot, usize)> length
    if hard_forks > 1024 {
        return None; // wildly implausible ⇒ not the real field
    }
    p += 8 + (hard_forks as usize).checked_mul(16)?; // len prefix + entries
    // transaction_count, tick_height, signature_count, [capitalization], max_tick_height
    let capitalization = read_u64(p.checked_add(8 * 3)?)?;
    p = p.checked_add(8 * 5)?;
    match b.get(p)? {
        0 => p += 1,     // hashes_per_tick: None
        1 => p += 1 + 8, // Some(u64)
        _ => return None,
    }
    // ticks_per_slot(8) + ns_per_slot(u128=16) + genesis_creation_time(8) + [slots_per_year] + accounts_data_len(8)
    let slots_per_year = read_f64(p.checked_add(8 + 16 + 8)?)?;
    p = p.checked_add(8 + 16 + 8 + 8 + 8)?;
    let slot = read_u64(p)?;
    let epoch = read_u64(p.checked_add(8)?)?;
    let block_height = read_u64(p.checked_add(16)?)?;

    let i = p.checked_add(SLOT_TO_INFLATION)?;
    let inflation = (|| {
        // Inflation has a private 6th f64 after foundation_term (48 bytes on the wire), so no struct literal.
        let mut curve = Inflation::default();
        curve.initial = read_f64(i)?;
        curve.terminal = read_f64(i + 8)?;
        curve.taper = read_f64(i + 16)?;
        curve.foundation = read_f64(i + 24)?;
        curve.foundation_term = read_f64(i + 32)?;
        plausible_inflation(&curve).then_some(curve)
    })();

    Some(Scalars {
        slot,
        epoch,
        block_height,
        capitalization,
        slots_per_year,
        inflation,
    })
}

// A wrong offset gives NaN or absurd rates, not a plausible curve.
fn plausible_inflation(i: &Inflation) -> bool {
    let all = [
        i.initial,
        i.terminal,
        i.taper,
        i.foundation,
        i.foundation_term,
    ];
    all.iter().all(|v| v.is_finite() && *v >= 0.0)
        && i.initial > 0.0
        && i.initial <= 1.0
        && i.terminal <= i.initial
        && i.taper > 0.0
        && i.taper <= 1.0
        && i.foundation <= 1.0
        && i.foundation_term <= 100.0
}

// accounts_lt_hash trailer when present: 1-byte Option tag (0x01) + 1024 LE u16 lanes (2048B).
const LT_HASH_TRAILER: usize = 1 + 2048;

// In v2.2.x accounts_lt_hash is the manifest's LAST field, so it's the final 2049 bytes (0x01 tag + 2048 lattice bytes); None if serialized None (feature inactive at that slot).
// Streams to the tail keeping only the last bytes, never the ~1 GiB body.
pub fn read_manifest_lt_hash<R: Read>(reader: R, snapshot_slot: u64) -> Result<Option<LtHash>> {
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
        // Roll a window of the last LT_HASH_TRAILER bytes across the streamed manifest to reach its tail.
        let mut tail = Vec::with_capacity(LT_HASH_TRAILER + 64 * 1024);
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = entry.read(&mut buf).context("read manifest")?;
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > LT_HASH_TRAILER {
                tail.drain(0..tail.len() - LT_HASH_TRAILER);
            }
        }
        return Ok(parse_lt_hash_trailer(&tail));
    }
    anyhow::bail!("manifest {manifest_path} not found in snapshot")
}

// Trailer decode: 0x01 + 2048 LE bytes ⇒ Some(LtHash); 0x00 or too-short ⇒ None.
fn parse_lt_hash_trailer(tail: &[u8]) -> Option<LtHash> {
    if tail.len() < LT_HASH_TRAILER || tail[tail.len() - LT_HASH_TRAILER] != 1 {
        return None;
    }
    Some(LtHash::from_bytes(
        tail[tail.len() - LtHash::NUM_BYTES..].try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real solana-test-validator full snapshot (slot 200), embedded so the test stays offline.
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
    fn stream_into_store_agrees_with_load_accounts() {
        use crate::store::MemStore;

        let map = load_accounts(SNAPSHOT, None, None).unwrap();
        let mut store = MemStore::default();
        stream_into_store(SNAPSHOT, &mut store, None, None, None).unwrap();

        // Every live account load_accounts kept is byte-identical in the streamed store.
        for (pk, (account, slot)) in &map {
            let (got, got_slot) = store.get(pk).expect("live account present after stream");
            assert_eq!(got_slot, *slot);
            assert_eq!(got.lamports(), account.lamports());
            assert_eq!(got.data(), account.data());
            assert_eq!(got.owner(), account.owner());
            assert_eq!(got.executable(), account.executable());
        }
        assert!(!map.is_empty());
    }

    #[test]
    fn stream_into_store_collects_the_owned_baseline() {
        use crate::store::MemStore;

        // Sysvar accounts (Clock, Rent, ...) are owned by the sysvar id in the snapshot.
        let owner = solana_sdk_ids::sysvar::id();
        let empty = HashSet::new();
        // load_accounts owner-scoped (empty footprint) is the reference baseline set.
        let reference = load_accounts(SNAPSHOT, Some(&empty), Some(&owner)).unwrap();
        assert!(!reference.is_empty());

        let mut store = MemStore::default();
        let (_writes, owned) =
            stream_into_store(SNAPSHOT, &mut store, None, Some(&owner), None).unwrap();

        // The baseline collected during the seed matches load_accounts' owner-scoped set.
        assert_eq!(owned.len(), reference.len());
        for (pk, (account, slot)) in &reference {
            let (got, got_slot) = owned.get(pk).expect("owned baseline account present");
            assert_eq!(got_slot, slot);
            assert_eq!(got.lamports(), account.lamports());
            assert_eq!(got.owner(), account.owner());
        }
    }

    #[test]
    fn reads_bank_hashes_from_the_manifest() {
        // The embedded snapshot is at slot 200.
        let mh = read_manifest_fields(SNAPSHOT, 200).expect("read manifest hashes");
        assert_eq!(mh.slot, 200);
        assert_eq!(mh.parent_slot, 199);
        assert_ne!(
            mh.bank_hash,
            Hash::default(),
            "bank hash should be populated"
        );
        assert_ne!(
            mh.parent_hash,
            Hash::default(),
            "parent hash should be populated"
        );
    }

    #[test]
    fn manifest_parent_hash_matches_the_snapshot_slothashes() {
        // Two independent reads must agree: the manifest's parent_hash and the newest SlotHashes entry (both are bank_hash(slot-1)).
        let mh = read_manifest_fields(SNAPSHOT, 200).expect("read manifest hashes");
        let accounts = load_accounts(SNAPSHOT, None, None).unwrap();
        let slot_hashes_id: Pubkey = "SysvarS1otHashes111111111111111111111111111"
            .parse()
            .unwrap();
        let (sh, _) = accounts
            .get(&slot_hashes_id)
            .expect("SlotHashes sysvar present in the snapshot");
        // SlotHashes data = bincode Vec<(Slot, Hash)> newest-first: u64 len, then (u64 slot, 32B hash) entries.
        let data = sh.data();
        let len = u64::from_le_bytes(data[0..8].try_into().unwrap());
        assert!(len > 0, "SlotHashes should carry entries");
        let newest_slot = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let newest_hash = Hash::new_from_array(data[16..48].try_into().unwrap());
        assert_eq!(
            newest_slot, mh.parent_slot,
            "newest SlotHashes slot = parent slot"
        );
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
        let mh = read_manifest_fields(f, 349047024).expect("read manifest hashes");
        let expected: Hash = "Cv87aY5YPjpDpWfEzbikfxyhthNmfYSJ1rZdbJfQ8gm6"
            .parse()
            .unwrap();
        assert_eq!(mh.bank_hash, expected, "mainnet bank_hash(s_snap)");
        assert_eq!(mh.parent_slot, 349047023);
    }
}

#[cfg(test)]
mod manifest_field_tests {
    use super::*;

    // Ground truth against the real epoch-807 snapshot: inflation sits ~220 bytes past the validated
    // `slot` anchor, so this is what proves that offset arithmetic. Expected values are mainnet's
    // genesis curve, cross-checked against getInflationGovernor.
    #[test]
    #[ignore = "needs the local mainnet snapshot at /Users/mctursh/slate-data"]
    fn reads_the_reward_scalars_from_the_mainnet_manifest() {
        let path = "/Users/mctursh/slate-data/\
                    snapshot-349047024-Cv8fHRuDLaRVhB8YTXGMxbMpZBC1BDGpN5MN99GFGqUv.tar.zst";
        let slot = 349_047_024;
        let m = read_manifest_fields(File::open(path).unwrap(), slot).unwrap();

        assert_eq!(m.slot, slot);
        assert!(
            m.capitalization > 500_000_000 * 1_000_000_000,
            "mainnet supply is well over 500M SOL, got {} lamports",
            m.capitalization
        );
        assert!(
            m.slots_per_year > 60_000_000.0 && m.slots_per_year < 90_000_000.0,
            "400ms slots put a year near 78.9M slots, got {}",
            m.slots_per_year
        );

        // Independent confirmation the offset chain landed right.
        assert_eq!(
            m.epoch,
            slot / 432_000,
            "manifest epoch disagrees with the slot"
        );

        // The slot-to-height gap is the count of slots ever skipped, so it only grows: live mainnet
        // showed 21,956,549 skipped by slot 445,721,748, and this earlier slot must be under that.
        const GAP_AT_445M: u64 = 445_721_748 - 423_765_199;
        assert!(
            m.block_height < slot && m.block_height > slot - GAP_AT_445M,
            "block_height {} out of bounds for slot {slot} (gap must be under {GAP_AT_445M})",
            m.block_height
        );
        let inf = m
            .inflation
            .expect("inflation curve parsed and passed the range check");
        assert_eq!(inf.initial, 0.08);
        assert_eq!(inf.terminal, 0.015);
        assert_eq!(inf.taper, 0.15);
        assert_eq!(
            inf.foundation, 0.0,
            "mainnet foundation is 0.0; Inflation::default() would be 0.05 and underpay by 5%"
        );
        assert_eq!(inf.foundation_term, 0.0);

        // Illustrative: agave derives the year via slot_in_year_for_inflation(), not a plain division.
        let year = slot as f64 / m.slots_per_year;
        assert_eq!(
            inf.validator(year),
            inf.total(year),
            "with foundation 0.0 the validator rate is the whole curve"
        );
    }
}

