// Native Stake for slots before its core-BPF migration (migrate_stake_program_to_core_bpf).
// Vendored solana-stake-program 3.0.14 (last native version) under vendor/, its 5 pinned
// =3.0.14 deps repointed to =3.1.14. Verified bit-exact for epoch 807; see
// vendor/solana-stake-program/PROVENANCE.md.

pub use solana_stake_program::stake_instruction::Entrypoint;
