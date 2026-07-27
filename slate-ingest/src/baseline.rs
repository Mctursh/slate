//! baseline — bootstrap Slate's coverage floor from a `getProgramAccounts` snapshot.
//!
//! For a program-scoped Slate, the complete account set at a slot is exactly what
//! `getProgramAccounts` returns against a full RPC. We fetch it with `withContext` (which hands
//! back the exact slot), stamp every account at that slot, and record coverage there — the same
//! shape `read_snapshot_accounts` produces from a snapshot file, so `live` can use either source.
//!
//! Scope must be small enough for the RPC to return the whole set (SPL Token and the like are too
//! big — use a full snapshot for those).

use std::str::FromStr;

use anyhow::Context;
use base64::Engine;
use slate_store::{AccountUpdateInsert, ClickHouseClient};
use solana_pubkey::Pubkey;

/// Fetch every account owned by `owner` as of the current finalized slot, load them as the
/// baseline, record coverage there, and return that slot (`S_snap`). The caller then subscribes
/// `from_slot = S_snap + 1` to continue contiguously.
pub async fn fetch_program_baseline(
    store: &ClickHouseClient,
    rpc_url: &str,
    owner: &str,
) -> anyhow::Result<u64> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getProgramAccounts",
        "params": [owner, { "encoding": "base64", "commitment": "finalized", "withContext": true }],
    });

    let resp: serde_json::Value = reqwest::Client::new()
        .post(rpc_url)
        .json(&request)
        .send()
        .await?
        .json()
        .await?;

    // withContext wraps the accounts as { context: { slot }, value: [...] }.
    let result = resp
        .get("result")
        .with_context(|| format!("getProgramAccounts returned no result: {resp}"))?;
    let s_snap = result["context"]["slot"]
        .as_u64()
        .context("getProgramAccounts response missing context.slot")?;

    let accounts = result["value"]
        .as_array()
        .context("getProgramAccounts response missing value[]")?;
    let mut rows = Vec::with_capacity(accounts.len());
    for item in accounts {
        let acct = &item["account"];
        // data is [base64_string, "base64"].
        let data_b64 = acct["data"][0]
            .as_str()
            .context("account missing base64 data")?;
        rows.push(AccountUpdateInsert {
            pubkey: pubkey_bytes(item["pubkey"].as_str().context("missing pubkey")?)?,
            slot: s_snap, // whole baseline stamped at S_snap = state as of this slot
            write_version: 0,
            owner: pubkey_bytes(acct["owner"].as_str().context("missing owner")?)?,
            lamports: acct["lamports"].as_u64().context("missing lamports")?,
            executable: acct["executable"].as_bool().unwrap_or(false) as u8,
            rent_epoch: acct["rentEpoch"].as_u64().unwrap_or(0),
            data: base64::engine::general_purpose::STANDARD.decode(data_b64)?,
        });
    }

    store.insert_accounts(&rows).await?;
    store.record_coverage(s_snap, s_snap).await?;
    Ok(s_snap)
}

fn pubkey_bytes(s: &str) -> anyhow::Result<[u8; 32]> {
    Ok(Pubkey::from_str(s)?.to_bytes())
}
