# Slate v1 Tasks

Start to finish for v1. v1 proves one sentence: "full account state at any past slot, provably correct," on real data.

Build order matters. Milestone 1 is the risk-killer and needs no gRPC access, so it can start first and offline. Legend: `[lift]` = adapt from Cloudbreak/Superbank, `[you]` = you write it, `[claude]` = I set up or review.

---

## Milestone 0: Scaffold

Goal: an empty project that builds, lints, and runs a local ClickHouse.

- [ ] Create the repo and Cargo workspace
  - [ ] Crates: `slate-store`, `slate-ingest`, `slate-rpc`, `slate-common` (config, types, db helpers)
  - [ ] AGPL-3.0 LICENSE, README stub pointing at SPEC.md
  - [ ] rust-toolchain, rustfmt, clippy config
- [ ] Local dev environment
  - [ ] docker-compose with single-node ClickHouse (pin a recent version, 26.x for reverse-key support if needed later)
  - [ ] Makefile or justfile: `up`, `ddl`, `run-*`, `test`
- [ ] Config skeleton (TOML)
  - [ ] ClickHouse connection
  - [ ] Capture scope (owner include/exclude filter)
  - [ ] Source config (Fumarole endpoint, token) and snapshot source
- [ ] CI: fmt, clippy -D warnings, test

Acceptance: `docker compose up` starts ClickHouse, workspace builds, CI is green.

---

## Milestone 1: Store + snapshot-diff (RISK-KILLER, offline, no gRPC)

Goal: prove the data model answers as-of-slot correctly, before writing any streaming code.

- [ ] Schema `[claude sets, you apply]`
  - [ ] `account_updates` table (ReplacingMergeTree, ORDER BY (pubkey, slot, write_version), PARTITION BY intDiv(slot, 432000), codecs)
  - [ ] `account_updates_by_owner` materialized view (lean: owner, pubkey, slot, write_version, lamports, data_len)
  - [ ] DDL apply script / migration
- [ ] Snapshot reader `[lift]`
  - [ ] Pull Cloudbreak `crates/snapshot` AccountsFile / append-vec reader into `slate-store` (or a helper)
  - [ ] Download two real mainnet snapshots at slots S1 and S2 (S2 > S1)
- [ ] Baseline loader `[you]`
  - [ ] Read snapshot at S1, bulk insert every in-scope account as a version at S1
- [ ] Write feeder `[you]`
  - [ ] Derive the account writes between S1 and S2 (from the S2 snapshot vs S1, or a captured sample) and insert them as versioned rows
- [ ] As-of-slot queries `[you]`
  - [ ] Point lookup: latest version WHERE pubkey = X AND slot <= N
  - [ ] Program scan: candidates from owner MV, argMax per pubkey, confirm owner = P and lamports > 0
- [ ] Snapshot-diff harness `[you, claude reviews]`
  - [ ] Load an independent snapshot at S2
  - [ ] Query Slate's state as of S2 for all in-scope accounts
  - [ ] Diff account by account (lamports, owner, data, executable, rent_epoch); report mismatches

Acceptance: snapshot-diff at S2 is green. The core thesis is proven.

---

## Milestone 2: Capture (needs Fumarole / gRPC access)

Goal: real data flowing in, finalized-only, idempotent, resumable.

- [ ] Stream consumer `[lift]`
  - [ ] Fumarole subscription (durable cursor, manual-commit mode)
  - [ ] Account + slot/block status decode
  - [ ] Scope filter from Cloudbreak `AccountSelectorConfig`
- [ ] Finalized-only commit `[lift + you]`
  - [ ] In-memory buffer of unrooted writes, keyed by slot (collapse intra-slot to max write_version per pubkey)
  - [ ] Flush on finalize, in slot order
  - [ ] Batched, idempotent INSERT into ClickHouse (Superbank inserter pattern, guarantee the trailing flush)
- [ ] Bootstrap seam `[you]`
  - [ ] Subscribe first, then load baseline snapshot at S_snap, apply only writes with slot > S_snap
  - [ ] Advance cursor only after durable ClickHouse write (no-loss)
- [ ] Watermark + coverage (simple) `[you]`
  - [ ] Persist highest contiguous finalized slot
  - [ ] Coverage = [S0, watermark]; a true gap beyond Fumarole replay marks a missing window
  - [ ] Resume from cursor on restart

Acceptance: live capture runs; Slate's recent as-of-slot answers match a live RPC (spot differential).

---

## Milestone 3: Serve (lift Cloudbreak api)

Goal: the RPC surface, with asOfSlot and honest coverage.

- [ ] RPC server `[lift]`
  - [ ] Pull Cloudbreak `crates/api`: JSON-RPC + HTTP, encodings (base58, base64, base64+zstd, jsonParsed), filters (memcmp, dataSize, dataSlice)
  - [ ] Repoint the backend from Postgres to the ClickHouse as-of-slot queries
- [ ] asOfSlot `[you]`
  - [ ] Parse the param, thread it into the slot <= N bound
  - [ ] Omitted = latest committed finalized slot
- [ ] Coverage gate + shadow check `[you, claude reviews]`
  - [ ] Point: verify (lower, N] is covered (lower = serving slot, else S0)
  - [ ] Scan: verify [S0, N] has no missing window
  - [ ] Outside coverage returns an error, never a value; response carries context.slot
- [ ] Methods `[lift + you]`
  - [ ] getAccountInfo, getBalance, getMultipleAccounts, getProgramAccounts
  - [ ] Keyset pagination for getProgramAccounts (WHERE pubkey > cursor ORDER BY pubkey LIMIT n)
  - [ ] getCoverage / getFirstAvailableSlot

Acceptance: query any asOfSlot since bootstrap; honest decline outside coverage; pagination works on a large program.

---

## Milestone 4: Trust, harden, ship

Goal: prove it stays correct, make it runnable by someone else.

- [ ] Validation `[you, claude reviews]`
  - [ ] Snapshot-diff wired into CI (periodic, against a real snapshot slot)
  - [ ] Live-RPC differential harness (adapt Cloudbreak integration_tests, add asOfSlot)
- [ ] Observability `[you]`
  - [ ] Metrics: ingestion lag, coverage percent, open-gap count, oldest unrepaired gap
  - [ ] /health endpoint
- [ ] Docs and config `[you, claude reviews]`
  - [ ] README (quick start), self-hosting doc, example config
  - [ ] Config validation with clear errors
- [ ] Storage sizing `[you]`
  - [ ] Run capture on the real scope for ~1 hour, read exact compressed sizes from system.parts, extrapolate
- [ ] Release `[you]`
  - [ ] Tag v1, write the demo (state of an account as of last week, proven by snapshot-diff; program accounts at that slot, paginated)

Acceptance: the v1 definition of done is met. Point it at Fumarole plus a program filter, it bootstraps, streams, answers any asOfSlot since bootstrap, correctness proven by snapshot-diff, honest coverage. Single node.

---

## Explicitly out of v1 (roadmap)

Incremental-snapshot repair, reduced-fidelity answers, full coverage state machine, backward backfill (SVM replay), federated archive, time-travel simulation, asOfTime, memcmp pushdown projection, content-addressed dedup, TTL to S3 tiering, head cache, token-specific methods, ClickHouse clustering.
