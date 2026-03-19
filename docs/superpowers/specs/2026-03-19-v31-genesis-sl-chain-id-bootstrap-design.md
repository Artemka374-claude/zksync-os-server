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
2. **In-run upgrade from v30 to v31** — the `L1UpgradeTxWatcher` detects the upgrade event and,
   if the L2 value is still `0`, injects the bootstrap transaction without requiring a restart.

## Scope

**In scope**:
- Genesis bootstrap (`starting_block == 1`, protocol version >= v31).
- In-run upgrade bootstrap: when `L1UpgradeTxWatcher` processes an upgrade whose new version is
  >= v31 and whose previous tracked version was < v31, and the current L2 value of
  `currentSettlementLayerChainId` is `0`.

**Not in scope**: chains that were already at v31 before this code ships, or any case where
`currentSettlementLayerChainId` is already non-zero. The L2 contract's own idempotency guard
makes a redundant call a no-op, but we skip injection entirely when the value is non-zero to avoid
unnecessary transactions.

## Design

### `sl_chain_id` value

`node_startup_state.l1_state.sl_chain_id` is populated by `sl_provider.get_chain_id()` in
`L1State::fetch` (`lib/contract_interface/src/l1_discovery.rs`). When `gateway_rpc_url` is not
configured, `sl_provider` equals `l1_provider`, so `sl_chain_id` equals `l1_chain_id`. This is
always the chain ID of the current settlement layer at startup — the correct value to write into
`SystemContext`.

`L1State::fetch` also calls `validate_chain_ids`, which confirms that `sl_chain_id` is whitelisted
on the L1 Bridgehub. A chain ID of 0 cannot be whitelisted, so `sl_chain_id` is always non-zero
when startup proceeds past that validation.

### Sentinel migration number: `u64::MAX`

`SetSLChainId` carries a `migration_number` salt. This value propagates through the system as
follows:

1. `SlChainIdSubpool::on_canonical_state_change` returns `Option<u64>` (the last migration number
   seen) to the pool.
2. The pool propagates it as `PoolOutcome::last_migration_number`.
3. `BlockContextProvider::on_canonical_state_change` reads `outcome.last_migration_number` and, if
   `Some`, advances `next_migration_number`.
4. `next_migration_number` becomes `starting_migration_number` in the WAL record for subsequent
   blocks.
5. On restart, `starting_migration_number` from the first replay record becomes the
   `current_migration_number` passed to `GatewayMigrationWatcher`, which uses it to binary-search
   for its starting L1 block via `IChainAssetHandler.migrationNumber(chainId) >=
   current_migration_number`.

The bootstrap transaction must not disturb this tracking. We designate `u64::MAX` as a sentinel
meaning "bootstrap, not a real gateway migration". No real migration will ever reach this number.
Change 1 below adds a guard: when `on_canonical_state_change` sees `migration_number == u64::MAX`
it returns `None`, leaving `next_migration_number` at `0`. The gateway migration watcher then
binary-searches for the first L1 block where `migrationNumber >= 0` (always true), i.e., block 0
— the correct starting point before any real migration.

`BlockContextProvider` already handles `None` via an `if let Some(...)` guard, so no changes are
needed there. This filter is **load-bearing**: if the sentinel were propagated as `Some(u64::MAX)`,
`BlockContextProvider` would compute `u64::MAX + 1`, which panics in debug builds and silently
wraps to `0` in release — resetting `next_migration_number` incorrectly. The guard in
`on_canonical_state_change` is therefore a correctness requirement, not just a bookkeeping
convenience.

The sentinel applies equally to both the genesis and the in-run upgrade case, so no additional
sentinel handling is required for the new path.

### Concurrency with `GatewayMigrationWatcher`

#### Genesis case

`GatewayMigrationWatcher` is spawned at startup **before** block 1 is executed — not after genesis
completes. It starts polling L1 immediately. If a real `MigrateToGateway` event arrives on L1
during the genesis block's execution window, the watcher calls `sl_chain_id_subpool.insert()`,
pushing the migration tx onto `pending_txs` while the bootstrap tx is already there.

