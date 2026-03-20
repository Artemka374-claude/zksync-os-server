# V31 Settlement Layer Chain ID Bootstrap

## Background

Protocol version 31 introduces `sl_chain_id` into the batch public input hash (see
`batch_info.rs::public_input_hash`). The L2 value of `currentSettlementLayerChainId` in
`SystemContext` (address `0x800b`) is what the prover reads. On any chain that starts at or
upgrades to v31, this L2 value must be initialised to match the actual settlement layer chain ID
before the first v31 batch is proved.

Prior to v31 the value is never set, so it is `0`. The gateway migration watcher (spawned when
`current_protocol_version >= v31`) handles subsequent changes via `MigrateToGateway` /
`MigrateFromGateway` events. But there is no mechanism to perform the initial population.

## Goal

Seed `currentSettlementLayerChainId` with the correct settlement layer chain ID before the first
v31 batch is proved, in both cases where the value starts at `0`:

1. **Genesis** (`starting_block == 1`, protocol version >= v31) — the value is provably `0`.
2. **In-run upgrade from v30 to v31** — the value is provably `0` because gateway migrations
   only fire on v31+, so no prior block could have set it.

## Scope

**In scope**:
- Genesis bootstrap (`starting_block == 1`, protocol version >= v31).
- In-run upgrade bootstrap: when `BlockContextProvider` builds a block containing a v30→v31
  upgrade transaction, it chains a `SetSLChainId` bootstrap transaction immediately after the
  upgrade transaction in the same block.

**Not in scope**: chains that were already at v31 before this code ships, or any case where
`currentSettlementLayerChainId` is already non-zero. The value is provably `0` at any v30→v31
boundary crossing (see above), so no runtime L2 read is needed before injecting.

## Design

### `sl_chain_id` value

`node_startup_state.l1_state.sl_chain_id` is populated by `sl_provider.get_chain_id()` in
`L1State::fetch` (`lib/contract_interface/src/l1_discovery.rs`). When `gateway_rpc_url` is not
configured, `sl_provider` equals `l1_provider`, so `sl_chain_id` equals `l1_chain_id`. This is
always the chain ID of the current settlement layer at startup — the correct value to write into
`SystemContext`.

`L1State::fetch` also calls `validate_chain_ids`, which confirms that `sl_chain_id` is
whitelisted on the L1 Bridgehub. A chain ID of 0 cannot be whitelisted, so `sl_chain_id` is
always non-zero when startup proceeds past that validation.

### Dynamic construction in `BlockContextProvider`

Rather than injecting the bootstrap transaction through the `SlChainIdSubpool` (which would
require either external coordination from `L1UpgradeTxWatcher` or genesis-time injection in
`node/bin`), the transaction is constructed inline in `BlockContextProvider::prepare_command`
when building a `Produce` block.

`BlockContextProvider` already tracks `self.protocol_version` and reads `upgrade_metadata`
from the pool stream on each block. When `upgrade_metadata` indicates a version crossing from
below v31 to v31+, `prepare_command` creates a `SetSLChainId(sl_chain_id, u64::MAX)` envelope
and chains it immediately after the upgrade transaction stream:

```rust
// Inside the upgrade branch of prepare_command:
if upgrade_metadata.protocol_version >= ProtocolSemanticVersion::new(0, 31, 0)
    && self.protocol_version < ProtocolSemanticVersion::new(0, 31, 0)
{
    let bootstrap: ZkTransaction =
        SystemTxEnvelope::set_sl_chain_id(self.sl_chain_id, u64::MAX).into();
    best_txs.stream = MarkingTxStream::unmarkable(
        old_stream.chain(futures::stream::once(std::future::ready(bootstrap)))
    );
}
```

`BlockContextProvider` gains one new field: `sl_chain_id: u64`, passed in from `node/bin` at
startup.

This approach has no timing dependency: both the upgrade transaction and the bootstrap
transaction are present in the stream before the block executor reads a single item. It also
requires no changes to `L1UpgradeTxWatcher`, `EthCallHandler`, or genesis startup code.

