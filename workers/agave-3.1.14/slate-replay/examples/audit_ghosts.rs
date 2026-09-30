use anyhow::{Context, Result};
use slate_replay::store::{AccountStore, DiskStore};
use solana_account::{AccountSharedData, ReadableAccount};
use solana_pubkey::Pubkey;
use std::collections::HashMap;

fn slot_of(path: &std::path::Path) -> Option<u64> {
    if !path.to_str()?.contains("accounts/") {
        return None;
    }
    path.file_name()?.to_str()?.split('.').next()?.parse().ok()
}

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let snap = a
        .next()
        .context("usage: <snapshot.tar.zst> <store.redb> [--apply]")?;
    let store_path = a.next().context("store path")?;
    let apply = a.next().is_some_and(|f| f == "--apply");

    // The store is ~5M rows against >1e9 snapshot records, so candidates come from the store.
    let mut store = DiskStore::open(&store_path, 1 << 30)?;
    let mut held: HashMap<Pubkey, u64> = HashMap::new();
    store.scan(&mut |pubkey, account| {
        if account.lamports() > 0 {
            held.insert(pubkey, account.lamports());
        }
    });
    eprintln!("store holds {} funded accounts", held.len());
    let mut store_slot: HashMap<Pubkey, u64> = HashMap::new();
    for pk in held.keys() {
        if let Some((_, slot)) = store.get(pk) {
            store_slot.insert(*pk, slot);
        }
    }

    let f = std::fs::File::open(&snap)?;
    let dec = zstd::Decoder::new(std::io::BufReader::with_capacity(8 << 20, f))?;
    let mut archive = tar::Archive::new(dec);
    let mut top: HashMap<Pubkey, (u64, u64)> = HashMap::new();
    let mut files = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let Some(slot) = slot_of(&path) else { continue };
        files += 1;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes)?;
        for (pubkey, account) in slate_replay::snapshot::parse_append_vec(&bytes) {
            if !held.contains_key(&pubkey) {
                continue;
            }
            let lamports = account.lamports();
            match top.get(&pubkey) {
                Some((prev, _)) if *prev >= slot => {}
                _ => {
                    top.insert(pubkey, (slot, lamports));
                }
            }
        }
        if files.is_multiple_of(50_000) {
            eprintln!(
                "  .. {files} account files, {} candidates resolved",
                top.len()
            );
        }
    }
    eprintln!("scanned {files} account files");

    let mut ghosts = Vec::new();
    for (pubkey, (top_slot, top_lamports)) in &top {
        if *top_lamports == 0 {
            let had = store_slot.get(pubkey).copied().unwrap_or(0);
            if had < *top_slot {
                let lam = held.get(pubkey).copied().unwrap_or(0);
                ghosts.push((*pubkey, *top_slot, had, lam));
            }
        }
    }
    println!("GHOSTS FOUND: {}", ghosts.len());
    for (pk, del, had, lam) in ghosts.iter().take(40) {
        println!("  {pk}  store_slot {had} lamports {lam}  -> deleted at {del}");
    }
    if ghosts.len() > 40 {
        println!("  ... and {} more", ghosts.len() - 40);
    }

    if apply {
        for (pk, del_slot, _, _) in &ghosts {
            store.put(
                *pk,
                AccountSharedData::new(0, 0, &Pubkey::default()),
                *del_slot,
            );
        }
        store.flush();
        println!("APPLIED {} tombstone(s)", ghosts.len());
    } else if !ghosts.is_empty() {
        println!("dry run; pass --apply to write tombstones");
    }
    Ok(())
}
