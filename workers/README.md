# Era workers

No single agave build replays all of mainnet history. A version doesn't know features that activate after it was cut, and it has already deleted gates that earlier epochs need. So history is split into **eras**, each replayed by a worker linking an agave from that period.

Each worker is its own Cargo workspace with its own `Cargo.lock` and `rust-toolchain.toml`, so adding or fixing one era can't change another era's build. Eras seed from their own snapshot, so they don't depend on each other.

```
slate/
  slate-hash/  slate-format/  slate-store/  slate-common/   shared, byte API only
  workers/
    agave-3.1.14/        own workspace, lockfile, toolchain
      slate-replay/  slate-backfill/  fixtures/  vendor/
```

Shared crates take bytes, not solana types, because `Pubkey` at 3.x and 4.x are different types. A change to a shared crate re-proves every era.

## Eras

Checked 2026-09-20 against mainnet (tip epoch 1038).

| Worker | agave | Floor | Ceiling | Status |
| --- | --- | --- | --- | --- |
| `agave-3.1.14` | 3.1.14 | 823, shimmed down to 807 | 978 | era 1 |
| not built | 4.2.1 | 953 | past tip | era 2, from 979 |

- **Ceiling**: first mainnet feature the version doesn't know. 3.1.14 lacks `create_account_allow_prefund` (979).
- **Floor**: latest activation whose gate the version deleted. 3.1.14 dropped `migrate_stake_program_to_core_bpf` (823); 4.2.1 dropped `relax_intrabatch_account_locks` (953).
- 3.1.14 reaches below 823 with two shims: the vendored native Stake program (`vendor/solana-stake-program`) and the SIMD-0162 executable check. The 823 migration itself is in `slate-replay/src/compat/core_bpf.rs`.

## Era 1 (`agave-3.1.14`, epochs 807 to 978)

27 features activate in range:

    807 808 809 811 818 821 822 823 825 836 841 878 880 884 890
    933 935 936 940 942 943 946 949 950 953 956 971

821, 880, 935 and 946 are networking-only and can't change account state.

Four rewrite accounts at the boundary. All four reproduce mainnet's boundary bank hash:

| Epoch | Feature | Writes | Boundary hash |
| --- | --- | --- | --- |
| 823 | `migrate_stake_program_to_core_bpf` | Stake native to Core BPF | `4xqydB2QXwTF...` |
| 943 | `deprecate_rent_exemption_threshold` | Rent sysvar rewrite (SIMD-0194) | `APwjdDg3GPcX...` |
| 949 | `vote_state_v4` | Stake upgraded from buffer (SIMD-0185) | `Bzx7vGBzg7da...` |
| 971 | `replace_spl_token_with_p_token` | SPL Token to p-token | `33wyH2pTNjsQ...` |

`relax_programdata_account_check_migration` activates at 956, between 949 and 971, and changes the upgrade path at 971. From 949, vote accounts are `VoteStateV4`; reading them with an older type silently drops voters.

### Verified windows

Every slot's bank hash matched a consensus vote.

| Window | Slots | Note |
| --- | --- | --- |
| 807 to 809 | 440,342 | one contiguous range crossing both the 808 and 809 boundaries |
| inside 808 | 50,079 | end state byte-exact vs mainnet's snapshot, 8,412,739 / 8,412,739 accounts |
| 822 to 823 | 27,779 | 27,462 before the boundary, 317 across it through all 244 reward partitions |
| 824 to 825 | 89,953 | |
| 942 to 943 | 30,035 | 316 past the boundary |
| 948 to 949 | 18,991 | 320 past the boundary |
| 970 to 971 | 13,397 | 316 past the boundary |

Crossed so far: 808, 809, 823, 825, 943, 949, 971. That leaves 20 of 27 uncrossed, 16 of them execution-relevant. Era 1 is about 74 million slots, so coverage targets the boundaries where behaviour changes rather than the whole range.

### Fixtures

`fixtures/slots/` holds recorded mainnet slots that once diverged, committed and replayed by `cargo test`. `fixtures/boundary/` pins the six boundary blocks published as the `fixtures-v1` release, replayed with `--features boundary-fixtures`. Each `expected.txt` lists what its fixtures cover.

## Adding an era

Pick an agave that knows the feature activating just past the previous era's ceiling, then measure both ends:

- **Ceiling**: the first activated feature whose id the version lacks.
- **Floor**: the latest activated feature whose gate the version deleted. Diff gate usage across all agave crates, excluding the declaration-only `agave-feature-set` and `solana-feature-set` but keeping `solana-svm-feature-set`. Checking feature ids instead reports floor 0, because ids outlive their gates.

Prefer fewer, wider eras. Each one is a standing proof plus a retained snapshot and block cache.
