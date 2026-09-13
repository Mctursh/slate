// Ground truth for roll_stake_history: pull the StakeHistory sysvar out of a snapshot and print the
// entries around the boundary. A snapshot taken inside epoch N carries the entry agave wrote for N-1.
// Usage: cargo run --release -p slate-replay --example read_stake_history -- <snapshot.tar.zst> [epoch]
use std::{collections::HashSet, fs::File};

use solana_account::ReadableAccount;
use solana_stake_interface::stake_history::StakeHistory;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: <snapshot.tar.zst> [epoch]");
    let focus: Option<u64> = args.next().and_then(|e| e.parse().ok());

    let id = solana_sdk_ids::sysvar::stake_history::id();
    let accounts = slate_replay::snapshot::load_accounts(
        File::open(&path)?,
        Some(&HashSet::from([id])),
        None,
    )?;
    let (account, slot) = accounts
        .get(&id)
        .expect("StakeHistory sysvar in the snapshot");
    let history: StakeHistory = bincode::deserialize(account.data())?;

    println!(
        "\n{path}\n  sysvar written at slot {slot}, {} entries",
        history.len()
    );
    println!(
        "\n  {:>6}  {:>22}  {:>20}  {:>20}",
        "epoch", "effective", "activating", "deactivating"
    );
    for (epoch, e) in history.iter().take(6) {
        let mark = if Some(*epoch) == focus { "  <--" } else { "" };
        println!(
            "  {epoch:>6}  {:>22}  {:>20}  {:>20}{mark}",
            e.effective, e.activating, e.deactivating
        );
    }
    if let Some(want) = focus {
        match history.iter().find(|(e, _)| *e == want) {
            Some((_, e)) => println!(
                "\n  epoch {want}: effective {} activating {} deactivating {}",
                e.effective, e.activating, e.deactivating
            ),
            None => println!("\n  epoch {want} not present"),
        }
    }
    Ok(())
}
