use anyhow::{Context, Result};
use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;
use std::collections::HashSet;
use std::str::FromStr;

fn slot_of(path: &std::path::Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    if !path.to_str()?.contains("accounts/") {
        return None;
    }
    name.split('.').next()?.parse().ok()
}

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let snap = a.next().context("usage: <snapshot.tar.zst> <pubkey>...")?;
    let want: HashSet<Pubkey> = a.map(|s| Pubkey::from_str(&s).unwrap()).collect();
    let f = std::fs::File::open(&snap)?;
    let dec = zstd::Decoder::new(std::io::BufReader::with_capacity(8 << 20, f))?;
    let mut archive = tar::Archive::new(dec);
    let mut files = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let Some(slot) = slot_of(&path) else { continue };
        files += 1;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes)?;
        for (pubkey, account) in slate_replay::snapshot::parse_append_vec(&bytes) {
            if want.contains(&pubkey) {
                println!(
                    "FOUND {pubkey}  file_slot {slot}  lamports {}  owner {}  data_len {}  exec {}",
                    account.lamports(),
                    account.owner(),
                    account.data().len(),
                    account.executable()
                );
            }
        }
        if files.is_multiple_of(2000) {
            eprintln!("  .. {files} account files scanned (last slot {slot})");
        }
    }
    eprintln!("done: {files} account files scanned");
    Ok(())
}
