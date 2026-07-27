//! validate — differential correctness harness.
//!
//! Slate's marquee claim is answering for a *past* slot, which a live RPC can't check directly (it
//! forgot). So we borrow the Cloudbreak/Superbank pattern — diff against a reference RPC — and add
//! a time twist: read a program's full account set from an independent RPC at its current finalized
//! slot `S1`, wait until Slate has captured *past* `S1`, then ask Slate for the same program as-of
//! `S1`. If Slate's live-captured reconstruction of a now-historical slot matches an RPC that never
//! fed it, the ingest pipeline is correct. No snapshot, no data download — just a reference RPC.
//!
//! `rent_epoch` is deliberately NOT part of the pass/fail check: it's a vestigial field (`u64::MAX`
//! for rent-exempt accounts) that providers report inconsistently, and Slate stores whatever its
//! source hands it — so a difference reflects provider disagreement, not a Slate bug. We tally it
//! separately for transparency instead of failing on it.

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::Context;
use base64::Engine;
use slate_store::{ClickHouseClient, Fidelity};
use solana_pubkey::Pubkey;

/// The normalized account state both sides reduce to, so the oracle and Slate are directly
/// comparable regardless of wire shape.
#[derive(Debug, Clone)]
pub struct AcctState {
    pub owner: [u8; 32],
    pub lamports: u64,
    pub executable: bool,
    pub rent_epoch: u64,
    pub data: Vec<u8>,
}

/// Fetch every account owned by `program` from the reference RPC as of its current finalized slot.
/// Returns that slot (`S1`) and the accounts keyed by pubkey. This is the independent oracle — an
/// RPC that has never seen Slate's data. Use one that did NOT seed Slate's baseline.
pub async fn fetch_oracle(
    rpc_url: &str,
    program: &str,
) -> anyhow::Result<(u64, HashMap<[u8; 32], AcctState>)> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getProgramAccounts",
        // base64 (not base64+zstd): zstd is non-deterministic across implementations and would
        // false-positive on every account. finalized + withContext so we get the exact slot.
        "params": [program, { "encoding": "base64", "commitment": "finalized", "withContext": true }],
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
    let s1 = result["context"]["slot"]
        .as_u64()
        .context("reference RPC response missing context.slot")?;

    let items = result["value"]
        .as_array()
        .context("reference RPC response missing value[]")?;
    let mut map = HashMap::with_capacity(items.len());
    for item in items {
        let acct = &item["account"];
        let pubkey = pubkey_bytes(item["pubkey"].as_str().context("missing pubkey")?)?;
        let data_b64 = acct["data"][0].as_str().context("missing base64 data")?;
        map.insert(
            pubkey,
            AcctState {
                owner: pubkey_bytes(acct["owner"].as_str().context("missing owner")?)?,
                lamports: acct["lamports"].as_u64().context("missing lamports")?,
                executable: acct["executable"].as_bool().unwrap_or(false),
                rent_epoch: acct["rentEpoch"].as_u64().unwrap_or(0),
                data: base64::engine::general_purpose::STANDARD.decode(data_b64)?,
            },
        );
    }
    Ok((s1, map))
}

/// Ask Slate for the same program as-of slot `s1`, reshaped into the comparable map, plus the
/// fidelity Slate assigns to that answer (the caller respects it — see the bin).
pub async fn fetch_slate(
    store: &ClickHouseClient,
    program: &[u8; 32],
    s1: u64,
) -> anyhow::Result<(Fidelity, HashMap<[u8; 32], AcctState>)> {
    let answer = store.get_program_accounts_as_of(program, s1).await?;
    let mut map = HashMap::with_capacity(answer.accounts.len());
    for a in answer.accounts {
        map.insert(
            a.pubkey,
            AcctState {
                owner: a.owner,
                lamports: a.lamports,
                executable: a.executable != 0,
                rent_epoch: a.rent_epoch,
                data: a.data,
            },
        );
    }
    Ok((answer.fidelity, map))
}

/// A single disagreement between the oracle and Slate.
#[derive(Debug)]
pub enum Diff {
    /// In the reference RPC but absent from Slate — a write Slate missed.
    MissingInSlate([u8; 32]),
    /// In Slate but absent from the reference RPC — Slate is holding stale/extra state.
    ExtraInSlate([u8; 32]),
    /// Both have it, but a field disagrees.
    Field {
        pubkey: [u8; 32],
        field: &'static str,
        oracle: String,
        slate: String,
    },
}