### Sentinel migration number: `u64::MAX`

`SetSLChainId` carries a `migration_number` salt. This value propagates through the system as
follows:

1. `SlChainIdSubpool::on_canonical_state_change` returns `Option<u64>` (the last migration
   number seen) to the pool.
2. The pool propagates it as `PoolOutcome::last_migration_number`.
3. `BlockContextProvider::on_canonical_state_change` reads `outcome.last_migration_number` and,
   if `Some`, advances `next_migration_number`.
4. `next_migration_number` becomes `starting_migration_number` in the WAL record for subsequent
   blocks.
5. On restart, `starting_migration_number` from the first replay record becomes the
   `current_migration_number` passed to `GatewayMigrationWatcher`, which uses it to
   binary-search for its starting L1 block via
   `IChainAssetHandler.migrationNumber(chainId) >= current_migration_number`.

The bootstrap transaction must not disturb this tracking. We designate `u64::MAX` as a sentinel
meaning "bootstrap, not a real gateway migration". No real migration will ever reach this number.

The bootstrap transaction is constructed dynamically in `BlockContextProvider` and **never
inserted into `SlChainIdSubpool`**. When `SlChainIdSubpool::on_canonical_state_change` sees a
sentinel tx in the replay record it must therefore skip `pop_wait` (which would otherwise
deadlock on an empty deque) and skip migration tracking:

```rust
for tx in txs {
    if let SystemTxType::SetSLChainId(migration_number) = *tx.system_subtype() {
        // u64::MAX is the bootstrap sentinel — dynamically constructed, never queued here.
        // Calling pop_wait would deadlock; propagating u64::MAX would cause
        // u64::MAX + 1 overflow in BlockContextProvider.
        if migration_number == u64::MAX {
            continue;
        }
        let pending_tx = self.pop_wait().await;
        assert_eq!(tx, &pending_tx);
        last_migration_number = Some(migration_number);
    }
}
```

`BlockContextProvider` already handles `None` via an `if let Some(...)` guard, so no changes
are needed there. The sentinel guard is **load-bearing**: if it were absent, `pop_wait` would
deadlock and, even if somehow bypassed, `u64::MAX + 1` would panic in debug builds and silently
wrap to `0` in release — resetting `next_migration_number` incorrectly.

### Sealing after the upgrade transaction

`execute_block_in_vm` currently breaks the execution loop immediately after a successful upgrade
transaction (`break SealReason::UpgradeTx`). To allow the chained bootstrap transaction to
execute in the same block, the loop must continue after the upgrade transaction rather than
sealing immediately.

The change introduces a boolean flag `upgrade_tx_executed`. After the upgrade transaction
succeeds:

- The flag is set to `true` instead of breaking.
- The execution loop continues and reads the next transaction from the stream.
- If the next transaction is `SetSLChainId`, the existing `SealReason::SLChainIdUpdateTx`
  logic fires after it executes, sealing the block normally.
- If the stream is exhausted with no further transaction (upgrade-only block, no bootstrap),
  the `TxStreamExhausted` branch is changed to break with `SealReason::UpgradeTx` when
  `upgrade_tx_executed` is set — rather than returning an error.

The seal-reason validation at the end of `execute_block_in_vm` already accepts
`SealReason::UpgradeTx` and `SealReason::SLChainIdUpdateTx` under `SealPolicy::Decide`; only
`SealReason::TxStreamExhausted` is an error under that policy. The flag-guarded early exit
preserves this contract.

Under `SealPolicy::UntilExhausted { allowed_to_finish_early: false }` (replay), the existing
behaviour is unchanged: the upgrade-tx break is already suppressed for that policy, and the
stream is run to exhaustion regardless.

### Concurrency with `GatewayMigrationWatcher`

#### Genesis case

`GatewayMigrationWatcher` is spawned at startup **before** block 1 is executed — not after
genesis completes. It starts polling L1 immediately. If a real `MigrateToGateway` event arrives
on L1 during the genesis block's execution window, the watcher calls
`sl_chain_id_subpool.insert()`, pushing a real migration tx onto `pending_txs` while the
bootstrap tx is being executed in block 1 via the chained stream.

