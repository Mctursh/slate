use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use redb::{Database, Durability, TableDefinition};
use reqwest::blocking::Client;

use crate::block::{Block, fetch_block_opt, fetch_confirmed_slots};

// Big retry budget: one unrecovered miss aborts a whole pass, and Old Faithful flakes transiently (CDN range-fetch), so it has to outlast a transient window, not just a blip.
const MAX_RETRIES: usize = 80;

// Send + Sync so a shared source can be handed to a blocking fetch task while the async loop persists the previous chunk.
pub trait BlockSource: Send + Sync {
    // Confirmed slots in (from, to], the ones that actually produced a block.
    fn confirmed_slots(&self, from: u64, to: u64) -> Result<Vec<u64>>;
    // Blocking is fine, the caller drives chunks, so only one chunk is ever resident.
    fn fetch(&self, slots: &[u64]) -> Result<Vec<Block>>;
}

// getBlock over JSON-RPC; backs both a local yellowstone-faithful (production) and a remote provider (Helius/QuickNode), only the URL differs.
pub struct RpcBlockSource {
    rpc_url: String,
    client: Client,
    concurrency: usize,
}

const BLOCKS: TableDefinition<u64, &[u8]> = TableDefinition::new("blocks");

pub struct CachingBlockSource {
    inner: Arc<dyn BlockSource>,
    db: Database,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl CachingBlockSource {
    pub fn new(inner: Arc<dyn BlockSource>, cache_path: PathBuf) -> Result<Self> {
        let db = Database::create(&cache_path)?;
        let txn = db.begin_write()?;
        txn.open_table(BLOCKS)?;
        txn.commit()?;
        Ok(Self {
            inner,
            db,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })
    }
}

impl RpcBlockSource {
    pub fn new(rpc_url: impl Into<String>) -> Self {
        let client = Client::builder()
            .pool_max_idle_per_host(64)
            .timeout(Duration::from_secs(120))
            .build()
            .expect("build blocking http client");
        Self {
            rpc_url: rpc_url.into(),
            client,
            concurrency: 1,
        }
    }

    // Leave at 1 (serial) for a rate-limited provider like Helius; raise it for an unmetered local source where parallel fetches make a big window tractable.
    pub fn with_concurrency(mut self, n: usize) -> Self {
        self.concurrency = n.max(1);
        self
    }

    // Retries transient failures with backoff; None for a skipped slot so the caller drops it.
    fn fetch_one(&self, slot: u64) -> Result<Option<Block>> {
        let mut attempt = 0usize;
        loop {
            match fetch_block_opt(&self.client, &self.rpc_url, slot) {
                Ok(block) => return Ok(block),
                Err(e) => {
                    attempt += 1;
                    if attempt > MAX_RETRIES {
                        return Err(e.context(format!(
                            "fetching block {slot} failed after {MAX_RETRIES} retries"
                        )));
                    }
                    // Exponential backoff capped at 20s: 0.5, 1, 2, 4, 8, 16, then 20s.
                    // Old Faithful's transient range-fetch failures clear in seconds-to-minutes; spaced-out retries ride them out instead of hammering a saturated CDN.
                    let backoff_ms = (500u64 << (attempt as u32 - 1).min(6)).min(20_000);
                    std::thread::sleep(Duration::from_millis(backoff_ms));
                }
            }
        }
    }
}

impl BlockSource for RpcBlockSource {
    fn confirmed_slots(&self, from: u64, to: u64) -> Result<Vec<u64>> {
        // `(from, to]`: exclude the snapshot slot itself, which the seed already covers.
        fetch_confirmed_slots(&self.rpc_url, from + 1, to)
    }

    fn fetch(&self, slots: &[u64]) -> Result<Vec<Block>> {
        if slots.is_empty() {
            return Ok(Vec::new());
        }
        if self.concurrency == 1 {
            let mut out = Vec::with_capacity(slots.len());
            for &slot in slots {
                if let Some(block) = self.fetch_one(slot)? {
                    out.push(block);
                }
            }
            return Ok(out);
        }
        // Bounded-concurrency, order-preserving: workers pull indices off a shared counter into per-index cells; skipped slots (None) drop out when collecting, leaving confirmed blocks in slot order.
        let results: Vec<Mutex<Option<Result<Option<Block>>>>> =
            (0..slots.len()).map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let workers = self.concurrency.min(slots.len());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= slots.len() {
                            break;
                        }
                        let fetched = self.fetch_one(slots[i]);
                        *results[i].lock().expect("results mutex") = Some(fetched);
                    }
                });
            }
        });
        let mut out = Vec::with_capacity(slots.len());
        for m in results {
            match m
                .into_inner()
                .expect("results mutex")
                .expect("worker set result")
            {
                Ok(Some(block)) => out.push(block),
                Ok(None) => {} // skipped slot
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }
}

impl BlockSource for CachingBlockSource {
    fn confirmed_slots(&self, from: u64, to: u64) -> Result<Vec<u64>> {
        self.inner.confirmed_slots(from, to)
    }

