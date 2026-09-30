use slate_replay::store::{AccountStore, DiskStore};
use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;
use std::str::FromStr;

fn show(store: &DiskStore, label: &str, key: &Pubkey) -> Option<Vec<u8>> {
    match store.get(key) {
        None => {
            println!("{label} {key}: ABSENT");
            None
        }
        Some((a, slot)) => {
            println!(
                "{label} {key}: owner={} lamports={} len={} last_write_slot={} exec={}",
                a.owner(),
                a.lamports(),
                a.data().len(),
                slot,
                a.executable()
            );
            Some(a.data().to_vec())
        }
    }
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("store path");
    let program = Pubkey::from_str(
        &std::env::args()
            .nth(2)
            .unwrap_or_else(|| "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc".to_string()),
    )?;
    let store = DiskStore::open(&path, 64 * 1024 * 1024)?;

    let data = show(&store, "program    ", &program);
    let Some(data) = data else { return Ok(()) };
    if data.len() >= 36 && data[0..4] == [2, 0, 0, 0] {
        let pd = Pubkey::new_from_array(<[u8; 32]>::try_from(&data[4..36]).unwrap());
        if let Some(pd_data) = show(&store, "programdata", &pd) {
            let off = 45usize;
            if pd_data.len() > off + 0x34 {
                let elf = &pd_data[off..];
                println!("  elf magic  = {:02x?}", &elf[0..4]);
                println!(
                    "  e_flags    = {}",
                    u32::from_le_bytes(elf[0x30..0x34].try_into().unwrap())
                );
                println!(
                    "  deploy_slot(in programdata header) = {}",
                    u64::from_le_bytes(pd_data[4..12].try_into().unwrap())
                );
                println!("  elf bytes  = {}", elf.len());
            }
        }
    } else {
        println!("  program account is not loader-v3 Program state: first4={:?}", &data[0..4.min(data.len())]);
    }
    Ok(())
}