`SlChainIdTransactionsStream` is intentionally a single-item stream: it serves one transaction
then closes. The `pending_txs` deque receives the real migration tx via `insert`. After block 1
seals, `BlockContextProvider` calls `best_transactions_stream` again. The biased select finds
the `sl_chain_id_stream` has a pending tx and returns it for block 2. No migration event is
lost.

Additionally, because `starting_migration_number` stays at `0`, the watcher re-scans from L1
block 0 on any restart, ensuring any real migration event is also picked up from L1 history.

#### In-run upgrade case

`GatewayMigrationWatcher` is only spawned when `current_protocol_version >= v31` at startup.
When the upgrade triggers at runtime the version was < v31 at startup, so the watcher is **not
running**. There is no concurrent `insert()` from the migration watcher during block N.

Any real gateway migration that fires on L1 between the in-run upgrade and the next restart will
not be tracked in-memory. After the restart `GatewayMigrationWatcher` starts with
`current_migration_number = 0` (because `starting_migration_number` remains `0` thanks to the
sentinel) and re-scans from L1 block 0, picking up any migration events from history. No
migration event is permanently lost.

#### Restart mid-upgrade (node killed after block N is written to WAL but before `on_canonical_state_change` runs)

On the next startup the WAL replays block N via `BlockCommand::Replay`. The replay stream is
built from `record.transactions`, which includes both the upgrade tx and the bootstrap tx (they
were pushed to `executed_txs` and persisted in the `ReplayRecord`). `execute_block_in_vm` runs
both under `SealPolicy::UntilExhausted { allowed_to_finish_early: false }`, exhausting the
stream normally. `SlChainIdSubpool::on_canonical_state_change` then sees the sentinel and skips
it (Change 1). No double-injection or state inconsistency results.

### What is NOT changed

- `L1UpgradeTxWatcher` — no changes. The bootstrap is handled entirely in `BlockContextProvider`.
- `GatewayMigrationWatcher` — no changes.
- `BlockContextProvider::on_canonical_state_change` — no changes; the existing `if let Some(...)`
  guard handles `None` from `SlChainIdSubpool::on_canonical_state_change`. This is correct only
  because Change 1 ensures the sentinel returns `None` rather than `Some(u64::MAX)` — it is a
  consequence of Change 1, not an independent invariant.
- `SystemTxEnvelope::set_sl_chain_id` — reused as-is with the sentinel salt.
- `Pool` — no changes to how `sl_chain_id_subpool` is consumed by the pool; dispatch on
  `SetSLChainId` is unchanged.
- `SlChainIdTransactionsStream` — single-item stream semantics are intentionally preserved for
  real gateway migration txs.
- `node/bin/src/lib.rs` genesis startup — only a new `sl_chain_id` argument to
  `BlockContextProvider::new` is needed; no injection into `SlChainIdSubpool`.

## Changes

### 1. `lib/mempool/src/subpools/sl_chain_id.rs` — handle sentinel in `on_canonical_state_change`

The sentinel tx appears in the replay record but was never inserted into this subpool. The
current loop calls `pop_wait()` unconditionally for every tx before checking the migration
number. The sentinel guard must therefore appear **before** `pop_wait`, otherwise it would
deadlock on an empty deque. The minimal change to the existing loop:

```rust
// Current code (before change):
//   for tx in txs {
//       let pending_tx = self.pop_wait().await;
//       assert_eq!(tx, &pending_tx);
//       if let SystemTxType::SetSLChainId(migration_number) = *tx.system_subtype() {
//           last_migration_number = Some(migration_number);
//       }
//   }

for tx in txs {
    // u64::MAX is the bootstrap sentinel — dynamically constructed by BlockContextProvider,
    // never queued in this subpool. Calling pop_wait would deadlock on an empty deque;
    // propagating u64::MAX would cause u64::MAX + 1 overflow in BlockContextProvider.
    if matches!(*tx.system_subtype(), SystemTxType::SetSLChainId(u64::MAX)) {
        continue;
    }
    let pending_tx = self.pop_wait().await;
    assert_eq!(tx, &pending_tx);
    if let SystemTxType::SetSLChainId(migration_number) = *tx.system_subtype() {
        last_migration_number = Some(migration_number);
    }
}
```

