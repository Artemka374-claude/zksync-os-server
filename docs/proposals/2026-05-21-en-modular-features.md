# EN modular features — implementation breakdown

**Status:** Proposal
**Date:** 2026-05-21
**Author:** @afo

## Motivation

External Nodes (ENs) currently incur high RPC-provider costs (up to ~$300/month
reported by @Emil Luta and @Kostia Savchuk) and run components that most
operators do not need (e.g. the priority tree, which is only needed if a node
can be promoted to main). We want operators to opt out of components they don't
need and to make the EN more resilient to L1 provider issues.

The full problem statement and proposal motivating this work is in the
companion design note. This document breaks the proposal into concrete,
PR-sized deliverables.

## Goals

- Reduce baseline L1 RPC cost for the default EN configuration.
- Make finality, priority-tree, and batch-verification optional features.
- Keep current behavior as the default (all features on) so existing operators
  are unaffected until they opt out.
- Replace cross-checking watchers with a single cheaper consistency check.

## Non-goals

- Reworking the main node's L1 footprint. MN behavior stays as-is.
- Changing the wire format or storage layout.
- Reducing L1 polling frequency as part of the main thread (tracked as a
  follow-up — see PR 12).

## Sequencing strategy

We land the underlying refactors first, then introduce mode flags last. Every
PR in Group A ships independent value to all operators and does not depend on
a feature-flag scaffolding existing yet. Group B introduces the flags and
wires the gating in; by then the components are already shaped to make this
mechanical.

## Dependency graph

```
PR 1 ─┐
PR 2 ─┼─→ PR 3
PR 4 ─┘
PR 5 ──────────────────→ PR 11
PR 6 ──┐
PR 7 ──┼──→ PR 9
PR 8 ──┴──→ PR 9, PR 10, PR 11
PR 12 (independent follow-up)
```

---

## Group A — Foundational improvements

These ship to every operator regardless of mode. No feature flags introduced
in this group.

### PR 1 — Unify finality watchers into `FinalityWatcher`

**What.** Merge `L1CommitWatcher`, `L1ExecuteWatcher`, and
`L1FinalizedExecuteWatcher` into a single `FinalityWatcher`. One event
subscription, one state machine progressing batches through
committed → executed → finalized.

**Why.** All three currently serve one purpose — adjusting finality. Keeping
them separated multiplies subcomponents, log noise, and provider calls without
benefit.

**Scope.** Pure refactor inside `lib/l1_watcher`. No behavior change. Public
re-exports updated; node-bin wiring updated to spawn one watcher instead of
three.

**Risks.** Subtle ordering between the three event streams must be preserved
in the unified state machine. Cover with existing finality integration tests.

**Acceptance.** Integration tests pass unchanged; metrics show the same
finality progression timing within noise.

---

### PR 2 — L1 consistency check inside `L1PersistBatchWatcher`

**What.** When the `ExecuteBatch` event arrives, compare its `batchHash` and
`commitment` against the replayed batch held locally. On mismatch, panic with
a structured diagnostic (batch number, expected vs. observed hash, event
source).

**Why.** This is the safety net that lets us remove `L1TxWatcher`,
`L1UpgradeTxWatcher`, `GatewayMigrationWatcher`, and `InteropWatcher` from EN
in PR 3. It's cheaper because it piggy-backs on an event we already consume
instead of issuing new contract calls.

**Scope.** `lib/l1_watcher/src/persist_batch_watcher.rs`. No new watcher. The
existing finality stall behavior naturally handles provider outages — batches
won't be marked persisted until the event is observed.

**Risks.** A genuine MN bug or replay desync would now panic instead of
silently logging. This is intentional — we prefer a fast, loud failure to
corrupted state.

**Acceptance.** Unit tests covering matching and mismatching event payloads.
An integration test that injects a deliberately wrong replay and asserts the
node panics.

---

### PR 3 — Drop redundant cross-check watchers from EN

**What.** Stop running `L1TxWatcher`, `L1UpgradeTxWatcher`,
`GatewayMigrationWatcher`, and `InteropWatcher` on EN. The watchers themselves
remain in `lib/l1_watcher` — MN continues to depend on them.

**Why.** Their EN role was to cross-check MN-replayed data against L1. PR 2
now does this more cheaply on the `ExecuteBatch` path.

**Scope.** Deletion-only in `node/bin` wiring. No changes to the watcher
crates themselves.

**Depends on.** PR 2.

**Acceptance.** EN starts and runs against a live network without these
watchers. Provider call rate measurably drops (record before/after numbers in
the PR description).

---

### PR 4 — Genesis upgrade tx via MN RPC

**What.** Add a new MN RPC method (proposed name `zks_getGenesisUpgradeTx`)
that returns the genesis upgrade transaction. EN calls it instead of doing
`3 + log2(latest_block)` L1 queries to discover the `GenesisUpgrade` event.

**Why.** Genesis discovery is one of the most call-expensive steps on EN
startup. The MN already knows the answer.

**Scope.** Two-sided:
- Server side: new method on the MN RPC namespace, plumbed from existing
  genesis state.
- Client side: EN startup path consumes the new method; old L1-discovery path
  removed.

**Trust.** A malicious or buggy MN returning the wrong tx will be caught by
PR 2 — subsequent batches will not match.

**Risks.** Backward compatibility for ENs running against an older MN that
lacks the new endpoint. Either gate on protocol/server version, or keep the
L1 fallback for one release.

**Acceptance.** EN startup against a current MN succeeds without the
log-scaling L1 calls. Integration test that spins MN + EN end-to-end through
genesis.

---

### PR 5 — Drop `MultisigCommitter` startup calls from EN

