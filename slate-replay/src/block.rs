//! Block source: turn a getBlock response into the transactions + per-tx meta
//! the replay loop needs. Parsing lives here (pure, unit-tested against an
//! embedded fixture); the thin reqwest wrapper that fetches a slot lives with
//! the loop, since network I/O is the orchestrator's job, not the engine's.

use std::collections::HashSet;

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
#[derive(Clone)]
pub struct Block {
    pub slot: u64,
    pub parent_slot: u64,
    pub blockhash: Hash,
    /// The parent block's blockhash (getBlock `previousBlockhash`) — the blockhash
    /// the bank is on while it processes this slot. This is what a durable nonce
    /// advances from, so it, not any individual tx's recent_blockhash, is the
    /// environment blockhash for the block's transactions.
    pub previous_blockhash: Hash,
    pub block_time: i64,
    pub transactions: Vec<BlockTx>,
    /// The slot's leader fee reward from getBlock `rewards` (the "Fee" entry): the
    /// leader's identity pubkey and the lamports credited to it at freeze. The runtime
    /// pays the leader 50% of the slot's fees (burning the rest), an account write that
    /// the bank-hash lattice must include. `None` if the block carried no fee reward.
    pub fee_reward: Option<(Pubkey, u64)>,
}

/// A transaction and the on-chain result we reconcile our replay against.
#[derive(Clone)]
pub struct BlockTx {
    pub transaction: VersionedTransaction,
    pub meta: TxMeta,
}

/// The getBlock meta fields the oracle checks a replay against. Inner instructions
/// and logs are omitted until something consumes them.
#[derive(Clone)]
pub struct TxMeta {
    /// The on-chain error, rendered; `None` means the transaction succeeded.
    pub err: Option<String>,
    pub fee: u64,
    pub compute_units_consumed: u64,
    pub pre_balances: Vec<u64>,
    pub post_balances: Vec<u64>,
    /// Addresses pulled in from lookup tables (empty for legacy transactions).
    pub loaded_addresses: LoadedAddresses,
    /// Post-transaction SPL token amounts, one per touched token account (empty
    /// when the tx touches no token accounts). The oracle checks these too.
    pub post_token_balances: Vec<TokenBalance>,
}

impl TxMeta {
    /// Whether the transaction succeeded on chain.
    pub fn succeeded(&self) -> bool {
        self.err.is_none()
    }
}

/// One token account's post amount from getBlock's `postTokenBalances`.
/// `account_index` indexes the transaction's account list (the same order as
/// `post_balances`); `amount` is the raw token amount (not UI-scaled).
#[derive(Clone)]
pub struct TokenBalance {
    pub account_index: u8,
    pub mint: Pubkey,
    pub amount: u64,
}

#[derive(Default, Clone)]
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
        let previous_blockhash = result["previousBlockhash"]
            .as_str()
            .context("getBlock missing previousBlockhash")?
            .parse::<Hash>()
            .context("getBlock previousBlockhash is not a valid hash")?;
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

        // The leader fee reward (rewardType "Fee"): the leader's pubkey and the lamports
        // credited to it at freeze. Mid-epoch this is the block's only reward.
        let fee_reward = result["rewards"]
            .as_array()
            .and_then(|rewards| rewards.iter().find(|r| r["rewardType"].as_str() == Some("Fee")))
            .and_then(|r| {
                let pubkey = r["pubkey"].as_str()?.parse::<Pubkey>().ok()?;
                let lamports = u64::try_from(r["lamports"].as_i64()?).ok()?;
                Some((pubkey, lamports))
            });

        Ok(Block {
            slot,
            parent_slot,
            blockhash,
            previous_blockhash,
            block_time,
            transactions,
            fee_reward,
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
        post_token_balances: parse_token_balances(&m["postTokenBalances"])?,
    };

    Ok(BlockTx { transaction, meta })
}

