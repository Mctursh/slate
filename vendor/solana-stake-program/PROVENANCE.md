# solana-stake-program (vendored)

The native Stake program, vendored so Slate can execute stake transactions from
slots that predate its core-BPF migration.

## Source

- Crate: `solana-stake-program` 3.0.14, from crates.io
- Upstream: https://github.com/anza-xyz/agave, path `programs/stake`
- Commit: `f516a8927c76c07f0ebc54ca7a4ce8b2046eee86` (recorded by crates.io in the
  original `.cargo_vcs_info.json`, which is removed here, see below)

3.0.14 is the last release that shipped the native Stake processor. At 4.0.0 the
crate was repurposed to the core-BPF program, so the native processor only
exists up to this version.

## Why it's here

Agave migrates native builtins to on-chain BPF and then deletes the native code.
`migrate_stake_program_to_core_bpf` activated on mainnet at slot 355536000.
Slate replays epoch 807 (slot ~349047024), which is before that, so Stake still
ran natively there. Slate's runtime is agave 3.1.x, which no longer carries the
native Stake builtin. Without re-supplying it, any stake or stake-pool
transaction hits a program the runtime can't find, and replay halts.

`slate-replay/src/compat/` registers this processor as a builtin, gated on the
migration feature so it's only active for pre-migration slots. It's a builtin
with the fixed native compute cost, not a seeded BPF account. Seeding it as BPF
meters per bytecode instruction, which made pre-migration transactions blow
their compute budget.

## Changes from upstream 3.0.14

The processor logic is unchanged. The edits are only what it takes to build the
3.0.14 source against the 3.1.14 runtime, plus trimming to the library.

Build fixes:

1. `Cargo.toml`: the 5 runtime dep pins repointed `=3.0.14` -> `=3.1.14`:
   `agave-feature-set`, `solana-program-runtime`, `solana-svm-log-collector`,
   `solana-svm-type-overrides`, `solana-transaction-context`.
2. `src/lib.rs`: added `#![allow(deprecated)]`. Built against the 3.1.x deps the
   code uses items that are now `#[deprecated]`, and the crate compiles with
   `[lints.rust] warnings = "deny"`, so the warnings would fail the build. The
   allow keeps them as warnings without touching code.
3. `src/config.rs`, `src/stake_state.rs`: `solana_transaction_context`'s
   `BorrowedAccount` was renamed to `BorrowedInstructionAccount` in 3.1.x.
   Imported with `as BorrowedAccount` so the bodies stay exactly as upstream.

Trimmed to only what compiles into the library:

4. Removed the crate's own `#[cfg(test)] mod tests` from `stake_instruction.rs`
   and `stake_state.rs`, the `benches/` directory and its `[[bench]]` entry, and
   all `[dev-dependencies]`. None of it links into the library Slate uses.
5. Removed crates.io packaging artifacts: `Cargo.lock`, `Cargo.toml.orig`, and
   `.cargo_vcs_info.json`. Cargo doesn't read them for a path dependency, and the
   source commit they carried is recorded under Source above.

## Re-vendoring a newer version

When Slate's runtime moves off 3.1.14, or to pull a newer stake processor:

1. Get the source for the version you want. Either the crates.io crate, or
   `programs/stake` from the agave repo at the matching release tag.
2. Copy `src/` and `Cargo.toml` over this directory.
3. Repoint the runtime dep pins to match slate-replay's runtime version.
4. Re-apply the two source deltas above if the APIs still differ (the
   `BorrowedAccount` alias, the `#![allow(deprecated)]`).
5. Strip the tests, benches, and dev-dependencies again.
6. `cargo build -p slate-replay`, then replay and confirm the bank hashes are
   still bit-exact (see below).

## Verification

Registered through the compat module, this processor replays slots 349047032
through 349047035 of epoch 807 bit-exact against the mainnet bank hashes. Slot
349047033 is a GLAM -> stake-pool -> Stake CPI, the transaction that first
exposed the missing builtin. Ground-truth hashes come from vote transactions in
later blocks: a vote carries the bank hash of its newest lockout slot, so no
separate oracle box is needed.
