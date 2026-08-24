//! Historical-runtime compatibility for replay.
//!
//! A recent agave runtime plus the per-slot feature set already reproduces almost
//! all historical behavior: agave gates its consensus changes, so handing it the
//! feature set that was live at slot N reproduces slot N. The one thing it can't
//! reproduce is code agave *deleted* — when a core-BPF migration finishes, agave
//! removes the native program (no real validator replays pre-migration slots).
//! Slate does. This module re-supplies exactly that deleted code, gated on the
//! same feature that deleted it, so one binary spans every epoch from 807 to now.
//!
//! To handle another deleted program: add its native implementation in a sibling
//! module and add one row to [`REMOVED_BUILTINS`]. The logic below never changes.

mod stake;

use agave_feature_set::FeatureSet;
use solana_program_runtime::{
    invoke_context::BuiltinFunctionWithContext, loaded_programs::ProgramCacheEntry,
};
use solana_pubkey::Pubkey;
use solana_svm::transaction_processor::TransactionBatchProcessor;

use crate::{ReplayBank, SlateForkGraph};

/// A native program agave migrated to core BPF and then deleted. It's native for
/// slots before `removed_by` activates; agave's on-chain BPF program takes over
/// after. Registering it as a builtin (not a seeded BPF account) is what gives it
/// the fixed native compute cost the pre-migration runtime charged.
struct RemovedBuiltin {
    id: Pubkey,
    name: &'static str,
    entrypoint: BuiltinFunctionWithContext,
    /// The core-BPF migration feature that deleted the native program.
    removed_by: Pubkey,
}

const REMOVED_BUILTINS: &[RemovedBuiltin] = &[RemovedBuiltin {
    id: solana_sdk_ids::stake::id(),
    name: "stake",
    entrypoint: stake::Entrypoint::vm,
    removed_by: agave_feature_set::migrate_stake_program_to_core_bpf::id(),
}];

/// Register the deleted builtins this slot still needs, given its feature set.
/// Call once during setup, after the current builtins are registered. A no-op for
/// any program whose migration is already active (agave's BPF account covers it),
/// which is what keeps one binary correct across every migration boundary.
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
