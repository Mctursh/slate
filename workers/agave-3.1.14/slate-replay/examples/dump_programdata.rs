use slate_replay::store::{AccountStore, DiskStore};
use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;
use std::str::FromStr;

fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let path = a.next().expect("store");
    let key = Pubkey::from_str(&a.next().expect("pubkey"))?;
    let out = a.next().expect("out file");
    let store = DiskStore::open(&path, 16 * 1024 * 1024)?;
    let (acct, slot) = store.get(&key).expect("account absent");
    let data = acct.data();
    let elf = if data.len() > 45 && data[0..4] == [3, 0, 0, 0] {
        &data[45..]
    } else {
        data
    };
    std::fs::write(&out, elf)?;
    println!("wrote {} bytes from slot {slot} to {out}", elf.len());
    Ok(())
}
