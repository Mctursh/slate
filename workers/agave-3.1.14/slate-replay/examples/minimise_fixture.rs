//! Shrinks a boundary fixture to the accounts the slot actually needs.
//!
//! A full extraction carries the whole replayed footprint. Only what the boundary writes (so its
//! prior value can be mixed out of the lattice) and what it reads is required. Keeps the minimised
//! fixture only if it still replays to the same vote-confirmed hash.
//!
//! Usage: minimise_fixture <in.slfix> <out.slfix>

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use slate_format::fixture::Fixture;
use slate_replay::fixture_capture::{replay_fixture, replay_fixture_with_bank};
use solana_pubkey::Pubkey;

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() != 2 {
        bail!("usage: <in.slfix> <out.slfix>");
    }
    let full = Fixture::decode(&std::fs::read(&a[0])?)
        .map_err(|e| anyhow::anyhow!("decoding {}: {e}", a[0]))?;
    eprintln!("input: {} accounts", full.accounts.len());

    let (hash, bank) = replay_fixture_with_bank(&full).context("replaying the full fixture")?;
    if hash.to_bytes() != full.expected_bank_hash {
        bail!("the input fixture does not replay to its own recorded hash");
    }
    // A boundary writes through insert(), so take_writes() is empty; diff the bank instead.
    let mut before: HashMap<Pubkey, u64> = HashMap::with_capacity(full.accounts.len());
    for (pubkey, record) in &full.accounts {
        let r = slate_format::decode_account(record)
            .map_err(|e| anyhow::anyhow!("fixture account: {e}"))?;
        before.insert(
            Pubkey::new_from_array(*pubkey),
            value_of(r.lamports, r.rent_epoch, r.executable, &r.owner, r.data),
        );
    }
    let mut written: HashSet<Pubkey> = HashSet::new();
    bank.store().scan(&mut |pubkey, account| {
        use solana_account::ReadableAccount;
        let after = value_of(
            account.lamports(),
            account.rent_epoch(),
            account.executable(),
            &account.owner().to_bytes(),
            account.data(),
        );
        if before.get(&pubkey) != Some(&after) {
            written.insert(pubkey);
        }
    });
    eprintln!("the boundary changes {} accounts", written.len());

    let block: slate_replay::block::Block = bincode::deserialize(&full.block)?;
    let mut keep: HashSet<Pubkey> = written;
    keep.extend(slate_replay::block::footprint(std::slice::from_ref(&block)));
    keep.extend(agave_feature_set::FEATURE_NAMES.keys().copied());
    keep.extend(
        full.stake_delegations
            .iter()
            .map(|k| Pubkey::new_from_array(*k)),
    );
    if let Some(r) = &full.reward_inputs {
        keep.extend(r.vote_accounts.iter().map(|k| Pubkey::new_from_array(*k)));
    }
    keep.insert(slate_replay::compat::core_bpf::SPL_TOKEN_PROGRAM_ID);
    keep.insert(solana_sdk_ids::stake::id());
    let programdata = slate_replay::block::programdata_addresses(&keep);
    keep.extend(programdata);

    let mut small = full.clone();
    small
        .accounts
        .retain(|(k, _)| keep.contains(&Pubkey::new_from_array(*k)));
    eprintln!(
        "kept {} of {} accounts ({:.1}%)",
        small.accounts.len(),
        full.accounts.len(),
        small.accounts.len() as f64 / full.accounts.len() as f64 * 100.0
    );

    let got = replay_fixture(&small).context("replaying the minimised fixture")?;
    if got.to_bytes() != full.expected_bank_hash {
        bail!("minimised fixture replays to {got}, not the recorded hash; it is missing something");
    }

    let bytes = small.encode();
    std::fs::write(&a[1], &bytes)?;
    eprintln!(
        "wrote {} ({:.2} GB -> {:.2} GB), still vote-confirmed",
        a[1],
        std::fs::metadata(&a[0])?.len() as f64 / 1e9,
        bytes.len() as f64 / 1e9
    );
    Ok(())
}

// A digest, not the bytes: 825 carries 10.1M accounts and the values alone would be gigabytes.
fn value_of(
    lamports: u64,
    rent_epoch: u64,
    executable: bool,
    owner: &[u8; 32],
    data: &[u8],
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    lamports.hash(&mut h);
    rent_epoch.hash(&mut h);
    executable.hash(&mut h);
    owner.hash(&mut h);
    data.hash(&mut h);
    h.finish()
}