**What.** Remove the following L1 calls from EN startup:
- `MultisigCommitter::try_new` — contract-interface check.
- `multisigCommitter.getSigningThreshold()` — only meaningful to MN.
- `multisigCommitter.getValidators()` — only used today to log a warning if
  the node isn't a validator; not actionable on EN.

**Why.** These have no EN-side semantics. MN owns validator-set correctness.

**Scope.** `lib/batch_verification` — surgical deletion of the calls and any
now-dead code they fed.

**Acceptance.** EN startup makes zero `MultisigCommitter`-related L1 calls.

---

### PR 6 — Refactor `L1State::fetch` into composable futures

**What.** Split the monolithic L1-discovery routine into per-component
futures. Each future returns just the slice of `L1State` it discovered.
Consumers `.await` only the futures they need.

**Why.** Today every consumer pays for every discovery call, even if it only
cares about a subset. After this refactor, PRs 9–11 can naturally gate
discovery alongside the component being gated.

**Scope.** `lib/contract_interface/src/l1_discovery.rs` and call sites.
No observable behavior change yet — current callers `.await` all the futures
and assemble the same `L1State`.

**Risks.** Care to preserve the existing ordering for any discovery step
whose result is consumed by another (split the dependency graph explicitly
rather than relying on call order).

**Acceptance.** All existing startup paths produce the same `L1State`.
Integration tests pass.

---

### PR 7 — Make `SettlementLayerWatcher` and `MigrationFinalizedWatcher` actually run on EN

**What.** Both watchers exist but currently no-op on EN. Wire them so they
maintain SL state correctly on EN.

**Why.** PR 9 (Finality mode) depends on `L1PersistBatchWatcher`, which in
turn needs correct SL interval information. Today the EN-side gap is masked
because `L1PersistBatchWatcher` is the one that ends up driving things; once
modes exist, the dependency becomes load-bearing.

**Scope.** Node-bin wiring for both watchers on EN; possibly minor changes in
the watchers themselves if they assumed MN-only execution.

**Acceptance.** EN SL state advances correctly across a migration in an
integration test.

---

## Group B — Mode flags

This is where the user-facing optionality lands. All flags default to `true`
so existing operators see no behavior change without opting in.

### PR 8 — Introduce EN feature-flag config surface

**What.** New config structure (proposed shape):

```toml
[features]
finality = true
priority_tree = true
batch_verification = true
```

Plumb the flags through to `node/bin` startup. No gating wired in this PR —
the next three do that. Document the tradeoffs in `docs/src/setup/` and
reference this proposal.

**Why.** Landing the config surface as its own PR keeps the gating PRs small
and review-friendly.

**Acceptance.** Config parses; flags reach the startup site; default behavior
unchanged.

---

### PR 9 — Gate Finality mode

**What.** When `features.finality = false`:
- Do not start `FinalityWatcher`, `L1PersistBatchWatcher`,
  `SettlementLayerWatcher`, `MigrationFinalizedWatcher`.
- Do not await their L1-discovery futures (from PR 6).
- Return a clear error (or 404) from these RPC methods:
  - `zks_getL2ToL1LogProof`
  - `zks_getProof`
  - `unstable_getBatchByBlockNumber`
  - `unstable_getBatchByNumber`

**Why.** Finality is the highest-volume L1 consumer (~10–20k calls/hour and
up to ~45 startup calls). Operators who only need to serve recent state can
skip it entirely.

**Tradeoffs.** Data is no longer L1-backed; the listed RPC methods become
unavailable. Surface this clearly in docs and in the error message returned
by the disabled RPCs.

**Depends on.** PR 6, PR 7, PR 8.

**Acceptance.** EN with `finality = false` starts, serves remaining RPCs, and
makes zero finality-related L1 calls during a fixed observation window.

---

### PR 10 — Gate Priority tree mode

**What.** When `features.priority_tree = false`: do not start
`PriorityTreeManager`. If `features.finality = true`, `FinalityWatcher` still
runs; only the priority-tree-specific subscription is skipped.

**Why.** Priority tree is only needed if the node can be promoted to main.
Consumes ~7k calls/hour and up to ~45 startup calls.

**Depends on.** PR 6, PR 8.

**Acceptance.** EN with `priority_tree = false` (and `finality = true`)
starts, finality progresses, and no priority-tree-related L1 calls are made.

---

### PR 11 — Normalize Batch verification gating

**What.** Batch verification is already optional today via a separate config
path. Fold it into the new `features` surface and confirm zero L1 calls when
off (PR 5 should already have removed them).

**Depends on.** PR 5, PR 8.

**Acceptance.** Single config surface for all three modes. Documentation
updated.

---

## Group C — Follow-up (independent)

### PR 12 — Configurable L1 poll interval

**What.** Surface the watcher poll interval as config. Consider raising the
default above 1s, which is the current rate per watcher.

**Why.** Even after the cuts above, polling cadence is a multiplier on every
remaining call. Operators on metered providers benefit from a gentler
default.

**Acceptance.** Config parses; documented; default chosen with a brief
analysis in the PR description.

---

## Risks and open questions

- **PR 2 panic policy.** Panicking on divergence is the safest default but
  may be considered too aggressive in production. We could gate behind a
  config (panic vs. error-and-stall) if operators push back.
- **PR 4 MN compatibility.** If we expect ENs to run against older MN
  versions, the L1 fallback may need to stay for one release.
- **Default for `features.*`.** All three default to `true` here. We may
  revisit once we have data on what fraction of operators actually need each.

## Out of scope

- Reducing MN L1 footprint.
- Changing how MN performs validator-set or threshold checks.
- Wire-format or storage changes.
