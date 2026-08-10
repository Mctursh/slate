//! Block source: turn a getBlock response into the transactions + per-tx meta
//! the replay loop needs. Parsing lives here (pure, unit-tested against an
//! embedded fixture); the thin reqwest wrapper that fetches a slot lives with
//! the loop, since network I/O is the orchestrator's job, not the engine's.

use agave_reserved_account_keys::ReservedAccountKeys;
use anyhow::{Context, Result};
use base64::Engine;
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use solana_transaction::{sanitized::SanitizedTransaction, versioned::VersionedTransaction};

/// One block's worth of replay input: the transactions in execution order plus
/// the block-level context the environment needs (blockhash, block time).
pub struct Block {
    pub slot: u64,
    pub parent_slot: u64,
    pub blockhash: Hash,
    pub block_time: i64,
    pub transactions: Vec<BlockTx>,
}

/// A transaction and the on-chain result we reconcile our replay against.
pub struct BlockTx {
    pub transaction: VersionedTransaction,
    pub meta: TxMeta,
}

/// The getBlock meta fields the oracle checks a replay against. Token balances,
/// inner instructions, and logs are omitted until something consumes them.
pub struct TxMeta {
    /// The on-chain error, rendered; `None` means the transaction succeeded.
    pub err: Option<String>,
    pub fee: u64,
    pub compute_units_consumed: u64,
    pub pre_balances: Vec<u64>,
    pub post_balances: Vec<u64>,
    /// Addresses pulled in from lookup tables (empty for legacy transactions).
    pub loaded_addresses: LoadedAddresses,
}

impl TxMeta {
    /// Whether the transaction succeeded on chain.
    pub fn succeeded(&self) -> bool {
        self.err.is_none()
    }
}

#[derive(Default)]
pub struct LoadedAddresses {
    pub writable: Vec<Pubkey>,
    pub readonly: Vec<Pubkey>,
}

impl Block {
    /// Parse the `result` object of a getBlock response (encoding `base64`,
    /// `transactionDetails: full`, `maxSupportedTransactionVersion: 0`).
    pub fn from_getblock(slot: u64, result: &serde_json::Value) -> Result<Block> {
        let parent_slot = result["parentSlot"]
            .as_u64()
            .context("getBlock missing parentSlot")?;
        let blockhash = result["blockhash"]
            .as_str()
            .context("getBlock missing blockhash")?
            .parse::<Hash>()
            .context("getBlock blockhash is not a valid hash")?;
        let block_time = result["blockTime"]
            .as_i64()
            .context("getBlock missing blockTime")?;

        let raw = result["transactions"]
            .as_array()
            .context("getBlock missing transactions[]")?;
        let mut transactions = Vec::with_capacity(raw.len());
        for (i, t) in raw.iter().enumerate() {
            transactions.push(parse_tx(t).with_context(|| format!("transaction[{i}]"))?);
        }

        Ok(Block {
            slot,
            parent_slot,
            blockhash,
            block_time,
            transactions,
        })
    }
}

fn parse_tx(t: &serde_json::Value) -> Result<BlockTx> {
    // `transaction` is [base64, "base64"]; the bytes are a bincode VersionedTransaction.
    let b64 = t["transaction"][0]
        .as_str()
        .context("transaction is not base64-encoded")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("transaction base64 did not decode")?;
    let transaction: VersionedTransaction =
        bincode::deserialize(&bytes).context("transaction did not deserialize")?;

    let m = &t["meta"];
    let meta = TxMeta {
        err: if m["err"].is_null() {
            None
        } else {
            Some(m["err"].to_string())
        },
        fee: m["fee"].as_u64().context("meta missing fee")?,
        compute_units_consumed: m["computeUnitsConsumed"].as_u64().unwrap_or(0),
        pre_balances: parse_u64_array(&m["preBalances"]).context("meta preBalances")?,
        post_balances: parse_u64_array(&m["postBalances"]).context("meta postBalances")?,
        loaded_addresses: parse_loaded_addresses(&m["loadedAddresses"])?,
    };

    Ok(BlockTx { transaction, meta })
}

fn parse_u64_array(v: &serde_json::Value) -> Result<Vec<u64>> {
    v.as_array()
        .context("expected an array")?
        .iter()
        .map(|x| x.as_u64().context("expected a u64"))
        .collect()
}

