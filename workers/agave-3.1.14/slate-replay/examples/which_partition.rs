use anyhow::{Context, Result};
use solana_epoch_rewards_hasher::EpochRewardsHasher;
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use std::str::FromStr;

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let parent_blockhash = Hash::from_str(&a.next().context("usage: <parent_blockhash> <num_partitions> <pubkey>...")?)?;
    let n: usize = a.next().context("num_partitions")?.parse()?;
    for pk in a {
        let key = Pubkey::from_str(&pk)?;
        let idx = EpochRewardsHasher::new(n, &parent_blockhash)
            .hash_address_to_partition(&key);
        println!("{idx}\t{pk}");
    }
    Ok(())
}
