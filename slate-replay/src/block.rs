//! Block source: turn a getBlock response into the transactions + per-tx meta
//! the replay loop needs. Parsing lives here (pure, unit-tested against an
//! embedded fixture); the thin reqwest wrapper that fetches a slot lives with
//! the loop, since network I/O is the orchestrator's job, not the engine's.

use agave_reserved_account_keys::ReservedAccountKeys;
use anyhow::{Context, Result};
use base64::Engine;
use solana_hash::Hash;
use solana_message::{
    AddressLoader,
    v0::{LoadedAddresses as V0LoadedAddresses, MessageAddressTableLookup},
};
use solana_pubkey::Pubkey;
use solana_transaction::{
    sanitized::{MessageHash, SanitizedTransaction},
    versioned::VersionedTransaction,
};
use solana_transaction_error::AddressLoaderError;

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

/// An [`AddressLoader`] that hands back the lookup-table addresses getBlock
/// already resolved for a v0 transaction, instead of re-deriving them from the
/// on-chain address-table accounts. Those addresses are part of the committed
/// block, so we trust them at the same level as the transaction itself — which
/// also means the lookup-table accounts don't need to be seeded, or their exact
/// state as of this slot reconstructed. A block source that returns an
/// inconsistent set is caught downstream: the resolved account count won't match
/// the meta's balance arrays and the oracle halts the replay.
#[derive(Clone)]
struct ResolvedAddresses {
    writable: Vec<Pubkey>,
    readonly: Vec<Pubkey>,
}

impl AddressLoader for ResolvedAddresses {
    fn load_addresses(
        self,
        _lookups: &[MessageAddressTableLookup],
    ) -> std::result::Result<V0LoadedAddresses, AddressLoaderError> {
        Ok(V0LoadedAddresses {
            writable: self.writable,
            readonly: self.readonly,
        })
    }
}