fn parse_loaded_addresses(v: &serde_json::Value) -> Result<LoadedAddresses> {
    // Absent on old snapshots; treat as empty rather than erroring.
    if v.is_null() {
        return Ok(LoadedAddresses::default());
    }
    let pubkeys = |key: &str| -> Result<Vec<Pubkey>> {
        v[key]
            .as_array()
            .with_context(|| format!("loadedAddresses.{key} not an array"))?
            .iter()
            .map(|x| {
                x.as_str()
                    .context("loaded address not a string")?
                    .parse::<Pubkey>()
                    .context("loaded address not a pubkey")
            })
            .collect()
    };
    Ok(LoadedAddresses {
        writable: pubkeys("writable")?,
        readonly: pubkeys("readonly")?,
    })
}

/// Fetch one block over getBlock and parse it. Blocking I/O on purpose: backfill
/// is a batch job, so a straight fetch -> replay loop is simplest and needs no
/// async runtime. `rpc_url` must point at an archive RPC that still has `slot`.
pub fn fetch_block(rpc_url: &str, slot: u64) -> Result<Block> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getBlock",
        "params": [slot, {
            "encoding": "base64",
            "transactionDetails": "full",
            "rewards": false,
            "maxSupportedTransactionVersion": 0,
        }],
    });
    let resp: serde_json::Value = reqwest::blocking::Client::new()
        .post(rpc_url)
        .json(&request)
        .send()?
        .json()?;
    let result = resp
        .get("result")
        .with_context(|| format!("getBlock returned no result: {resp}"))?;
    Block::from_getblock(slot, result)
}

/// Turn a block transaction into the [`SanitizedTransaction`] the replayer takes.
/// Legacy only for now: v0 (address-lookup-table) transactions need their tables
/// resolved against the bank first, which isn't wired yet, so they're rejected.
/// The reserved-key set is the fully-activated one, correct for our post-epoch-808
/// floor where every reserved key is already live.
pub fn sanitize(tx: &VersionedTransaction) -> Result<SanitizedTransaction> {
    let legacy = tx
        .clone()
        .into_legacy_transaction()
        .context("v0 / address-lookup-table transactions are not supported yet")?;
    let reserved = ReservedAccountKeys::new_all_activated();
    SanitizedTransaction::try_from_legacy_transaction(legacy, &reserved.active)
        .context("transaction failed to sanitize")
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real mainnet getBlock (slot 437680849), trimmed to the first 3 txs.
    const FIXTURE: &str = include_str!("getblock_fixture.json");

    #[test]
    fn parses_getblock() {
        let json: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let block = Block::from_getblock(437_680_849, &json["result"]).unwrap();

        assert_eq!(block.slot, 437_680_849);
        assert_eq!(block.parent_slot, 437_680_848);
        assert_eq!(block.block_time, 1_769_392_720);
        assert_eq!(block.transactions.len(), 3);

        // tx[0]: values read straight from the fixture, so this pins the parse.
        let t0 = &block.transactions[0];
        assert!(t0.meta.succeeded(), "tx0 succeeded on chain");
        assert_eq!(t0.meta.fee, 5_000);
        assert_eq!(t0.meta.compute_units_consumed, 65_266);
        assert_eq!(t0.meta.pre_balances.len(), 8);
        assert_eq!(t0.meta.post_balances.len(), 8);
        // the transaction actually deserialized
        assert!(
            !t0.transaction.message.static_account_keys().is_empty(),
            "tx0 should have account keys"
        );
        // legacy tx: no lookup-table addresses
        assert!(t0.meta.loaded_addresses.writable.is_empty());
        assert!(t0.meta.loaded_addresses.readonly.is_empty());
    }

    #[test]
    fn sanitizes_a_legacy_tx() {
        let json: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let block = Block::from_getblock(437_680_849, &json["result"]).unwrap();
        let t0 = &block.transactions[0];

        let sanitized = sanitize(&t0.transaction).expect("legacy tx should sanitize");
        // account keys line up with the balance arrays the meta reports.
        assert_eq!(
            sanitized.message().account_keys().len(),
            t0.meta.pre_balances.len(),
        );
    }

    #[test]
    #[ignore = "hits a mainnet archive RPC; run with SLATE_RPC set"]
    fn fetch_block_live() {
        let url = std::env::var("SLATE_RPC").expect("set SLATE_RPC to an archive RPC url");
        let block = fetch_block(&url, 437_680_849).unwrap();
        assert_eq!(block.slot, 437_680_849);
        assert_eq!(block.parent_slot, 437_680_848);
        assert!(!block.transactions.is_empty());
    }
}