`SlChainIdTransactionsStream` is intentionally a single-item stream: it serves one transaction
then closes. Step-by-step deque state in the race scenario (deque shown front→back):

1. Genesis injection: `insert(bootstrap)` → `push_front` → deque: `[bootstrap]`
2. Watcher fires concurrent migration: `insert(migration_tx)` → `push_front` →
   deque: `[migration_tx, bootstrap]`
3. Block 1 — `best_transactions_stream`: reads `pending_txs.back()` = bootstrap → serves it,
   stream closes.
4. Block 1 — `on_canonical_state_change`: calls `pop_back()` → removes bootstrap →
   deque: `[migration_tx]`.
5. Block 2 — `best_transactions_stream`: reads `pending_txs.back()` = migration_tx →
   serves it immediately (`StreamState::Pending`).

The migration tx is delivered in block 2 with no loss. This is the normal multi-queued behaviour
of the subpool, unchanged by this feature.

Additionally, because `starting_migration_number` stays at `0`, the watcher re-scans from L1
block 0 on any restart, ensuring any real migration event is also picked up from L1 history even
if a concurrent in-memory delivery was interrupted.

#### In-run upgrade case

`GatewayMigrationWatcher` is only spawned when `current_protocol_version >= v31` at startup
(see `node/bin/src/lib.rs`). When the upgrade triggers at runtime the version was < v31 at
startup, so the watcher is **not running**. There is therefore no concurrent `insert()` from the
migration watcher — the `sl_chain_id_subpool` deque will contain only the bootstrap tx.

Any real gateway migration that fires on L1 between the in-run upgrade and the next restart will
not be tracked in-memory. After the restart `GatewayMigrationWatcher` starts with
`current_migration_number = 0` (because `starting_migration_number` remains `0` thanks to the
sentinel) and re-scans from L1 block 0, picking up any migration events from history. No migration
event is permanently lost.

### Reading `currentSettlementLayerChainId` for the upgrade case

To avoid injecting a redundant bootstrap tx when the value is already set, `L1UpgradeTxWatcher`
reads the current L2 value before injecting. This uses the same in-process `EthCallHandler`
pattern as `InteropFeeUpdater` uses for `interopProtocolFee()` — a direct VM execution against
local state, with no dependency on the HTTP RPC server being reachable.

```rust
let request = TransactionRequest::default()
    .with_to(SYSTEM_CONTEXT_ADDRESS)
    .with_input(Bytes::from(
        ISystemContext::currentSettlementLayerChainIdCall {}.abi_encode(),
    ));
let output = self
    .eth_call_handler
    .call_impl(request, Some(BlockId::latest()), None, None)
    .context("read currentSettlementLayerChainId")
    .map_err(L1WatcherError::Batch)?;
let current_sl_chain_id = U256::from_be_bytes(
    output.as_ref().try_into().context("unexpected return data length")
        .map_err(L1WatcherError::Batch)?,
);
if current_sl_chain_id == U256::ZERO {
    let bootstrap = SystemTxEnvelope::set_sl_chain_id(self.sl_chain_id, u64::MAX);
    self.sl_chain_id_subpool.insert(bootstrap).await;
}
```

`EthCallHandler::call_impl` returns an `anyhow::Result`, which does not implement `From` for
`L1WatcherError` directly. The `.map_err(L1WatcherError::Batch)` wrapping follows the same
pattern used by `fetch_upgrade_info` (line 315: `.map_err(L1WatcherError::Batch)?`).

The `EthCallHandler` call reads from the node's local replay storage with `BlockId::latest()`.
This is always safe for the in-run upgrade case: since `starting_block > 1` (blocks have already
been executed and persisted), replay storage is populated before `L1UpgradeTxWatcher` could fire.

`EthCallHandler` is generic over `RpcStorage: ReadRpcStorage`. Making `L1UpgradeTxWatcher`
generic over the same parameter follows the same pattern as `InteropFeeUpdater<RpcStorage>`, and
the existing `From<T> for Box<dyn ProcessRawEvents>` blanket impl erases the type when the watcher
is passed to `L1Watcher`.

