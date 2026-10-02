use anyhow::{Context, Result};
use slate_replay::store::{AccountStore, DiskStore};
use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;
use std::str::FromStr;

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let path = a.next().context("usage: <store.redb> <pubkey> [out.bin]")?;
    let key = Pubkey::from_str(&a.next().context("pubkey")?)?;
    let store = DiskStore::open(&path, 1 << 26)?;
    let (acct, slot) = store.get(&key).context("absent")?;
    println!(
        "{key} slot {slot} lamports {} owner {} len {}",
        acct.lamports(),
        acct.owner(),
        acct.data().len()
    );
    if let Some(out) = a.next() {
        std::fs::write(&out, acct.data())?;
        println!("data -> {out}");
    }
    Ok(())
}
