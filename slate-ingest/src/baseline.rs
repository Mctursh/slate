use std::str::FromStr;

use anyhow::Context;
use base64::Engine;
use slate_store::{AccountUpdateInsert, ClickHouseClient};
use solana_pubkey::Pubkey;

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
        // data arrives as [base64, "base64"]
        let data_b64 = acct["data"][0]
            .as_str()
            .context("account missing base64 data")?;
        rows.push(AccountUpdateInsert {
            pubkey: pubkey_bytes(item["pubkey"].as_str().context("missing pubkey")?)?,
            slot: s_snap, // stamp the whole baseline at S_snap
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