fn parse_token_balances(v: &serde_json::Value) -> Result<Vec<TokenBalance>> {
    // Absent when the tx touches no token accounts; treat as empty.
    if v.is_null() {
        return Ok(Vec::new());
    }
    v.as_array()
        .context("postTokenBalances not an array")?
        .iter()
        .map(|entry| {
            Ok(TokenBalance {
                account_index: entry["accountIndex"]
                    .as_u64()
                    .context("token balance missing accountIndex")?
                    as u8,
                mint: entry["mint"]
                    .as_str()
                    .context("token balance missing mint")?
                    .parse()
                    .context("token balance mint is not a pubkey")?,
                // uiTokenAmount.amount is the raw amount, sent as a string.
                amount: entry["uiTokenAmount"]["amount"]
                    .as_str()
                    .context("token balance missing uiTokenAmount.amount")?
                    .parse()
                    .context("token amount is not a u64")?,
            })
        })
        .collect()
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

/// Fetch one block over getBlock and parse it, reusing `client` so its connection pool
/// is shared across a chunk's fetches (no TCP+TLS handshake per block). Blocking I/O on
/// purpose: backfill is a batch job. `rpc_url` must point at an archive RPC (or a local
/// yellowstone-faithful) that still has `slot`.
pub fn fetch_block_with(
    client: &reqwest::blocking::Client,
    rpc_url: &str,
    slot: u64,
) -> Result<Block> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getBlock",
        "params": [slot, {
            "encoding": "base64",
            "transactionDetails": "full",
            "rewards": true,
            "maxSupportedTransactionVersion": 0,
        }],
    });
    let resp: serde_json::Value = client.post(rpc_url).json(&request).send()?.json()?;
    let result = resp
        .get("result")
        .with_context(|| format!("getBlock returned no result: {resp}"))?;
    Block::from_getblock(slot, result)
}

/// Fetch one block with a one-off client. For preflight/dry-run single calls; the backfill
/// path fetches through [`crate::source::RpcBlockSource`], which pools + parallelizes.
pub fn fetch_block(rpc_url: &str, slot: u64) -> Result<Block> {
    fetch_block_with(&reqwest::blocking::Client::new(), rpc_url, slot)
}

/// Ask the RPC which slots in `[start, end]` actually produced a block (getBlocks).
/// The ~5% of skipped slots never produced one, so getBlock would error on them;
/// getBlocks returns only the real ones. getBlocks caps the span at 500k slots, so
/// a larger backfill would have to page, which the caller doesn't do yet.
pub fn fetch_confirmed_slots(rpc_url: &str, start: u64, end: u64) -> Result<Vec<u64>> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getBlocks",
        "params": [start, end],
    });
    let resp: serde_json::Value = reqwest::blocking::Client::new()
        .post(rpc_url)
        .json(&request)
        .send()?
        .json()?;
    match resp.get("result") {
        // yellowstone-faithful (Old Faithful) serves getBlock but not getBlocks, and
        // returns a null result. Fall back to the whole candidate range; the fetch path
        // skips slots that produced no block (see `fetch_block_opt`).
        None | Some(serde_json::Value::Null) => Ok((start..=end).collect()),
        Some(result) => result
            .as_array()
            .with_context(|| format!("getBlocks result was not an array: {result}"))?
            .iter()
            .map(|v| v.as_u64().context("getBlocks returned a non-integer slot"))
            .collect(),
    }
}

/// Like [`fetch_block_with`] but returns `None` for a slot that produced no block (a
/// skipped slot: null result, or a "not available"/"skipped" RPC error) instead of
/// erroring — so a caller enumerating a full range (no getBlocks) can drop the ~5% of
/// empty slots. A genuine transport/parse failure still returns `Err`.
pub fn fetch_block_opt(
    client: &reqwest::blocking::Client,
    rpc_url: &str,
    slot: u64,
) -> Result<Option<Block>> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getBlock",
        "params": [slot, {
            "encoding": "base64",
            "transactionDetails": "full",
            "rewards": true,
            "maxSupportedTransactionVersion": 0,
        }],
    });
    let resp: serde_json::Value = client.post(rpc_url).json(&request).send()?.json()?;
    if let Some(err) = resp.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default();
        if msg.contains("skip") || msg.contains("not available") || msg.contains("Block not") {
            return Ok(None);
        }
        anyhow::bail!("getBlock error for slot {slot}: {err}");
    }
    match resp.get("result") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(result) => Ok(Some(Block::from_getblock(slot, result)?)),
    }
}