### 2. `lib/sequencer/src/execution/block_context_provider.rs` — inject bootstrap in `prepare_command`

Add `sl_chain_id: u64` field. In the `BlockCommand::Produce` branch of `prepare_command`,
after reading `upgrade_metadata`, detect the v30→v31 crossing and chain the bootstrap tx:

```rust
// Collect what we need without holding a borrow on best_txs.
let upgrade_info = best_txs.upgrade_metadata.as_ref().and_then(|m| {
    (m.protocol_version > self.protocol_version).then(|| {
        let inject_bootstrap =
            m.protocol_version >= ProtocolSemanticVersion::new(0, 31, 0)
            && self.protocol_version < ProtocolSemanticVersion::new(0, 31, 0);
        (m.protocol_version.clone(), m.force_preimages.clone(), inject_bootstrap)
    })
});

let force_preimages = if let Some((new_version, preimages, inject_bootstrap)) = upgrade_info {
    // ... existing timestamp check ...
    if inject_bootstrap {
        let bootstrap: ZkTransaction =
            SystemTxEnvelope::set_sl_chain_id(self.sl_chain_id, u64::MAX).into();
        // `std::mem::replace` extracts the old stream without a borrow conflict.
        // `.stream` is the public `BoxStream` field inside `MarkingTxStream` — accessed
        // directly because `MarkingTxStream` does not implement `Stream` itself, so `.chain`
        // must be called on the inner `BoxStream`.
        let old_stream = std::mem::replace(
            &mut best_txs.stream,
            MarkingTxStream::unmarkable(futures::stream::empty()),
        );
        best_txs.stream = MarkingTxStream::unmarkable(
            old_stream.stream.chain(futures::stream::once(std::future::ready(bootstrap)))
        );
    }
    self.protocol_version = new_version;
    preimages
} else {
    Vec::new()
};
```

### 3. `lib/sequencer/src/execution/execute_block_in_vm.rs` — allow one more tx after upgrade

Introduce `let mut upgrade_tx_executed = false;` before the execution loop.

Replace the current `break SealReason::UpgradeTx` (under `Decide` and
`UntilExhausted { allowed_to_finish_early: true }`) with `upgrade_tx_executed = true;`.

In the stream-exhaustion branch (`let Some(tx) = maybe_tx else { ... }`), add:

```rust
let Some(tx) = maybe_tx else {
    if upgrade_tx_executed {
        break SealReason::UpgradeTx;
    }
    tracing::debug!(...);
    break SealReason::TxStreamExhausted;
};
```

No change to the existing `SealReason::SLChainIdUpdateTx` logic — it fires naturally when the
bootstrap tx executes as the next item after the upgrade tx. The bootstrap tx is pushed to
`executed_txs` on success (same path as every other tx), so it is persisted in the `ReplayRecord`
and present in the WAL. This is what makes replay and restart correct (see Concurrency section).

**Rebuild path (`UntilExhausted { allowed_to_finish_early: true }`):** The same match arm that
currently breaks with `SealReason::UpgradeTx` is changed to set the flag instead. During a
rebuild the transaction list already contains both the upgrade tx and the bootstrap tx (read from
the WAL), so the loop exhausts them both and the `SLChainIdUpdateTx` seal fires naturally.
Changing the break to a flag is benign for this path.

**Replay path (`UntilExhausted { allowed_to_finish_early: false }`):** This arm is unchanged.
In replay the stream is always run to exhaustion regardless; the upgrade-tx break is already
suppressed for this policy. Both transactions are present in the stream from the WAL record and
execute in order.

### 4. `node/bin/src/lib.rs` — pass `sl_chain_id` to `BlockContextProvider`

Add `node_startup_state.l1_state.sl_chain_id` as a new argument to
`BlockContextProvider::new`. No other changes in this file.

## Data flow