### Changes

#### 1. `lib/mempool/src/subpools/sl_chain_id.rs` — skip sentinel in `on_canonical_state_change`

Applies to both genesis and in-run upgrade bootstrap transactions.

```rust
if let SystemTxType::SetSLChainId(migration_number) = *tx.system_subtype() {
    // u64::MAX is the bootstrap sentinel — not a real gateway migration.
    // Skipping it keeps starting_migration_number at 0 so the gateway migration
    // watcher starts its L1 binary search from block 0.
    if migration_number != u64::MAX {
        last_migration_number = Some(migration_number);
    }
}
```

#### 2. `node/bin/src/lib.rs` — inject bootstrap tx at genesis

In the existing `if starting_block == 1` block (where the genesis upgrade tx is inserted into
`upgrade_subpool`), also inject a bootstrap `SetSLChainId` when the genesis protocol version
is >= v31:

```rust
if genesis_upgrade.protocol_version >= ProtocolSemanticVersion::new(0, 31, 0) {
    let bootstrap = SystemTxEnvelope::set_sl_chain_id(
        node_startup_state.l1_state.sl_chain_id,
        u64::MAX, // bootstrap sentinel — not a real migration number
    );
    sl_chain_id_subpool.insert(bootstrap).await;
}
```

`sl_chain_id_subpool` and `node_startup_state.l1_state.sl_chain_id` are both already in scope at
this point.

#### 3. `lib/contract_interface/src/lib.rs` — add getter to `ISystemContext`

Add `currentSettlementLayerChainId()` to the existing interface so its call data can be
ABI-encoded. No `#[sol(rpc)]` annotation is needed — the call is made via `EthCallHandler`, not
via an alloy `DynProvider`.

```rust
interface ISystemContext {
    function setSettlementLayerChainId(uint256 _newSettlementLayerChainId);
    function currentSettlementLayerChainId() external view returns (uint256);
}
```

#### 4. `lib/l1_watcher/src/upgrade_tx_watcher.rs` — add in-run upgrade bootstrap

Make `L1UpgradeTxWatcher` generic over `RpcStorage: ReadRpcStorage + Send + Sync + 'static` and
add three new fields:

```rust
pub struct L1UpgradeTxWatcher<RpcStorage> {
    // ... existing fields ...
    sl_chain_id_subpool: SlChainIdSubpool,
    sl_chain_id: u64,
    eth_call_handler: EthCallHandler<RpcStorage>,
}
```

Update `create_watcher` to accept `sl_chain_id_subpool: SlChainIdSubpool`, `sl_chain_id: u64`,
and `eth_call_handler: EthCallHandler<RpcStorage>`.

In `process_event`, capture the old version **before** `wait_until_timestamp` (i.e., before
the existing timestamp wait and the upgrade tx insert), then check the version boundary after:

```rust
// Capture before any state mutation.
let old_version = self.current_protocol_version.clone();

// ... existing wait_until_timestamp and upgrade_subpool.insert calls ...

self.current_protocol_version = upgrade_info.protocol_version().clone();
self.upgrade_subpool.insert(upgrade_info).await;

if self.current_protocol_version >= ProtocolSemanticVersion::new(0, 31, 0)
    && old_version < ProtocolSemanticVersion::new(0, 31, 0)
{
    // ... EthCallHandler read and conditional insert as shown above ...
}
```

#### 5. `node/bin/src/lib.rs` — wire new fields into `L1UpgradeTxWatcher`

`rpc_storage` is currently created **after** the upgrade watcher is spawned. Reorder: move
`persistent_batch_storage` and `rpc_storage` construction before the upgrade watcher spawn (no
logic change, just reordering).

Then construct `EthCallHandler` at the same site and pass all three new arguments:

```rust
let eth_call_handler = EthCallHandler::new(
    config.rpc_config.clone().into(),
    rpc_storage.clone(),
    chain_id,
    last_constructed_block_ctx_receiver.clone(),
);

L1UpgradeTxWatcher::create_watcher(
    config.l1_watcher_config.clone().into(),
    node_startup_state.l1_state.diamond_proxy_l1.clone(),
    node_startup_state.l1_state.diamond_proxy_sl.clone(),
    bytecode_supplier_address,
    current_protocol_version.clone(),
    upgrade_subpool,
    sl_chain_id_subpool.clone(),
    node_startup_state.l1_state.sl_chain_id,
    eth_call_handler,
)
```

