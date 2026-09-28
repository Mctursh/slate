use anyhow::{Context, Result};
use slate_replay::store::{AccountStore, DiskStore};
use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;
use std::str::FromStr;

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let store_path = a.next().context("usage: <store.redb> <post-balances.txt>")?;
    let list = a.next().context("list")?;
    let store = DiskStore::create(&store_path, 1 << 28)?;
    let (mut ok, mut bad, mut missing) = (0usize, 0usize, 0usize);
    for line in std::fs::read_to_string(&list)?.lines() {
        let mut it = line.split_whitespace();
        let Some(pk) = it.next() else { continue };
        let want: u64 = it.next().unwrap_or("0").parse().unwrap_or(0);
        let kind = it.next().unwrap_or("?");
        let key = Pubkey::from_str(pk)?;
        match store.get(&key) {
            None => {
                missing += 1;
                if missing <= 10 {
                    println!("MISSING  {pk} (chain post {want}, {kind})");
                }
            }
            Some((acct, slot)) if acct.lamports() != want => {
                bad += 1;
                if bad <= 20 {
                    println!(
                        "MISMATCH {pk} chain {want} slate {} (slot {slot}, {kind})",
                        acct.lamports()
                    );
                }
            }
            Some(_) => ok += 1,
        }
    }
    println!("\nmatch {ok}, mismatch {bad}, missing {missing}");
    Ok(())
}
