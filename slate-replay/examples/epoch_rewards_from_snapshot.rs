// agave's own EpochRewards values for a boundary, read from a later snapshot: the account keeps its
// contents after deactivation, so total_points survives where block data cannot show it.
use solana_account::ReadableAccount;
use solana_epoch_rewards::EpochRewards;
use std::{collections::HashSet, fs::File};
fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap();
    let slot: u64 = std::env::args().nth(2).unwrap().parse()?;
    let id = solana_sdk_ids::sysvar::epoch_rewards::id();
    let accounts = slate_replay::snapshot::load_accounts(
        File::open(&path)?,
        Some(&HashSet::from([id])),
        None,
    )?;
    let (a, s) = accounts.get(&id).expect("epoch rewards sysvar");
    let e: EpochRewards = bincode::deserialize(a.data())?;
    println!("  snapshot {path} (slot {slot}), account written at slot {s}");
    println!(
        "  lamports={} rent_epoch={} data={}",
        a.lamports(),
        a.rent_epoch(),
        a.data().len()
    );
    println!(
        "  distribution_starting_block_height={}",
        e.distribution_starting_block_height
    );
    println!("  num_partitions={}", e.num_partitions);
    println!("  parent_blockhash={}", e.parent_blockhash);
    println!("  total_points={}", e.total_points);
    println!("  total_rewards={}", e.total_rewards);
    println!("  distributed_rewards={}", e.distributed_rewards);
    println!("  active={}", e.active);
    Ok(())
}
