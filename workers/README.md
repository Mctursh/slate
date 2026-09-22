# Era workers

Slate replays mainnet from epoch 807 forward. No single agave build covers that range: a
version doesn't know features that activate after it was cut, and it has already deleted
code that earlier epochs need. So the range is sliced into **eras**, each replayed by a
worker linking an agave contemporaneous with it.

Eras are **seeded independently, not chained.** Every epoch in the range has a full account
snapshot in the Solana Foundation warehouse buckets, so each era starts from its own
snapshot and eras can be replayed in parallel. Two eras meeting at a slot is therefore a
*verification* opportunity — both compute a bank hash there, and both must match consensus —
rather than a data dependency.

```
slate/
  slate-hash/  slate-format/  slate-store/  slate-common/   shared, byte-oriented
  slate-rpc/   slate-ingest/                                live pipeline
  workers/
    agave-3.1.14/        own workspace, Cargo.lock, rust-toolchain.toml
      slate-replay/  slate-backfill/  vendor/
```

## Why each worker is its own workspace

A Cargo workspace has one lockfile and resolves every member together. If eras shared one,
adding an era could re-resolve an existing era's dependency graph, and that era's proof
would no longer describe anything you can rebuild. So each worker pins its own
`Cargo.lock` and its own `rust-toolchain.toml` — per worker, because a later era may need a
newer Rust than an older era still builds on.

The point: **fixing one era re-proves only that era.** The explicit exception is a change to
a shared crate, which re-proves all of them. That is why the shared layer is deliberately
small.

## The shared layer

`Pubkey` at 3.0.0 and 4.3.0 are different types that don't interoperate, so a "shared" crate
taking typed solana values is version-locked, not shared. **Everything here takes bytes.**

- **`slate-hash`** — lattice hash and bank hash, i.e. the code that computes the proof.
  Shared so cross-era results are comparable. Owns its `LtHash` rather than depending on
  `solana-lattice-hash`, which is itself already multi-major in our lockfiles.
- **`slate-format`** — the on-disk contracts: account record and resume checkpoint, each
  versioned, each refusing a version it doesn't recognise.
- **`slate-store`**, **`slate-common`** — no solana dependencies at all.

## Eras

Re-verified 2026-09-20 against mainnet (genesis `5eykt4Us...`, tip epoch 1038).

| Worker | agave | Floor | Ceiling | Status |
|--------|-------|-------|---------|--------|
| `agave-3.1.14` | 3.1.14 | 823 static, **807 shimmed** | 978 | era 1, all 4 structural boundaries verified |
| *(not built)* | 4.2.1 | 953 | ≥ tip | era 2, covers 979.. |

**Two eras cover 807 → tip.** 4.2.1's floor sits comfortably below 979, so it picks up where
3.1.14 stops.

- **Ceiling** = the first mainnet-activated feature whose id this version's feature-set crate
  lacks. 3.1.14 lacks `create_account_allow_prefund` (979).
- **Floor** = the latest activation whose *gate* this version deleted. Past that point the
  version assumes the feature is always on and mis-executes earlier epochs. 3.1.14 dropped
  `migrate_stake_program_to_core_bpf` (823); 4.2.1 dropped `relax_intrabatch_account_locks`
  (953).

`agave-3.1.14` reaches back to 807 only via two shims it already carries: the vendored native
Stake program (pre-823 behaviour) and the SIMD-0162 executable-flag check (811). The 807→808
bit-exact proof is the evidence they work. The shims supply pre-migration *behaviour*; the 823
migration event that writes the accounts is implemented separately in `compat/core_bpf.rs`, and
the 822→823 crossing is bit-exact.

## What "green" means for an era

An era spans ~172 epochs ≈ 74 million slots. Replaying all of them is not on the table, and
static version analysis cannot substitute: it selects candidate versions and tells you where
to look, nothing more. So:

> **An era is green when every execution-relevant feature-activation boundary and structural
> event in its range has a replayed window in which (a) every slot's bank hash matches a
> consensus vote, and (b) the end state diffs byte-for-byte clean against the real mainnet
> snapshot at the window's end.**

This is a weaker claim than "bit-exact across the whole range" and the difference should stay
visible. It is defensible because version-compatibility bugs are not uniformly distributed —
they cluster exactly where behaviour changes, so uniform sampling mostly tests slots where
nothing can differ.

Verifying two ways matters: the bank hash says *something* diverged, the snapshot diff says
*which account*. Both exist already (`hash-check: N slots consensus-verified against votes`,
and `--verify-boundary`); a 50,079-slot run has previously matched a real snapshot at
8,412,739/8,412,739 accounts.