/// The outcome of a diff. `diffs` is the pass/fail set (never includes `rent_epoch`);
/// `rent_epoch_diffs` counts accounts that disagreed only on the vestigial field, for transparency.
pub struct DiffReport {
    pub diffs: Vec<Diff>,
    pub rent_epoch_diffs: usize,
    pub compared: usize,
}

/// Diff two account sets (order-agnostic — keyed by pubkey). Compares owner, lamports, executable,
/// and data exactly; `rent_epoch` is only tallied, never failed on.
pub fn diff(
    oracle: &HashMap<[u8; 32], AcctState>,
    slate: &HashMap<[u8; 32], AcctState>,
) -> DiffReport {
    let mut diffs = Vec::new();
    let mut rent_epoch_diffs = 0usize;

    for (pubkey, o) in oracle {
        let Some(s) = slate.get(pubkey) else {
            diffs.push(Diff::MissingInSlate(*pubkey));
            continue;
        };
        if o.owner != s.owner {
            diffs.push(Diff::Field {
                pubkey: *pubkey,
                field: "owner",
                oracle: bs58(&o.owner),
                slate: bs58(&s.owner),
            });
        }
        if o.lamports != s.lamports {
            diffs.push(Diff::Field {
                pubkey: *pubkey,
                field: "lamports",
                oracle: o.lamports.to_string(),
                slate: s.lamports.to_string(),
            });
        }
        if o.executable != s.executable {
            diffs.push(Diff::Field {
                pubkey: *pubkey,
                field: "executable",
                oracle: o.executable.to_string(),
                slate: s.executable.to_string(),
            });
        }
        if o.data != s.data {
            diffs.push(Diff::Field {
                pubkey: *pubkey,
                field: "data",
                oracle: format!("{} bytes [{}…]", o.data.len(), hex8(&o.data)),
                slate: format!("{} bytes [{}…]", s.data.len(), hex8(&s.data)),
            });
        }
        // rent_epoch: informational only, never a failure (see module docs).
        if o.rent_epoch != s.rent_epoch {
            rent_epoch_diffs += 1;
        }
    }

    for pubkey in slate.keys() {
        if !oracle.contains_key(pubkey) {
            diffs.push(Diff::ExtraInSlate(*pubkey));
        }
    }

    DiffReport {
        diffs,
        rent_epoch_diffs,
        compared: oracle.len(),
    }
}

fn pubkey_bytes(s: &str) -> anyhow::Result<[u8; 32]> {
    Ok(Pubkey::from_str(s)?.to_bytes())
}

fn bs58(b: &[u8; 32]) -> String {
    Pubkey::from(*b).to_string()
}

fn hex8(b: &[u8]) -> String {
    b.iter().take(8).map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = b;
        a
    }
    fn st(lamports: u64, data: &[u8]) -> AcctState {
        AcctState {
            owner: pk(0xC0),
            lamports,
            executable: false,
            rent_epoch: 0,
            data: data.to_vec(),
        }
    }

    /// The three failure shapes: an account only the oracle has, one only Slate has, and one both
    /// have but whose data disagrees.
    #[test]
    fn diff_flags_missing_extra_and_field() {
        let mut oracle = HashMap::new();
        oracle.insert(pk(1), st(10, b"a")); // matches
        oracle.insert(pk(2), st(20, b"b")); // missing in slate
        oracle.insert(pk(3), st(30, b"c")); // data differs
        let mut slate = HashMap::new();
        slate.insert(pk(1), st(10, b"a"));
        slate.insert(pk(3), st(30, b"DIFFERENT"));
        slate.insert(pk(4), st(40, b"d")); // extra in slate

        let r = diff(&oracle, &slate);
        assert_eq!(r.compared, 3);
        assert_eq!(r.diffs.len(), 3, "one missing, one field, one extra");
    }

    /// A rent_epoch-only difference must never fail the run, but must be counted.
    #[test]
    fn rent_epoch_never_fails_but_is_counted() {
        let mut oracle = HashMap::new();
        let mut a = st(10, b"x");
        a.rent_epoch = u64::MAX;
        oracle.insert(pk(1), a);
        let mut slate = HashMap::new();
        let mut b = st(10, b"x");
        b.rent_epoch = 0;
        slate.insert(pk(1), b);

        let r = diff(&oracle, &slate);
        assert!(r.diffs.is_empty(), "rent_epoch diff must not fail the run");
        assert_eq!(r.rent_epoch_diffs, 1);
    }
}