/// Turn a block transaction into the [`SanitizedTransaction`] the replayer takes.
/// Handles both legacy and v0 (address-lookup-table) messages. For a v0 tx the
/// lookup tables are resolved from `loaded` — the addresses getBlock already
/// resolved for this transaction (see [`ResolvedAddresses`]) — so no lookup-table
/// account state is needed. The message hash is computed and simple-vote status is
/// detected from the message. The reserved-key set is the fully-activated one,
/// correct for our post-epoch-808 floor where every reserved key is already live.
pub fn sanitize(
    tx: &VersionedTransaction,
    loaded: &LoadedAddresses,
) -> Result<SanitizedTransaction> {
    let reserved = ReservedAccountKeys::new_all_activated();
    let loader = ResolvedAddresses {
        writable: loaded.writable.clone(),
        readonly: loaded.readonly.clone(),
    };
    SanitizedTransaction::try_create(
        tx.clone(),
        MessageHash::Compute,
        None, // detect simple-vote from the message
        loader,
        &reserved.active,
    )
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

        let sanitized = sanitize(&t0.transaction, &t0.meta.loaded_addresses)
            .expect("legacy tx should sanitize");
        // account keys line up with the balance arrays the meta reports.
        assert_eq!(
            sanitized.message().account_keys().len(),
            t0.meta.pre_balances.len(),
        );
    }

    #[test]
    fn sanitizes_a_v0_tx_with_lookup_addresses() {
        use solana_message::{
            MessageHeader, VersionedMessage, compiled_instruction::CompiledInstruction,
            v0::Message as V0Message,
        };

        let payer = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let alt = Pubkey::new_unique();
        let w_loaded = Pubkey::new_unique();
        let r_loaded = Pubkey::new_unique();

        // A v0 message: two static keys (writable-signer payer, readonly program),
        // one lookup pulling in one writable + one readonly account, and an
        // instruction that touches both loaded accounts.
        let message = VersionedMessage::V0(V0Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys: vec![payer, program],
            recent_blockhash: Hash::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,  // `program` — a program must be a static key
                accounts: vec![2, 3], // the two loaded accounts
                data: vec![],
            }],
            address_table_lookups: vec![MessageAddressTableLookup {
                account_key: alt,
                writable_indexes: vec![0],
                readonly_indexes: vec![1],
            }],
        });
        let tx = VersionedTransaction {
            signatures: vec![solana_signature::Signature::default()],
            message,
        };
        let loaded = LoadedAddresses {
            writable: vec![w_loaded],
            readonly: vec![r_loaded],
        };

        let sanitized = sanitize(&tx, &loaded).expect("v0 tx should sanitize");

        // Resolved order is static keys, then loaded writable, then loaded readonly.
        let keys: Vec<Pubkey> = sanitized.message().account_keys().iter().copied().collect();
        assert_eq!(keys, vec![payer, program, w_loaded, r_loaded]);
        // Writability survives resolution: payer and the loaded-writable account are
        // writable; the readonly program and loaded-readonly account are not.
        assert!(sanitized.message().is_writable(0), "payer writable");
        assert!(!sanitized.message().is_writable(1), "program readonly");
        assert!(sanitized.message().is_writable(2), "loaded writable");
        assert!(!sanitized.message().is_writable(3), "loaded readonly");
    }

    #[test]
    fn sanitizes_a_real_mainnet_v0_tx() {
        // Real mainnet v0 tx `3mFFw1gy…A9tHw` at slot 438,686,267: a payer,
        // ComputeBudget, and the Archer program (all static), plus one writable and
        // one readonly account pulled from an address lookup table. The tx bytes are
        // straight from getTransaction; `loaded` is its `meta.loadedAddresses`; the
        // expected resolved order and writability are the chain's own (getTransaction
        // jsonParsed). This proves our ALT resolution reproduces exactly what the
        // runtime resolved, on real data. No execution: a real v0 tx touches volatile
        // program state we can't seed without a snapshot, so this exercises the new
        // resolution path, which is what v0 support actually added.
        const TX_B64: &str = "AYownfptVgnmlmBec4O8Ea7P0GwxZrSNzSMUoNkMq36JMSUX0vVDR753DYLNn0nZVQvgaU86SmimmV8/Hy0ipQmAAQACA+PR7JOJ9x+VNoggrlsrmG6pSPHWYFr79eziSqc10kxqAwZGb+UhFzL/7K26csOb57yM5bvF9xJrLEObOkAAAACSbwJV/6sw7SgPMIg8jcryjPkcMvll106orrFSiIQJ2Mmubt//DUKNJZxeh89/JU7lfBVVZURpqHZcGq6kK9npBAEABQJMHQAAAQAJAwAAAAAAAAAAAQAFBIAaBgACAwADBJgEB/2sHQoAAAAAWhUAAAAAAAAKCgAAAAAAncMCAAAAAAAAAAAAAAAAAGwlBAAAAAAA+/////////9sJQQAAAAAAPf/////////bCUEAAAAAADz/////////2wlBAAAAAAA7v////////+dwwIAAAAAAOr/////////bCUEAAAAAADl/////////2wlBAAAAAAA4f////////9sJQQAAAAAAN3/////////ncMCAAAAAADY/////////wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAL4CAAAAAAAAAQAAAAAAAAAdBAAAAAAAAAYAAAAAAAAAHQQAAAAAAAAKAAAAAAAAAB0EAAAAAAAADgAAAAAAAAC+AgAAAAAAABMAAAAAAAAAvgIAAAAAAAAXAAAAAAAAAB0EAAAAAAAAGwAAAAAAAAAdBAAAAAAAACAAAAAAAAAAHQQAAAAAAAAkAAAAAAAAAL4CAAAAAAAAKQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABb8+IV+seoVHrDD63JNlJEUTeQ0yR7qrG1EQue3HUY30BIAEp";

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(TX_B64)
            .expect("valid base64");
        let tx: VersionedTransaction = bincode::deserialize(&bytes).expect("v0 transaction");

        let loaded = LoadedAddresses {
            writable: vec![
                "7vS5eTNrSyQDE65x1vcRZC31jzBUwa8DUZgJs7pBKABC"
                    .parse()
                    .unwrap(),
            ],
            readonly: vec![
                "A7Zx9dfLuZmdXJjdfakwLbnPmeHE8L28Nf5D2buCCFNq"
                    .parse()
                    .unwrap(),
            ],
        };

        let sanitized = sanitize(&tx, &loaded).expect("real v0 tx should sanitize");

        // The chain's resolved order: 3 static keys, then loaded writable, then readonly.
        let expected: Vec<Pubkey> = [
            "GLKCuotKSjPtQhxHmf6q8GUkw6tUZnzs8W5vEbxdaBdK",
            "ComputeBudget111111111111111111111111111111",
            "Archer8kgiavM61GyusMzaaS2ft5sALtNsD1HxkUPMhy",
            "7vS5eTNrSyQDE65x1vcRZC31jzBUwa8DUZgJs7pBKABC",
            "A7Zx9dfLuZmdXJjdfakwLbnPmeHE8L28Nf5D2buCCFNq",
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
        let keys: Vec<Pubkey> = sanitized.message().account_keys().iter().copied().collect();
        assert_eq!(
            keys, expected,
            "resolved account order must match the chain"
        );

        // Writability, straight from the chain: payer and the loaded-writable account
        // are writable; the two programs and the loaded-readonly account are not.
        for (i, want) in [true, false, false, true, false].iter().enumerate() {
            assert_eq!(sanitized.message().is_writable(i), *want, "writable[{i}]");
        }
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
