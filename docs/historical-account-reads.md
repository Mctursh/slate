---
number: "0000"
title: Historical account reads
authors: ["Mctursh"]
status: draft
created: 2026-09-05
reference-implementations: ["https://github.com/Mctursh/slate"]
---

# Historical account reads

## Summary

Add an optional `asOfSlot` parameter to the account-reading methods, returning an
account's state as of that slot rather than the latest, and a `fidelity` field on the
response context saying whether the server has the coverage to answer exactly for that
slot.

## Motivation

Nothing in the current read layer answers "what did this account hold at slot N". RPC nodes prune, snapshots land a few times per epoch, and per-slot account writes are discarded once they finalize. Common cases: pool reserves just before a swap, program state at the slot an exploit landed, backtesting against the state a strategy saw, reproducing an old bug report.

The state can be rebuilt by replaying blocks from a snapshot. The hard part is proving it's right, so any implementation also has to say how much it can vouch for. If each implementation expresses coverage differently, clients can't treat them interchangeably. Fixing the shape while there are few implementations is cheaper than reconciling several later.

## Specification

An optional `asOfSlot` (u64) parameter is added to the config object of:

- `getAccountInfo`
- `getBalance`
- `getMultipleAccounts`
- `getProgramAccounts`

When present, the server MUST return the account state produced by the highest write
at or before `asOfSlot`. When absent, behavior is unchanged.

The response context gains a `fidelity` field with two values:

- `exact` — the server holds unbroken per-slot coverage from a verified anchor through
  `asOfSlot`. The value is the state at that slot.
- `uncertain` — the server does not hold unbroken coverage for that range. The value
  may be stale, or absent when the account did exist.

A server MUST NOT return `exact` for a slot whose coverage it cannot substantiate.
Returning `uncertain` is always permitted; silently returning a best guess as `exact`
is the failure this field exists to prevent.

For `getMultipleAccounts` the context carries `fidelities`, one entry per requested
account in request order, because coverage can differ per account.

A `getCoverage` method returns the contiguous slot segments the server can answer
`exact` for, so a client can decide what to ask before asking it.

`getFirstAvailableSlot` is left unchanged. Its existing meaning, the lowest slot the
server holds data for, is the floor of that coverage, but a floor cannot express gaps:
a server covering 100-200 and 500-600 reports 100 either way, which tells a client
nothing about slot 300. `getCoverage` is the precise form, and the reason this proposal
adds a method rather than reinterpreting one.

An implementation that does not support historical reads MUST reject `asOfSlot` with an
error rather than ignoring it and answering for the current slot.

## Return-type impact

- `context` gains `fidelity` (string) on `getAccountInfo`, `getBalance` and
  `getProgramAccounts`.
- `context` gains `fidelities` (array of string) on `getMultipleAccounts`.
- New method `getCoverage`, returning `{ segments: [{ firstSlot, lastSlot }] }`.
- No existing field changes meaning or type.

## Compatibility

Additive. A client that never sends `asOfSlot` sees identical behavior from every
implementation, so Agave, cloudbreak and superbank are unaffected until they choose to
support it.

There is prior art. Alchemy's Solana Account Archive is a closed, hosted implementation
of the same capability: coverage back to July 2025, a `slot` parameter on
`getAccountInfo` alongside `lastUpdateBeforeSlot` and `firstUpdateAfterSlot` cursors,
and error `-32020` when a request falls outside its coverage window. It excludes vote
accounts and the three sysvars rewritten every slot, and implements the historical
parameters on `getAccountInfo` only.

That is an argument for specifying rather than against, and the divergence is already
concrete rather than hypothetical. Two implementations have picked two names for the
same parameter, and two different ways of telling a client the answer is not available:
an error code in one, a response field in the other. Those imply different client code
for the same question. A spec settles which.

The surface matters too. `getAccountInfo` alone leaves out the question cloudbreak
exists to answer, `getProgramAccounts`, which is materially harder historically because
it needs the owner-indexed set as it stood at that slot rather than a single account
lookup. Specifying the parameter across the account-reading methods, rather than one of
them, is what makes it usable for the cases in the motivation.

## Reference implementation

[Slate](https://github.com/Mctursh/slate), AGPL-3.0, ClickHouse-backed. Serves the four methods with `asOfSlot`, plus `getCoverage`.

Backfill checks its replay three independent ways, each catching a different class of error:

1. **Per transaction**: status, fee, lamports and token balances against the block's recorded meta.
2. **Per slot**: bank hash against the hash validators voted on. Every byte of every written account feeds it. The run halts at the first mismatch, so coverage never extends past verified state.
3. **End state**: account-by-account diff against the official snapshot at the end of the range.

Results on mainnet:

- 440,342 consecutive slots vote-verified from epoch 807 to 809, crossing both boundaries.
- Every account a 50,079-slot range touched, byte-exact against mainnet's own snapshot at its end: 8,412,739 / 8,412,739.
- Epoch boundaries 808, 809, 823, 825, 943, 949 and 971 reproduce mainnet's bank hash, including all four in epochs 807 to 978 that rewrite accounts.
- Not done: 20 of the 27 feature activations in that range have no replayed window through them. 4 are networking-only.

## Security considerations

- **Unbounded historical scans.** `getProgramAccounts` with an old `asOfSlot` can touch
  far more state than the current-slot form. The reference implementation paginates with
  an opaque cursor, but only when the caller passes `limit`; it does not cap unpaged
  scans yet. The spec should require servers to bound the result rather than leave it
  implementation-defined.
- **Storage-driven denial of service.** Coverage is chosen by the operator, not the
  caller, and `getCoverage` lets clients avoid queries a server cannot serve, which keeps
  the expensive failure path off the hot path.
- **Data exposure.** None new. Every value returned is public chain state that was
  already committed and already served by a full node at the time. The parameter changes
  when it can be read, not who can read it.
