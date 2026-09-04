use std::{collections::HashMap, path::Path};

use redb::{Database, Durability, TableDefinition};
use solana_account::{Account, AccountSharedData, ReadableAccount};
use solana_pubkey::Pubkey;

// pubkey -> (account, last-written slot). Send + Sync so the bank stays usable as the SVM's account callback.
pub trait AccountStore: Send + Sync {
    fn get(&self, key: &Pubkey) -> Option<(AccountSharedData, u64)>;
    fn put(&mut self, key: Pubkey, account: AccountSharedData, slot: u64);
    fn contains(&self, key: &Pubkey) -> bool;
    // Commit buffered writes; no-op for write-through stores.
    fn flush(&mut self);
    // Atomically flush buffered accounts + a resume checkpoint (slot + roll bytes) in one durable commit; no-op if the store can't resume.
    fn checkpoint_flush(&mut self, slot: u64, roll: &[u8]) -> anyhow::Result<()>;
    fn read_checkpoint(&self) -> Option<(u64, Vec<u8>)>;
}

// In-RAM HashMap; for tests and ranges small enough to fit.
#[derive(Default)]
pub struct MemStore {
    accounts: HashMap<Pubkey, (AccountSharedData, u64)>,
}

impl AccountStore for MemStore {
    fn get(&self, key: &Pubkey) -> Option<(AccountSharedData, u64)> {
        self.accounts.get(key).cloned()
    }

    fn put(&mut self, key: Pubkey, account: AccountSharedData, slot: u64) {
        self.accounts.insert(key, (account, slot));
    }

    fn contains(&self, key: &Pubkey) -> bool {
        self.accounts.contains_key(key)
    }

    fn flush(&mut self) {}

    fn checkpoint_flush(&mut self, _slot: u64, _roll: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }

    fn read_checkpoint(&self) -> Option<(u64, Vec<u8>)> {
        None
    }
}

const ACCOUNTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("accounts");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

// Flush the write-buffer once it holds this many bytes, so it never grows unbounded.
const FLUSH_THRESHOLD_BYTES: usize = 256 * 1024 * 1024;

// redb-backed scratch store (rebuilt each run) so Durability::None; writes buffered + flushed in batches because per-put commits bloat the file to 152 GiB.
pub struct DiskStore {
    db: Database,
    buffer: HashMap<Pubkey, (AccountSharedData, u64)>,
    buffered_bytes: usize,
    // Set after seeding: put() stops auto-flushing and flush() no-ops, so checkpoint_flush is the only committer and the redb never holds writes past the last checkpoint (what makes --resume sound).
    checkpoint_mode: bool,
}

impl DiskStore {
    // cache_bytes is redb's page-cache = the store's RAM budget (universe lives on disk).
    pub fn create(path: impl AsRef<Path>, cache_bytes: usize) -> anyhow::Result<Self> {
        let db = Database::builder()
            .set_cache_size(cache_bytes)
            .create(path)?;
        Ok(Self {
            db,
            buffer: HashMap::new(),
            buffered_bytes: 0,
            checkpoint_mode: false,
        })
    }

    fn read_from_disk(&self, key: &Pubkey) -> Option<(AccountSharedData, u64)> {
        let txn = self.db.begin_read().ok()?;
        // A fresh db has no table yet; treat "no table" as "no account".
        let table = txn.open_table(ACCOUNTS).ok()?;
        let value = table.get(key.as_ref()).ok()??;
        decode(value.value())
    }

    pub fn set_checkpoint_mode(&mut self, on: bool) {
        self.checkpoint_mode = on;
    }
}

impl AccountStore for DiskStore {
    fn get(&self, key: &Pubkey) -> Option<(AccountSharedData, u64)> {
        match self.buffer.get(key) {
            Some(value) => Some(value.clone()),
            None => self.read_from_disk(key),
        }
    }

    fn put(&mut self, key: Pubkey, account: AccountSharedData, slot: u64) {
        let added = 57 + account.data().len();
        if let Some((old, _)) = self.buffer.insert(key, (account, slot)) {
            self.buffered_bytes = self.buffered_bytes.saturating_sub(57 + old.data().len());
        }
        self.buffered_bytes += added;
        // No mid-slot auto-flush in checkpoint mode; it would put the redb ahead of the last checkpoint.
        if !self.checkpoint_mode && self.buffered_bytes >= FLUSH_THRESHOLD_BYTES {
            self.flush();
        }
    }

    fn contains(&self, key: &Pubkey) -> bool {
        self.buffer.contains_key(key) || self.read_from_disk(key).is_some()
    }

    fn flush(&mut self) {
        // No-op in checkpoint mode (replay_range calls this per chunk); checkpoint_flush is the only committer.
        if self.checkpoint_mode || self.buffer.is_empty() {
            return;
        }
        let mut txn = self.db.begin_write().expect("begin write txn");
        txn.set_durability(Durability::None);
        {
            let mut table = txn.open_table(ACCOUNTS).expect("open accounts table");
            for (key, (account, slot)) in self.buffer.drain() {
                let bytes = encode(&account, slot);
                table
                    .insert(key.as_ref(), bytes.as_slice())
                    .expect("insert account");
            }
        }
        txn.commit().expect("commit account writes");
        self.buffered_bytes = 0;
    }