### Data flow

#### Genesis

```
Node starts, starting_block == 1, protocol_version >= v31
  │
  ├─ upgrade_subpool     ← genesis upgrade tx (existing)
  └─ sl_chain_id_subpool ← SetSLChainId { chain_id: sl_chain_id, salt: u64::MAX }  (new)
  │
  └─ GatewayMigrationWatcher spawned concurrently, starts polling from L1 block 0

Block 1 execution
  └─ block executor includes both transactions

on_canonical_state_change (SlChainIdSubpool)
  └─ sees migration_number == u64::MAX → returns None (skips starting_migration_number update)
       └─ starting_migration_number remains 0

BlockContextProvider.on_canonical_state_change
  └─ outcome.last_migration_number == None → next_migration_number unchanged (stays 0)

On restart
  └─ GatewayMigrationWatcher: current_migration_number = 0 → binary search finds L1 block 0
```

#### In-run upgrade (v30 → v31)

```
Node starts, starting_block > 1, current_protocol_version < v31
  └─ GatewayMigrationWatcher NOT spawned

L1UpgradeTxWatcher detects upgrade event with new_version >= v31
  └─ wait_until_timestamp(...)
  └─ upgrade_subpool ← upgrade tx (existing)
  └─ old_version < v31, new_version >= v31 → check L2 value
       └─ EthCallHandler.call_impl(currentSettlementLayerChainId()) → 0
       └─ sl_chain_id_subpool ← SetSLChainId { chain_id: sl_chain_id, salt: u64::MAX }

Next block execution
  └─ block executor includes SetSLChainId bootstrap tx

on_canonical_state_change (SlChainIdSubpool)
  └─ sees migration_number == u64::MAX → returns None
       └─ starting_migration_number remains 0

On next restart (now starting_block > 1, current_protocol_version >= v31)
  └─ GatewayMigrationWatcher spawned: current_migration_number = 0 → scans from L1 block 0
```

### What is NOT changed

- `GatewayMigrationWatcher` — no changes.
- `BlockContextProvider` — no changes; existing `None` guard already handles the sentinel path.
- `SystemTxEnvelope::set_sl_chain_id` — reused as-is with the sentinel salt.
- `Pool` — no changes to how `sl_chain_id_subpool` is consumed by the pool; dispatch on
  `SetSLChainId` is unchanged.
- `SlChainIdTransactionsStream` — single-item stream semantics are intentionally preserved.

## Invariants

- The genesis bootstrap `SetSLChainId` is emitted exactly once: when `starting_block == 1`
  and `protocol_version >= v31`. Chains that start at a protocol version below v31 receive no
  bootstrap injection via this path.
- The in-run upgrade bootstrap `SetSLChainId` is emitted at most once per process lifetime:
  only when `L1UpgradeTxWatcher` detects the v30→v31 boundary crossing and the L2 value is `0`.
- `starting_migration_number` is never set to `u64::MAX`; it remains `0` until the first real
  gateway migration fires.
- `sl_chain_id` at startup is always the settlement layer chain ID (L1 chain ID when not on
  Gateway, gateway chain ID otherwise). It is never zero.
- The contract's own idempotency guard in `SystemContext.setSettlementLayerChainId`
  (`if (currentSettlementLayerChainId != _newSettlementLayerChainId)`) means a duplicate bootstrap
  call is a safe no-op on L2. This is an external assumption: the guard is present in
  `era-contracts/l1-contracts/contracts/l2-system/zksync-os/SystemContext.sol` and must remain
  for this invariant to hold.
- After an in-run v30→v31 upgrade, `GatewayMigrationWatcher` is not running until the next
  restart. Any gateway migration that fires in this window is picked up from L1 history on restart,
  because `starting_migration_number` stays at `0`.
