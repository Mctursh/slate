//! Block source: the adapter that feeds the replay its blocks. One trait so the backend
//! swaps without touching the replay loop — yellowstone-faithful (Old Faithful) or plain
//! JSON-RPC today (both getBlock), Jetstreamer or direct CAR reads later. The replay
//! pulls one chunk of slots at a time, so blocks never all sit in RAM.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::Duration,
};

use anyhow::Result;
use reqwest::blocking::Client;

use crate::block::{fetch_block_opt, fetch_confirmed_slots, Block};

/// How many times to retry a transient fetch failure (429/timeout/dropped connection, or
/// Old Faithful failing to range-fetch a block from the remote CAR) before giving up.
/// A footprint or replay pass over a 50k-slot range fetches tens of thousands of blocks;
/// a single unrecovered miss aborts the whole pass, so the budget has to outlast a
/// transient CDN window (seconds to a couple of minutes), not just a blip. With the
/// backoff below this is ~12 min of retrying per block — long, but a pass is worth hours.
const MAX_RETRIES: usize = 40;

/// Where the replay gets its blocks. `Send + Sync` so a shared source can be handed to a
/// blocking fetch task while the async loop persists the previous chunk.
pub trait BlockSource: Send + Sync {
    /// The confirmed slots in `(from, to]` — the ones that actually have a block.
    fn confirmed_slots(&self, from: u64, to: u64) -> Result<Vec<u64>>;
    /// Fetch the blocks for `slots` (one chunk). Blocking is fine — the caller drives
    /// chunks, so only a chunk's worth is ever resident.
    fn fetch(&self, slots: &[u64]) -> Result<Vec<Block>>;
}

/// `getBlock` over JSON-RPC. Backs both a local `yellowstone-faithful` (Old Faithful,
/// unmetered, the production path) and a remote provider (Helius/QuickNode, handy for
/// recent slots or quick tests) — same protocol, only the URL differs.
pub struct RpcBlockSource {
    rpc_url: String,
    client: Client,
    concurrency: usize,
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

    /// How many blocks to fetch at once. Leave at 1 (serial) for a rate-limited provider
    /// like Helius; raise it for an unmetered local source (yellowstone-faithful), where
    /// parallel range-fetches are what make a 50k-slot window tractable.
    pub fn with_concurrency(mut self, n: usize) -> Self {
        self.concurrency = n.max(1);
        self
    }

    /// Fetch one block, retrying transient failures with a short linear backoff. Returns
    /// `None` for a skipped slot (no block) so the caller can drop it. The pooled client
    /// is shared, so a retry reuses the connection.
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
                    // Exponential backoff capped at 20s: 0.5s, 1, 2, 4, 8, 16, then 20s.
                    // Old Faithful's transient range-fetch failures clear on their own in
                    // seconds to a couple of minutes; longer, spaced-out retries ride them
                    // out instead of hammering a saturated CDN connection.
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
        // Bounded-concurrency parallel fetch, order-preserving: workers pull slot indices
        // off a shared counter and write each result into its own cell. Skipped slots
        // (None) are dropped when collecting, so the returned Vec is the confirmed blocks
        // in slot order.
        let results: Vec<Mutex<Option<Result<Option<Block>>>>> =
            (0..slots.len()).map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let workers = self.concurrency.min(slots.len());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= slots.len() {
                        break;
                    }
                    let fetched = self.fetch_one(slots[i]);
                    *results[i].lock().expect("results mutex") = Some(fetched);
                });
            }
        });
        let mut out = Vec::with_capacity(slots.len());
        for m in results {
            match m.into_inner().expect("results mutex").expect("worker set result") {
                Ok(Some(block)) => out.push(block),
                Ok(None) => {} // skipped slot
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }
}

/// A [`BlockSource`] over blocks already in memory. For tests, and for callers that have
/// a small range pre-built — the replay path treats it exactly like a remote source.
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
        // Return them in `slots` order (the chunk order the caller expects), not the
        // source's internal order.
        Ok(slots
            .iter()
            .filter_map(|s| self.blocks.iter().find(|b| b.slot == *s).cloned())
            .collect())
    }
}