An era also owes its **expected per-slot hashes committed** beside it (they're tiny), so a
re-proof is a comparison and the third-party inputs stay replaceable; and its **retained
inputs** on external storage, not in the repo.

### era 1 checklist (`agave-3.1.14`, 807..978)

27 features activate in range. Triage first — several are networking-layer
(`drop_unchained_merkle_shreds`, `enforce_fixed_fec_set`, `switch_to_chacha8_turbine`,
`disable_turbine_fanout_experiments`) and cannot affect replayed account state.

    807 808 809 811 818 821 822 823 825 836 841 878 880 884 890
    933 935 936 940 942 943 946 949 950 953 956 971

Four are **structural** — they write accounts at the boundary. All four are implemented, and
each one's boundary slot reproduces the bank hash mainnet's own votes committed to:

| epoch | event | what it writes | boundary bank hash |
|-------|-------|----------------|--------------------|
| 823 | `migrate_stake_program_to_core_bpf` | Stake native -> core BPF; 3.1.14 dropped the config, so Slate supplies it | `4xqydB2QXwTF...` |
| 943 | `deprecate_rent_exemption_threshold` | folds the threshold into the byte rate and rewrites the Rent sysvar | `APwjdDg3GPcX...` |
| 949 | `vote_state_v4` | upgrades the Stake program from a buffer (SIMD-0185) | `Bzx7vGBzg7da...` |
| 971 | `replace_spl_token_with_p_token` | SPL Token loader v2 -> loader v3 (p-token) | `33wyH2pTNjsQ...` |

Ordering trap, now exercised at both ends: `relax_programdata_account_check_migration` lands at
**956**, between 949 and 971, and is passed as a flag to both upgrade paths — so the same code
behaves differently at the two. At 971 it is active, which is why a system-owned prefunded
programdata account is tolerated there and its lamports join the burn.

949 also carries a trap of its own. `vote_state_v4` rewrites mainnet's vote accounts to
`VoteStateV4`, so from 949 onward anything reading vote state with a pre-V4 type silently sees
nothing. At 971 that dropped 751 of 774 voters and paid 1,898 delegations instead of 1.28
million, with an intact-looking bank hash right up to the boundary.

### Verified windows

| window | slots verified | note |
|--------|----------------|------|
| 807 -> 808 | 21,963 | plus a 50,079-slot run byte-exact vs the real snapshot, 8,412,739/8,412,739 accounts |
| 822 -> 823 | 317 | the whole 244-partition reward window |
| 824 -> 825 | 89,953 | largest single window |
| 942 -> 943 | 30,035 | 29,719 before the boundary, 316 after |
| 948 -> 949 | 18,991 | 18,671 before, 320 after |
| 970 -> 971 | 13,397 | 13,081 before, 316 after |

**174,656 of ~74,304,000 slots (0.24%)**, concentrated at the boundaries where behaviour changes
rather than spread uniformly. That is the point of the definition above, not a shortfall against
it, but the raw fraction should stay visible.

## Adding an era

Pick an agave that knows the feature activating just past the previous era's ceiling, then
check it in both directions. Both checks are local to the candidate, so there's no full
re-analysis per era.

- **forward** — the first activated feature whose id the version lacks is its ceiling.
- **backward** — the latest activated feature whose *gate* it deleted is its floor. This is
  the check that was missed at 823.

Measuring the floor is the part that goes wrong. Three methods, two of them broken:

- feature **id** presence — useless; ids are additive, kept for CLI display long after the
  gate is gone, so everything reports floor 0.
- **per-crate** diff over a hand-picked crate list — false positives, because a gate that
  merely moved crates looks deleted.
- union over **all** crates — false negatives, because `agave-feature-set`'s `FEATURE_NAMES`
  mentions every feature that exists.

Sound method: union diff across all agave crates at each version, **excluding the pure
declaration crates** (`agave-feature-set`, `solana-feature-set`) and keeping
`solana-svm-feature-set`, since that struct *is* the live gate set. Gates dropped for
features mainnet never activated are benign — their pubkeys are vanity tombstones
(`LoaderV4WasAbandoned111...`, `1ncomp1ete11...`, `TestFeature1/2...`).

Prefer fewer, wider eras. Each is a permanent commitment: a standing proof plus the retained
snapshot and block cache to re-run it. An era on agave 2.3.13 would handle 823 with no
vendoring at all, but its ceiling is 877 — so buying that costs a whole third era, which is
why 823 stays inside era 1 and is paid for with shims that were already built.