/// The chain's current slot (getSlot). Doubles as an RPC reachability probe and
/// lets a caller reject a target range that runs past the chain head.
pub fn current_slot(rpc_url: &str) -> Result<u64> {
    let request = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "getSlot" });
    let resp: serde_json::Value = reqwest::blocking::Client::new()
        .post(rpc_url)
        .json(&request)
        .send()?
        .json()?;
    resp.get("result")
        .and_then(|v| v.as_u64())
        .with_context(|| format!("getSlot returned no slot: {resp}"))
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

/// The set of accounts a range of blocks touches — the seed set to pull from the
/// snapshot. It's the union of every transaction's static account keys and the
/// addresses it loaded from lookup tables, plus every feature account so the
/// per-slot feature set can be computed.
///
/// Deliberately NOT owner-filtered: our program's transactions read accounts other
/// transactions write, so scoping the seed to one program yields stale reads. The
/// footprint bounds memory by skipping dormant untouched accounts, never by
/// narrowing the read-set.
///
/// Two things it does not add: sysvars (Clock/Rent/EpochSchedule are synthesized;
/// SlotHashes/StakeHistory content is a separate snapshot-seeded step), and an
/// upgradeable program's programdata account, which is resolved when the program
/// is loaded, not here.
/// Add the accounts `blocks` reference — every tx's static keys and lookup-loaded
/// addresses, plus each slot's fee-credited leader — to `set`. Called once per chunk
/// while streaming, so the footprint accumulates without ever holding every block.
pub fn extend_footprint(set: &mut HashSet<Pubkey>, blocks: &[Block]) {
    for block in blocks {
        for tx in &block.transactions {
            set.extend(tx.transaction.message.static_account_keys().iter().copied());
            set.extend(tx.meta.loaded_addresses.writable.iter().copied());
            set.extend(tx.meta.loaded_addresses.readonly.iter().copied());
        }
        // The leader credited this slot's fees — freeze writes it, so the lattice needs
        // its pre-slot value seeded.
        if let Some((leader, _)) = block.fee_reward {
            set.insert(leader);
        }
    }
}

/// Add the block-independent part of the seed to `set`: every feature account (so the
/// per-slot feature set can be built) and the sysvars replay reads but no block names.
/// Added once, after the block keys.
pub fn footprint_fixed(set: &mut HashSet<Pubkey>) {
    set.extend(agave_feature_set::FEATURE_NAMES.keys().copied());
    // Seed every sysvar we need from the snapshot rather than synthesize it. The
    // bank-hash roll needs each per-slot sysvar write to be bit-exact, so replay
    // starts from the real value at s_snap and applies the runtime's minimal delta:
    // Clock keeps its epoch fields and just advances slot/timestamp; SlotHistory and
    // RecentBlockhashes roll one entry forward; Rent/EpochSchedule don't change. The
    // historical ones (StakeHistory's stake curve, EpochRewards, LastRestartSlot)
    // likewise can't be synthesized. SlotHashes is added in backfill (it also seeds
    // the programData PDAs there).
    set.insert(solana_sdk_ids::sysvar::clock::id());
    set.insert(solana_sdk_ids::sysvar::slot_history::id());
    set.insert(solana_sdk_ids::sysvar::recent_blockhashes::id());
    set.insert(solana_sdk_ids::sysvar::rent::id());
    set.insert(solana_sdk_ids::sysvar::epoch_schedule::id());
    set.insert(solana_sdk_ids::sysvar::stake_history::id());
    set.insert(solana_sdk_ids::sysvar::epoch_rewards::id());
    set.insert(solana_sdk_ids::sysvar::last_restart_slot::id());
}

/// The full seed footprint for `blocks` in one shot: block keys ∪ the fixed set. Kept
/// for tests and non-streaming callers; the streaming backfill calls the two halves
/// directly so it never holds every block at once.
pub fn footprint(blocks: &[Block]) -> HashSet<Pubkey> {
    let mut set = HashSet::new();
    extend_footprint(&mut set, blocks);
    footprint_fixed(&mut set);
    set
}