    fn checkpoint_flush(&mut self, slot: u64, roll: &[u8]) -> anyhow::Result<()> {
        let mut txn = self.db.begin_write()?;
        // Immediate: accounts + checkpoint land durably together, so a crash can't split them.
        txn.set_durability(Durability::Immediate);
        {
            let mut accounts = txn.open_table(ACCOUNTS)?;
            for (key, (account, acct_slot)) in self.buffer.drain() {
                accounts.insert(key.as_ref(), encode(&account, acct_slot).as_slice())?;
            }
            let mut meta = txn.open_table(META)?;
            // slot(8 LE) ++ roll; written even on an empty buffer, so a no-write chunk still advances the slot.
            let mut value = slot.to_le_bytes().to_vec();
            value.extend_from_slice(roll);
            meta.insert("checkpoint", value.as_slice())?;
        }
        txn.commit()?;
        self.buffered_bytes = 0;
        Ok(())
    }

    fn read_checkpoint(&self) -> Option<(u64, Vec<u8>)> {
        let txn = self.db.begin_read().ok()?;
        let table = txn.open_table(META).ok()?; // no META table yet = never checkpointed
        let value = table.get("checkpoint").ok()??;
        let bytes = value.value();
        if bytes.len() < 8 {
            return None;
        }
        let slot = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        Some((slot, bytes[8..].to_vec()))
    }
}

// slot(8) | lamports(8) | rent_epoch(8) | executable(1) | owner(32) | data(rest): 57-byte head + data.
fn encode(account: &AccountSharedData, slot: u64) -> Vec<u8> {
    let data = account.data();
    let mut buf = Vec::with_capacity(57 + data.len());
    buf.extend_from_slice(&slot.to_le_bytes());
    buf.extend_from_slice(&account.lamports().to_le_bytes());
    buf.extend_from_slice(&account.rent_epoch().to_le_bytes());
    buf.push(account.executable() as u8);
    buf.extend_from_slice(account.owner().as_ref());
    buf.extend_from_slice(data);
    buf
}

fn decode(bytes: &[u8]) -> Option<(AccountSharedData, u64)> {
    if bytes.len() < 57 {
        return None;
    }
    let slot = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
    let lamports = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
    let rent_epoch = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
    let executable = bytes[24] != 0;
    let owner = Pubkey::try_from(&bytes[25..57]).ok()?;
    let data = bytes[57..].to_vec();
    let account = AccountSharedData::from(Account {
        lamports,
        data,
        owner,
        executable,
        rent_epoch,
    });
    Some((account, slot))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diskstore_round_trips_an_account() {
        let path = std::env::temp_dir().join("slate_diskstore_roundtrip.redb");
        let _ = std::fs::remove_file(&path);
        let mut store = DiskStore::create(&path, 16 * 1024 * 1024).unwrap();

        let key = Pubkey::new_from_array([3u8; 32]);
        let owner = Pubkey::new_from_array([9u8; 32]);
        let account = AccountSharedData::from(Account {
            lamports: 4_200,
            data: vec![1, 2, 3, 4, 5],
            owner,
            executable: false,
            rent_epoch: 7,
        });

        assert!(!store.contains(&key));
        assert!(store.get(&key).is_none());

        store.put(key, account, 99);
        assert!(store.contains(&key)); // visible from the buffer, before any flush

        store.flush(); // committed to redb; reads now come from disk
        assert!(store.contains(&key));
        let (got, slot) = store.get(&key).expect("present after flush");
        assert_eq!(slot, 99);
        assert_eq!(got.lamports(), 4_200);
        assert_eq!(got.data(), &[1, 2, 3, 4, 5]);
        assert_eq!(*got.owner(), owner);
        assert_eq!(got.rent_epoch(), 7);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn checkpoint_survives_reopen() {
        let path = std::env::temp_dir().join("slate_diskstore_checkpoint.redb");
        let _ = std::fs::remove_file(&path);
        let roll = vec![7u8; 40]; // opaque here; real roll serialization lives in bankhash

        let key = Pubkey::new_from_array([5u8; 32]);
        let account = AccountSharedData::from(Account {
            lamports: 9_000,
            data: vec![9, 9, 9],
            owner: Pubkey::new_from_array([1u8; 32]),
            executable: false,
            rent_epoch: 0,
        });

        {
            let mut store = DiskStore::create(&path, 16 * 1024 * 1024).unwrap();
            store.set_checkpoint_mode(true);
            store.put(key, account, 4242);
            store.checkpoint_flush(4242, &roll).unwrap();
        } // drop closes the db, standing in for a process exit

        let store = DiskStore::create(&path, 16 * 1024 * 1024).unwrap();
        let (slot, got_roll) = store.read_checkpoint().expect("checkpoint survived reopen");
        assert_eq!(slot, 4242);
        assert_eq!(got_roll, roll);
        let (acct, s) = store.get(&key).expect("account present after reopen");
        assert_eq!(s, 4242);
        assert_eq!(acct.lamports(), 9_000);

        let _ = std::fs::remove_file(&path);
    }
}
