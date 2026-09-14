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

The read layer answers "now".

Ledger history is covered: superbank ingests it into ClickHouse and serves it over
Solana-compatible JSON-RPC. Current account reads are covered: cloudbreak maintains a
Postgres-backed segment of account state by program owner and serves
getProgramAccounts-style reads from it. Nothing in this stack answers "what did this
account hold at slot N".

The question is routine. Reconstructing a pool's reserves just before a swap. Auditing
a program's state at the slot an exploit landed. Backtesting against the state a
strategy actually saw rather than the state that exists now. Reproducing a bug report
from three days ago. Each of these is currently solved privately, approximately, or
not at all.

Three things keep it broken, and they compound:

- RPC prunes. An archival node keeps blocks, not per-slot account state.
- Snapshots are periodic and large. They give you state at a few slots per epoch, not
  the slot you care about.
- The per-slot account writes that would answer it are discarded once they finalize.

So the state has to be rebuilt by replaying blocks forward from a snapshot. That part
is well understood. The hard part is proving the result is right, because a replay
that is subtly wrong looks exactly like one that is correct unless it is checked
against something the implementation did not produce itself. Without that check a
historical answer is a guess with a slot number attached, and a client has no way to
tell the difference.

Standardising the parameter matters more than it first appears. Any implementation
that reconstructs history has to express *coverage*: how far back it reaches, whether
a range is complete, whether a given slot is trustworthy. If each one expresses that
differently, clients cannot treat them interchangeably, which is the point of having a
shared spec. Fixing the shape while there are few implementations is cheaper than reconciling
several later.

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
- New method `getCoverage`, returning an array of `{ first_slot, last_slot }`.
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

[Slate](https://github.com/Mctursh/slate). AGPL-3.0, self-hostable, ClickHouse-backed.
Serves the four methods with `asOfSlot`, plus `getCoverage`, and tags every response.

Correctness is checked three independent ways, because a replay cannot be trusted to
grade itself, and each check sees a different class of error:

1. **Per transaction, against the block's own record.** Every replayed transaction's
   status, fee, lamport balances and token amounts are compared to the meta the block
   recorded. This is the narrowest check and gives the most precise diagnostics, but it
   is blind to account data the block does not report.
2. **Per slot, against consensus.** Each replayed slot's bank hash is compared to the
   hash validators voted on, parsed out of the vote transactions in later blocks. This
   covers the data the per-transaction check cannot see, since every byte of every
   written account feeds the hash. The run halts at the first slot it cannot reproduce,
   so coverage never extends past verified state.
3. **End state, against an artifact it did not produce.** At the end of a range the
   reconstructed state is diffed against the official Solana snapshot at that slot,
   account by account, comparing lamports, owner, executable flag and data. A hash
   mismatch says something is wrong; this says which account.

Most recent run: a full snapshot-to-snapshot mainnet window, 50,079 slots, and the end
state byte-exact against the snapshot at the end of it across 8,412,739 accounts with
zero mismatches. Slots are vote-verified except one per replayed chunk, whose
confirming votes fall outside the window being replayed and so are simply unavailable
rather than mismatched.

Two consensus-breaking bugs were found this way, each by a different check, which is
the argument for keeping all three:

- **Incinerator burn**, caught by the per-slot hash check. Lamports sent to the
  incinerator are destroyed when the bank freezes. The replay credited the account and
  left them there. This is invisible for almost every slot, because a zero-lamport
  account contributes nothing to the accounts lattice, so a missing burn changes nothing
  until a slot actually burns. It surfaced once in 29,538 slots.
- **Program delay visibility**, caught by the per-transaction check. A program deployed
  or upgraded in a slot is not invokable for the remainder of that slot. A mid-block
  Raydium upgrade was followed by seven transactions that failed on chain and succeeded
  in the replay, because the upgraded program was made available immediately.

Current scope, stated plainly. The largest verified window is 50,079 slots inside epoch
808. Epoch boundaries are replayed as well, verified at the 807 → 808 crossing: pending
features activate, the inflation pool and vote commission match mainnet to the lamport,
and all 228 stake-reward partitions pay out on the blocks that carried them.

Reaching further back needs a different agave than reaching forward does, because a given
version doesn't know features that activate after it was cut and has already deleted code
earlier epochs need. The range is therefore sliced into eras, one worker per agave version,
each pinned to its own lockfile and toolchain so a later era can't disturb an earlier one's
proof. Two eras cover epoch 807 to the present.

What is not done: four epochs in 807–978 rewrite accounts at their boundary and have no
implementation (823 Stake to core BPF, 943 Rent sysvar, 949 vote state v4, 971 SPL Token to
p-token), and the SVM's program-runtime environment is still built once from a range's
first slot rather than following a mid-range activation. Verification is also honest about
its own limits: an era spans roughly 74 million slots, replaying all of them is not on the
table, so coverage is built at the points where behaviour actually changes rather than
claimed across the whole range. Extending this is engineering, not open research, but it is
not done.

## Security considerations

- **Unbounded historical scans.** `getProgramAccounts` with an old `asOfSlot` can touch
  far more state than the current-slot form. The reference implementation paginates with
  an opaque cursor and a server-enforced limit. The spec should require servers to bound
  the result rather than leave it implementation-defined.
- **Storage-driven denial of service.** Coverage is chosen by the operator, not the
  caller, and `getCoverage` lets clients avoid queries a server cannot serve, which keeps
  the expensive failure path off the hot path.
- **Data exposure.** None new. Every value returned is public chain state that was
  already committed and already served by a full node at the time. The parameter changes
  when it can be read, not who can read it.
