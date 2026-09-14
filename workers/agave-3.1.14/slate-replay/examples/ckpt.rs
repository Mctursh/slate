//! Dump a store's resume checkpoint: `cargo run --example ckpt -- <store.redb>`.
//! Reads through slate-format, so it also checks the build can resume from it.

use slate_replay::store::{AccountStore, DiskStore};

fn main() -> anyhow::Result<()> {
    let db = std::env::args().nth(1).unwrap();
    let store = DiskStore::create(&db, 1 << 28)?;

    let Some(blob) = store.read_checkpoint() else {
        println!("no checkpoint in {db}");
        return Ok(());
    };
    let c = slate_format::Checkpoint::decode(&blob)?;

    println!("slot                {}", c.slot);
    println!("capitalization      {}", c.capitalization);
    println!("stake keys          {}", c.stake_keys.len());
    println!(
        "pending partitions  {} ({} rewards)",
        c.pending_partitions.len(),
        c.pending_partitions.iter().map(Vec::len).sum::<usize>()
    );
    match &c.roll {
        Some(roll) => println!(
            "bank hash           {}\nlt hash             {}",
            bs58::encode(roll.bank_hash).into_string(),
            roll.lt_hash.checksum()
        ),
        None => println!("bank hash           (roll not active)"),
    }
    Ok(())
}
