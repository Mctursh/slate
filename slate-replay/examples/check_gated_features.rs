// One-shot: is the feature-gating change actually a no-op at the verified epoch-808 window?
// Reads the real seeded store and reports the activation state of every gated builtin/precompile.
// Usage: cargo run --release --example check_gated_features -- <accounts.redb> <slot>
use slate_replay::{ReplayBank, build_feature_set, store::DiskStore};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: <accounts.redb> <slot>");
    let slot: u64 = args.next().expect("usage: <accounts.redb> <slot>").parse()?;

    let store = DiskStore::create(&path, 1 << 30)?;
    let bank = ReplayBank::with_store(Box::new(store));
    let feature_set = build_feature_set(&bank, slot);

    println!("store {path}\nslot  {slot}\n");

    println!("gated BUILTINS (solana_builtins::BUILTINS):");
    for b in solana_builtins::BUILTINS {
        if let Some(fid) = b.enable_feature_id {
            let active = feature_set.is_active(&fid);
            println!(
                "  {:<22} {:<7} feature {fid}",
                b.name,
                if active { "ACTIVE" } else { "INACTIVE" }
            );
        }
    }

    println!("\ngated PRECOMPILE:");
    let r1 = agave_feature_set::enable_secp256r1_precompile::id();
    println!(
        "  secp256r1              {:<7} feature {r1}",
        if feature_set.is_active(&r1) {
            "ACTIVE"
        } else {
            "INACTIVE"
        }
    );

    let ed = agave_feature_set::ed25519_precompile_verify_strict::id();
    println!(
        "  ed25519 verify_strict  {:<7} feature {ed}",
        if feature_set.is_active(&ed) {
            "ACTIVE"
        } else {
            "INACTIVE"
        }
    );

    // add_builtin only stubs an account when the store lacks one, so store presence decides whether
    // registering an inactive builtin wrote phantom state or merely populated the program cache.
    println!("\nprogram account present in the seeded store?");
    for b in solana_builtins::BUILTINS {
        if b.enable_feature_id.is_some() {
            let present = bank.store().get(&b.program_id).is_some();
            println!(
                "  {:<24} {}",
                b.name,
                if present {
                    "PRESENT (no stub written)"
                } else {
                    "ABSENT  (old code stubbed it)"
                }
            );
        }
    }

    println!(
        "\ntotal active features: {}",
        agave_feature_set::FEATURE_NAMES
            .keys()
            .filter(|id| feature_set.is_active(id))
            .count()
    );
    Ok(())
}