    fn fetch(&self, slots: &[u64]) -> Result<Vec<Block>> {
        let mut hits: HashMap<u64, Block> = HashMap::new();
        let mut misses: Vec<u64> = Vec::new();

        {
            let txn = self.db.begin_read()?;
            let table = txn.open_table(BLOCKS)?;
            for &slot in slots {
                match table.get(slot)? {
                    Some(g) => {
                        hits.insert(slot, bincode::deserialize(g.value())?);
                    }
                    None => misses.push(slot),
                }
            }
        }

        self.hits.fetch_add(hits.len() as u64, Ordering::Relaxed);
        self.misses
            .fetch_add(misses.len() as u64, Ordering::Relaxed);

        let fresh = self.inner.fetch(&misses)?;
        if !fresh.is_empty() {
            let mut txn = self.db.begin_write()?;
            // Immediate, not None: redb rolls back to the last DURABLE commit on reopen, so a
            // cache that only ever commits with None is empty in every later process and its
            // pages are never freed (the file grew to 98 GB holding nothing). That cost two
            // full 50k-block refetches before it was spotted.
            txn.set_durability(Durability::Immediate);
            {
                let mut table = txn.open_table(BLOCKS)?;
                for b in &fresh {
                    table.insert(b.slot, bincode::serialize(&b)?.as_slice())?;
                }
            }
            txn.commit()?;
        }

        for b in fresh {
            hits.insert(b.slot, b);
        }

        let (h, m) = (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        );
        if (h + m).is_multiple_of(10_000) && h + m > 0 {
            eprintln!(
                "block cache: {h} hits, {m} misses ({:.0}% hit)",
                100.0 * h as f64 / (h + m) as f64
            );
        }
        let out: Vec<Block> = slots.iter().filter_map(|s| hits.remove(s)).collect();
        // Covers cached blocks too, so a bad block already on disk is caught on the way out.
        for pair in out.windows(2) {
            crate::block::verify_chains_to(&pair[1], &pair[0])?;
        }
        Ok(out)
    }
}

// In-memory BlockSource for tests and small pre-built ranges; the replay path treats it like a remote source.
pub struct VecBlockSource {
    blocks: Vec<Block>,
}

impl VecBlockSource {
    pub fn new(blocks: Vec<Block>) -> Self {
        Self { blocks }
    }
}

impl BlockSource for VecBlockSource {
    fn confirmed_slots(&self, from: u64, to: u64) -> Result<Vec<u64>> {
        let mut slots: Vec<u64> = self
            .blocks
            .iter()
            .map(|b| b.slot)
            .filter(|&s| s > from && s <= to)
            .collect();
        slots.sort_unstable();
        Ok(slots)
    }

    fn fetch(&self, slots: &[u64]) -> Result<Vec<Block>> {
        // Return in `slots` order (the chunk order the caller expects), not the source's internal order.
        Ok(slots
            .iter()
            .filter_map(|s| self.blocks.iter().find(|b| b.slot == *s).cloned())
            .collect())
    }
}

#[cfg(test)]
mod tests {

    // redb rolls back to the last DURABLE commit on reopen. The cache committed only with
    // Durability::None and never higher, so every process after the first saw an empty table
    // and refetched the whole range, while the file grew unboundedly holding nothing.
    #[test]
    fn the_cache_survives_a_reopen() {
        let path = std::env::temp_dir().join("slate_cache_durability.redb");
        let _ = std::fs::remove_file(&path);

        struct Never;
        impl BlockSource for Never {
            fn confirmed_slots(&self, _: u64, _: u64) -> Result<Vec<u64>> {
                Ok(vec![])
            }
            fn fetch(&self, slots: &[u64]) -> Result<Vec<Block>> {
                assert!(slots.is_empty(), "a warm cache must not refetch {slots:?}");
                Ok(vec![])
            }
        }

        let block = Block {
            slot: 42,
            parent_slot: 41,
            blockhash: Default::default(),
            block_height: 7,
            previous_blockhash: Default::default(),
            block_time: 1_700_000_000,
            transactions: vec![],
            fee_reward: None,
        };

        {
            struct One(Block);
            impl BlockSource for One {
                fn confirmed_slots(&self, _: u64, _: u64) -> Result<Vec<u64>> {
                    Ok(vec![42])
                }
                fn fetch(&self, _: &[u64]) -> Result<Vec<Block>> {
                    Ok(vec![self.0.clone()])
                }
            }
            let c = CachingBlockSource::new(Arc::new(One(block.clone())), path.clone()).unwrap();
            assert_eq!(c.fetch(&[42]).unwrap().len(), 1);
        } // dropped: releases redb's lock so the reopen below can happen

        let warm = CachingBlockSource::new(Arc::new(Never), path.clone()).unwrap();
        assert_eq!(
            warm.fetch(&[42]).unwrap().len(),
            1,
            "the block must come back from the cache after a reopen"
        );

        let _ = std::fs::remove_file(&path);
    }
    use super::*;
}