/// The consensus bank-hash confirmations carried by a block's vote transactions.
/// A TowerSync/VoteStateUpdate vote carries the bank hash of the newest slot it
/// votes on, so votes in one block confirm the computed hashes of slots a handful
/// back. Returns `(voted slot, that slot's bank hash)` for every decodable vote.
/// Best-effort: non-vote txs and older instruction variants that carry no hash are
/// skipped — a slot with no confirming vote is reported unverified, never wrong.
pub fn vote_confirmations(block: &Block) -> Vec<(u64, Hash)> {
    use solana_vote_interface::instruction::VoteInstruction;
    let mut out = Vec::new();
    for btx in &block.transactions {
        let keys = btx.transaction.message.static_account_keys();
        for ix in btx.transaction.message.instructions() {
            if keys.get(ix.program_id_index as usize) != Some(&solana_sdk_ids::vote::id()) {
                continue;
            }
            let Ok(vote) = bincode::deserialize::<VoteInstruction>(&ix.data) else {
                continue;
            };
            // The hash is the bank hash of the last (newest) slot the vote covers.
            let (lockouts, hash) = match &vote {
                VoteInstruction::TowerSync(t) | VoteInstruction::TowerSyncSwitch(t, _) => {
                    (&t.lockouts, t.hash)
                }
                VoteInstruction::UpdateVoteState(u)
                | VoteInstruction::UpdateVoteStateSwitch(u, _)
                | VoteInstruction::CompactUpdateVoteState(u)
                | VoteInstruction::CompactUpdateVoteStateSwitch(u, _) => (&u.lockouts, u.hash),
                _ => continue,
            };
            if let Some(slot) = lockouts.iter().map(|l| l.slot()).max() {
                out.push((slot, hash));
            }
        }
    }
    out
}

