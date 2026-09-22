// Re-supply native programs agave migrated to core BPF then deleted, so one 3.1.x
// binary replays pre-migration slots too. Gated per program on the migration feature
// that removed it. Add another via a sibling module plus a REMOVED_BUILTINS row.

pub mod core_bpf;
mod stake;

use agave_feature_set::FeatureSet;
use solana_program_runtime::{
    invoke_context::BuiltinFunctionWithContext, loaded_programs::ProgramCacheEntry,
};
use solana_pubkey::Pubkey;
use solana_svm::transaction_processor::TransactionBatchProcessor;

use crate::{ReplayBank, SlateForkGraph};

// Native before removed_by activates, agave's on-chain BPF program after. Registered
// as a builtin (not a seeded BPF account) for the fixed native compute cost.
struct RemovedBuiltin {
    id: Pubkey,
    name: &'static str,
    entrypoint: BuiltinFunctionWithContext,
    removed_by: Pubkey,
}

const REMOVED_BUILTINS: &[RemovedBuiltin] = &[RemovedBuiltin {
    id: solana_sdk_ids::stake::id(),
    name: "stake",
    entrypoint: stake::Entrypoint::vm,
    removed_by: agave_feature_set::migrate_stake_program_to_core_bpf::id(),
}];

// Run any core-BPF migration whose feature activated at this crossing. Keyed on the newly
// activated set, so it fires exactly once, in the block the feature turns on.
pub fn apply_core_bpf_migrations(
    bank: &mut ReplayBank,
    processor: &TransactionBatchProcessor<SlateForkGraph>,
    parent: &TransactionBatchProcessor<SlateForkGraph>,
    activated: &[Pubkey],
    epoch: u64,
    slot: u64,
) {
    if activated.contains(&agave_feature_set::migrate_stake_program_to_core_bpf::id()) {
        match core_bpf::migrate_stake_to_core_bpf(bank, processor, parent, epoch, slot) {
            Ok(m) => eprintln!(
                "epoch {epoch}: {} migrated to core BPF, programdata {}, burned {} funded {}",
                m.program_address, m.program_data_address, m.burned, m.funded
            ),
            // Halt rather than continue: a failed migration means every later stake transaction
            // replays against the wrong program, and the bank hash diverges from here on.
            Err(e) => panic!("epoch {epoch}: stake core-BPF migration failed: {e:?}"),
        }
    }
    if activated.contains(&agave_feature_set::vote_state_v4::id()) {
        match core_bpf::upgrade_stake_for_vote_state_v4(bank, processor, epoch, slot) {
            Ok(m) => eprintln!(
                "epoch {epoch}: {} upgraded for vote_state_v4, programdata {}, burned {} funded {}",
                m.program_address, m.program_data_address, m.burned, m.funded
            ),
            Err(e) => panic!("epoch {epoch}: stake vote_state_v4 upgrade failed: {e:?}"),
        }
    }
}

// No-op for any program whose migration is already active (agave's BPF account covers
// it), which keeps one binary correct across every migration boundary.
pub fn register_removed_builtins(
    bank: &mut ReplayBank,
    processor: &TransactionBatchProcessor<SlateForkGraph>,
    feature_set: &FeatureSet,
) {
    for b in REMOVED_BUILTINS {
        if feature_set.is_active(&b.removed_by) {
            continue; // migrated on chain: agave's BPF program handles it
        }
        bank.add_builtin(
            processor,
            b.id,
            b.name,
            ProgramCacheEntry::new_builtin(0, b.name.len(), b.entrypoint),
        );
    }
}
