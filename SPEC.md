# Slate

*Historical Solana account state. Working name, easy to change (alternates: Slotstate, Rewind, Aevum, Statlas).*

## What it is

Slate answers a question the Solana network can't: "what did this account look like at slot N?"

Give it an account and a past slot, it returns the full account as it was at that moment: data bytes, owner, lamports. Give it a program and a past slot, it lists every account that program owned then. It's a self-hostable RPC that serves `getAccountInfo` and `getProgramAccounts` "as of" any past slot since you started running it.

## The gap it fills

Solana has no way to read historical account state. Validators keep only current state. Snapshots are occasional full dumps. The transaction archives (BigTable, Old Faithful) store transactions and blocks, not queryable account state. So "the balance of X at slot N" or "every account this program owned last Tuesday" has no off-the-shelf answer today. Analytics firms rebuild this privately, at cost. There's no open, self-hostable version.

The reason it's missing comes down to one asymmetry: transactions are archived forever, but per-slot account writes are archived nowhere. Slate is the tool that captures those account writes and serves them over time.

The mental split:

- Live validator: what is true right now.
- Cloudbreak: present account state (fast, current).
- Superbank: past transactions and blocks.
- **Slate: past account state.** The missing corner.

## What it does

- `getAccountInfo(pubkey, asOfSlot)` returns the full account at a past slot.
- `getProgramAccounts(programId, asOfSlot)` returns all accounts a program owned at a past slot, with `memcmp` / `dataSize` / `dataSlice` filters and keyset pagination.
- `getBalance`, `getMultipleAccounts` do the same, as of a slot.
- `getCoverage` reports what range it can actually answer for. It's honest about its own limits.
- Encodings: base58, base64, base64+zstd, jsonParsed.

## Scope and non-goals

Slate serves account state over time. It is not:

- A live or low-latency RPC. It lags the tip by finalization. Use a validator or Cloudbreak for "now."
- A transaction or block history service. That's Superbank and BigTable.
- A full-genesis archive out of the box. History starts when you start running it. Reaching further back is a roadmap feature, not a v1 promise.

## Architecture

Three stages, with validation and coverage running across all of them.

```
  Fumarole account stream  +  baseline snapshot
                 |
                 v
            Capture  (finalized-only, buffered, idempotent)
                 |
                 v
            Store    (ClickHouse, append-only, versioned)
                 |
                 v
            Serve    (JSON-RPC, as-of-slot resolved at read time)

  Trust (coverage map + snapshot-diff validation) wraps all three
```

### Store (ClickHouse)

One append-only table, `account_updates`, holding every account version keyed by `(pubkey, slot, write_version)`. Engine is `ReplacingMergeTree`, sorted by `(pubkey, slot, write_version)`, partitioned by epoch. A lean owner-index materialized view backs program scans.

"State at slot N" is just the latest version with `slot <= N`. Nothing gets mutated, there are no intervals to maintain. ClickHouse resolves the as-of-slot at read time with `argMax` / `LIMIT 1 BY`. We store the full account bytes, so a change that only touches data (with no balance change) is captured like any other write.

### Capture

- Live account writes come from Fumarole (durable gRPC, 4-day replay cursor, manual-commit for no-loss ingestion).
- A recent full snapshot seeds the baseline, so accounts that never change are still present. A delta stream alone can't see them.
- Finalized-only commit: unrooted writes sit in an in-memory buffer, and land in ClickHouse only when the slot finalizes. This makes reorgs a non-event (we never undo a committed row) and makes re-ingestion idempotent.
- Bootstrap order: subscribe first, then take the snapshot, then apply writes past the snapshot slot. Same shape as bootstrapping a database replica from a base backup plus its write-ahead log.

### Serve

Lifts Cloudbreak's RPC method bodies (encodings, filters, jsonParsed), swaps the backend from Postgres to the ClickHouse as-of-slot queries, and adds the `asOfSlot` parameter. Every query checks coverage before it answers.

### Trust

- Coverage map: tracks which slots we've fully captured. A query outside it returns "unavailable," never a guess.
- Snapshot-diff validation: a full snapshot at slot S is the network's own canonical state at S. We materialize Slate's state at S and diff it, account by account, against that snapshot. Ground truth, not a fixture. This is how we prove correctness.

## Key decisions and why

- **ClickHouse, not Postgres.** Columnar, append-optimized, built for this write volume and for as-of-slot scans. Superbank runs it at Solana scale. Postgres (Cloudbreak's engine) fights the write firehose.
- **Finalized-only.** We never store speculative state, so reorgs cost nothing and re-ingestion is safe to repeat. It also makes ClickHouse (which dislikes deletes) the right fit.
- **Never fabricate, always disclose.** If we didn't capture something, we say so. A point query checks the span between the account's last known write and the asked slot (the "gap shadow"). A scan checks the whole history up to the asked slot, since a blind spot could hide an account we never recorded.
- **Honesty over availability.** We'd rather return "can't be sure" than a confident wrong answer. That's the bar for audit, forensic, and analytics users.
- **Reuse, don't reinvent.** Cloudbreak gives us the account ingest, the snapshot reader, and the RPC method bodies. Superbank gives us the ClickHouse patterns. Slate is Cloudbreak's brain on Superbank's spine.
- **AGPL-3.0.** Both parents are AGPL, and lifting their code makes Slate AGPL too. Fine for an open project.

## Correctness model, in plain terms

The one real hazard is a blind spot: a stretch where capture was down. We handle it in layers.

- Fumarole's 4-day cursor replays almost any real outage on reconnect, so blind spots are rare. Crashes, deploys, and maintenance all fit inside the window.
- If we're ever blind longer than that, we re-anchor forward from a fresh snapshot. Without that, answers for accounts that didn't change again would silently go stale. The blind window itself gets marked honestly.
- Every query checks coverage first, so a blind spot becomes "unavailable" or "approximate," never a lie.

The finer machinery (snapshot repair inside a gap, reduced-fidelity answers, per-account gap accounting) is designed but deferred. v1 leans on Fumarole's window and a simple decline.

## Two tiers

- **Tier 1, balance history.** Lamport and token balances at a past slot, derivable from a transaction archive (Superbank) with one materialized view. Numbers only, no data bytes or owner. Optional, and a natural contribution back to Superbank.
- **Tier 2, state history.** Full account state: data, owner, lamports. Needs the account-write stream. This is Slate, the flagship, and the priority.

## Roadmap

- Backward backfill: reconstruct history before your start slot by replaying archived transactions through the SVM.
- Federated archive: operators share the per-slot writes they captured, and the union becomes the historical account-write archive that doesn't exist today.
- Time-travel simulation: run a transaction against real account state at a past slot, with solana-svm on top of the store.
- Also: incremental-snapshot repair with reduced-fidelity answers, `asOfTime`, S3 tiering, an optional head cache for freshness, token-specific methods, ClickHouse clustering.

## Tradeoffs we accepted

- No genesis history out of the box. It starts at your bootstrap slot.
- Not fresh to the tip. Finalized only.
- We gave up neither scale nor scope flexibility. ClickHouse with epoch partitions handles both.
