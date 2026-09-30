use slate_replay::{build_feature_set, store::DiskStore, ReplayBank};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: <accounts.redb> <slot>");
    let slot: u64 = args.next().expect("usage: <accounts.redb> <slot>").parse()?;
    let store = DiskStore::open(&path, 1 << 28)?;
    let bank = ReplayBank::with_store(Box::new(store));
    let fs = build_feature_set(&bank, slot);
    for (id, name) in agave_feature_set::FEATURE_NAMES.iter() {
        let at = fs.activated_slot(id);
        println!(
            "{id}\t{}\t{}\t{name}",
            if fs.is_active(id) { "ACTIVE" } else { "inactive" },
            at.map(|s| s.to_string()).unwrap_or_else(|| "-".into())
        );
    }
    Ok(())
}
