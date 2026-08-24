//! Native Stake program, for slots before its core-BPF migration
//! (`migrate_stake_program_to_core_bpf`). Agave 3.1.x deleted the native
//! implementation after the migration, so historical replay has to re-supply it.
//!
//! We reuse the real thing — `solana-stake-program` 3.0.14, the last version that
//! shipped the native processor before it became the core-BPF program at 4.0.0 —
//! vendored under `vendor/` with its 5 pinned `=3.0.14` deps repointed to
//! `=3.1.14` so it builds against this runtime. Registering its `Entrypoint` as a
//! builtin charges the fixed native compute cost the pre-migration runtime used,
//! unlike the on-chain BPF program's per-instruction metering.
//!
//! Verified bit-exact for the epoch-807 range against vote-confirmed mainnet
//! bank hashes. See vendor/solana-stake-program/PROVENANCE.md.

pub use solana_stake_program::stake_instruction::Entrypoint;
