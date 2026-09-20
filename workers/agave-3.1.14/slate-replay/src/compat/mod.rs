// Re-supply native programs agave migrated to core BPF then deleted, so one 3.1.x
// binary replays pre-migration slots too. Gated per program on the migration feature
// that removed it. Add another via a sibling module plus a REMOVED_BUILTINS row.

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
