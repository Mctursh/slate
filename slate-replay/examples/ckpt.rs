use slate_replay::store::{AccountStore, DiskStore};
fn main() -> anyhow::Result<()> {
    let db = std::env::args().nth(1).unwrap();
    let store = DiskStore::create(&db, 1 << 28)?;
    let slot = store.read_checkpoint().map(|(s, _)| s);
    let cap = store
        .get_meta("capitalization")
        .and_then(|b| b.get(..8).map(|s| u64::from_le_bytes(s.try_into().unwrap())));
    println!("  checkpoint slot {slot:?}  capitalization {cap:?}");
    Ok(())
}