/// The programData accounts for every key in `footprint`, derived as the
/// upgradeable-loader PDA of that key. An upgradeable program's bytecode lives in
/// a separate programData account (a PDA of the program id under the upgradeable
/// loader) that is NEVER a declared account key, so the footprint misses it and
/// the SVM can't load the program without it. We can't tell which keys are
/// programs before the scan, so we derive the PDA for ALL of them and let the
/// snapshot decide: a real program's programData is present and gets seeded; a
/// non-program key derives to an address the snapshot doesn't hold, which is
/// harmlessly skipped. The derivation is deterministic, so no RPC or second pass
/// is needed. Any program a tx invokes (top-level or via CPI) is a declared key,
/// so this covers CPI targets too.
pub fn programdata_addresses(footprint: &HashSet<Pubkey>) -> HashSet<Pubkey> {
    let loader = solana_sdk_ids::bpf_loader_upgradeable::id();
    footprint
        .iter()
        .map(|key| Pubkey::find_program_address(&[key.as_ref()], &loader).0)
        .collect()
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
    fn footprint_covers_static_loaded_features_and_sysvars() {
        let json: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let block = Block::from_getblock(437_680_849, &json["result"]).unwrap();
        let statics: Vec<Pubkey> = block.transactions[0]
            .transaction
            .message
            .static_account_keys()
            .to_vec();

        // Re-wrap tx0 with synthetic loaded addresses to exercise that path (the
        // fixture is all legacy, so its real loaded set is empty).
        let w = Pubkey::new_unique();
        let r = Pubkey::new_unique();
        let synthetic = Block {
            slot: 1,
            parent_slot: 0,
            blockhash: Hash::default(),
            previous_blockhash: Hash::default(),
            block_time: 0,
            transactions: vec![BlockTx {
                transaction: block.transactions[0].transaction.clone(),
                meta: TxMeta {
                    err: None,
                    fee: 0,
                    compute_units_consumed: 0,
                    pre_balances: vec![],
                    post_balances: vec![],
                    loaded_addresses: LoadedAddresses {
                        writable: vec![w],
                        readonly: vec![r],
                    },
                    post_token_balances: vec![],
                },
            }],
            fee_reward: None,
        };

        let fp = footprint(std::slice::from_ref(&synthetic));
        for key in &statics {
            assert!(fp.contains(key), "static key {key} missing from footprint");
        }
        assert!(fp.contains(&w), "loaded writable missing");
        assert!(fp.contains(&r), "loaded readonly missing");
        assert!(
            fp.contains(agave_feature_set::FEATURE_NAMES.keys().next().unwrap()),
            "feature accounts missing"
        );
        // Non-synthesized cache sysvars must be seeded from the snapshot, so the
        // footprint has to name them regardless of what the transactions touch.
        assert!(
            fp.contains(&solana_sdk_ids::sysvar::stake_history::id()),
            "StakeHistory sysvar missing from footprint"
        );
        assert!(
            fp.contains(&solana_sdk_ids::sysvar::epoch_rewards::id()),
            "EpochRewards sysvar missing from footprint"
        );
        assert!(
            fp.contains(&solana_sdk_ids::sysvar::last_restart_slot::id()),
            "LastRestartSlot sysvar missing from footprint"
        );
    }

    #[test]
    fn programdata_addresses_derive_the_upgradeable_pda() {
        // Raydium v4 and its real programData account, confirmed on-chain.
        let raydium: Pubkey = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8".parse().unwrap();
        let expected: Pubkey = "A7ZG7ByDi8DpzT9Ab7CiXhvgYTJQmaDPJkMDoPitaCQV".parse().unwrap();
        let pd = programdata_addresses(&HashSet::from([raydium]));
        assert!(
            pd.contains(&expected),
            "the upgradeable-loader PDA of Raydium v4 should be its programData account"
        );
    }

    #[test]
    fn parses_token_balances_from_meta() {
        let balances = serde_json::json!([
            {
                "accountIndex": 3,
                "mint": "So11111111111111111111111111111111111111112",
                "uiTokenAmount": { "amount": "12345" }
            }
        ]);
        let tbs = parse_token_balances(&balances).unwrap();
        assert_eq!(tbs.len(), 1);
        assert_eq!(tbs[0].account_index, 3);
        assert_eq!(tbs[0].amount, 12_345);
        // absent (null) means the tx touched no token accounts.
        assert!(
            parse_token_balances(&serde_json::Value::Null)
                .unwrap()
                .is_empty()
        );
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
    fn detects_a_durable_nonce_tx() {
        use solana_message::{
            MessageHeader, VersionedMessage, compiled_instruction::CompiledInstruction,
            legacy::Message,
        };

        let authority = Pubkey::new_unique();
        let nonce = Pubkey::new_unique();
        let recent_blockhashes = solana_sdk_ids::sysvar::recent_blockhashes::id();
        let system = solana_sdk_ids::system_program::id();

        // A legacy durable-nonce tx: authority is the writable signer, the nonce
        // account is a writable non-signer (the last two unsigned keys are readonly),
        // and the first instruction is System AdvanceNonceAccount. `build` lets the
        // negative case reuse the exact same shape with a different discriminant.
        let build = |data: Vec<u8>| VersionedTransaction {
            signatures: vec![solana_signature::Signature::default()],
            message: VersionedMessage::Legacy(Message {
                header: MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 2,
                },
                account_keys: vec![authority, nonce, recent_blockhashes, system],
                recent_blockhash: Hash::default(),
                instructions: vec![CompiledInstruction {
                    program_id_index: 3,      // system program
                    accounts: vec![1, 2, 0],  // nonce, recent_blockhashes, authority
                    data,
                }],
            }),
        };

        // AdvanceNonceAccount is SystemInstruction discriminant 4.
        let nonce_tx = build(4u32.to_le_bytes().to_vec());
        let sanitized =
            sanitize(&nonce_tx, &LoadedAddresses::default()).expect("nonce tx sanitizes");
        assert_eq!(
            sanitized.get_durable_nonce(),
            Some(&nonce),
            "the nonce account should be surfaced as the durable nonce"
        );

        // Same shape, but the first instruction is Transfer (2): not a nonce tx.
        let plain_tx = build(2u32.to_le_bytes().to_vec());
        let sanitized =
            sanitize(&plain_tx, &LoadedAddresses::default()).expect("transfer sanitizes");
        assert_eq!(sanitized.get_durable_nonce(), None);
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

    #[test]
    #[ignore = "hits a mainnet archive RPC; run with SLATE_RPC set"]
    fn fetch_confirmed_slots_live() {
        let url = std::env::var("SLATE_RPC").expect("set SLATE_RPC to an archive RPC url");
        // The fixture slot is a real confirmed block, so getBlocks over a tight
        // range around it must list it.
        let slots = fetch_confirmed_slots(&url, 437_680_848, 437_680_849).unwrap();
        assert!(
            slots.contains(&437_680_849),
            "getBlocks should list the known block, got {slots:?}"
        );
    }
}
