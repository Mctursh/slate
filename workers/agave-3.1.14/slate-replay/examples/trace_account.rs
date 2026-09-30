use anyhow::{Context, Result};
use redb::{Database, TableDefinition};
use slate_replay::block::Block;
use solana_pubkey::Pubkey;
use std::str::FromStr;

const BLOCKS: TableDefinition<u64, &[u8]> = TableDefinition::new("blocks");

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let path = a
        .next()
        .context("usage: <cache.redb> <pubkey> [from] [to]")?;
    let want = Pubkey::from_str(&a.next().context("pubkey")?)?;
    let from: u64 = a.next().map(|s| s.parse().unwrap()).unwrap_or(0);
    let to: u64 = a.next().map(|s| s.parse().unwrap()).unwrap_or(u64::MAX);

    let db = Database::builder().open(&path)?;
    let txn = db.begin_read()?;
    let table = txn.open_table(BLOCKS)?;
    let mut hits = 0usize;
    for row in table.range(from..=to)?.flatten() {
        let (k, v) = row;
        let slot = k.value();
        let block: Block = bincode::deserialize(v.value())?;
        for (ti, btx) in block.transactions.iter().enumerate() {
            let mut keys: Vec<Pubkey> = btx.transaction.message.static_account_keys().to_vec();
            keys.extend(btx.meta.loaded_addresses.writable.iter().copied());
            keys.extend(btx.meta.loaded_addresses.readonly.iter().copied());
            if let Some(i) = keys.iter().position(|k| *k == want) {
                let pre = btx.meta.pre_balances.get(i).copied();
                let post = btx.meta.post_balances.get(i).copied();
                println!(
                    "slot {slot} tx {ti} idx {i}  pre {:?}  post {:?}  err {:?}",
                    pre, post, btx.meta.err
                );
                hits += 1;
            }
        }
    }
    println!("--- {hits} transaction(s) naming {want} in {from}..={to}");
    Ok(())
}