### Genesis

```
Node starts, starting_block == 1, protocol_version >= v31
  │
  └─ upgrade_subpool ← genesis upgrade tx (existing)
     (SlChainIdSubpool: empty — no injection)

Block 1 — BlockContextProvider::prepare_command (Produce)
  └─ reads upgrade_metadata: new_version >= v31, self.protocol_version < v31
  └─ inject_bootstrap = true
  └─ stream: [upgrade_tx, SetSLChainId(sl_chain_id, u64::MAX)]

Block 1 — execute_block_in_vm
  └─ executes upgrade_tx → upgrade_tx_executed = true (no break)
  └─ executes SetSLChainId → SealReason::SLChainIdUpdateTx

Block 1 — on_canonical_state_change (SlChainIdSubpool)
  └─ sees SetSLChainId(u64::MAX) → continue (skip pop_wait, skip tracking)
       └─ starting_migration_number remains 0

On restart
  └─ GatewayMigrationWatcher: current_migration_number = 0 → binary search finds L1 block 0
```

### In-run upgrade (v30 → v31)

```
Node starts, starting_block > 1, current_protocol_version < v31
  └─ GatewayMigrationWatcher NOT spawned

L1UpgradeTxWatcher detects upgrade event with new_version >= v31
  └─ wait_until_timestamp(...)
  └─ upgrade_subpool ← upgrade tx (existing, no change)

Block N — BlockContextProvider::prepare_command (Produce)
  └─ reads upgrade_metadata: new_version >= v31, self.protocol_version < v31
  └─ inject_bootstrap = true
  └─ stream: [upgrade_tx, SetSLChainId(sl_chain_id, u64::MAX)]

Block N — execute_block_in_vm
  └─ executes upgrade_tx → upgrade_tx_executed = true
  └─ executes SetSLChainId → SealReason::SLChainIdUpdateTx

Block N — on_canonical_state_change (SlChainIdSubpool)
  └─ sees SetSLChainId(u64::MAX) → continue (skip pop_wait, skip tracking)
       └─ starting_migration_number remains 0

On next restart (now current_protocol_version >= v31)
  └─ GatewayMigrationWatcher spawned: current_migration_number = 0 → scans from L1 block 0
```

## Invariants

- The genesis bootstrap `SetSLChainId` is produced exactly once: the triggering predicate in
  `prepare_command` is `upgrade_metadata.protocol_version >= v31 && self.protocol_version < v31`.
  At genesis on a v31 chain, `self.protocol_version` is initialised to the pre-genesis version
  (< v31) and the genesis upgrade tx carries v31, so the predicate fires for the first and only
  time. Chains that start below v31 receive no bootstrap injection via this path.
- The in-run upgrade bootstrap `SetSLChainId` is produced exactly once per process lifetime:
  only when `BlockContextProvider` detects the v30→v31 boundary crossing during a `Produce`
  block. The version is updated to v31+ immediately after, so subsequent calls to
  `prepare_command` will not re-enter this branch.
- `starting_migration_number` is never set to `u64::MAX`; it remains `0` until the first real
  gateway migration fires.
- `sl_chain_id` at startup is always the settlement layer chain ID (L1 chain ID when not on
  Gateway, gateway chain ID otherwise). It is never zero.
- The contract's own idempotency guard in `SystemContext.setSettlementLayerChainId`
  (`if (currentSettlementLayerChainId != _newSettlementLayerChainId)`) means a duplicate
  bootstrap call is a safe no-op on L2. This is an external assumption: the guard is present in
  `era-contracts/l1-contracts/contracts/l2-system/zksync-os/SystemContext.sol` and must remain
  for this invariant to hold.
- After an in-run v30→v31 upgrade, `GatewayMigrationWatcher` is not running until the next
  restart. Any gateway migration that fires in this window is picked up from L1 history on
  restart, because `starting_migration_number` stays at `0`.
- The bootstrap tx is never inserted into `SlChainIdSubpool`. `on_canonical_state_change`
  correctly handles it by matching the `u64::MAX` sentinel and skipping `pop_wait`.
