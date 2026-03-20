# V31 SL Chain ID Bootstrap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Inject a `SetSLChainId` system transaction to bootstrap `SystemContext.currentSettlementLayerChainId` on L2 at genesis (v31+) and during in-run v30→v31 upgrades, co-located in the same block as the upgrade transaction, so v31 batch proofs are valid from the first batch.

**Architecture:** `BlockContextProvider::prepare_command` detects the v30→v31 version crossing from `upgrade_metadata` and chains a dynamically-constructed `SetSLChainId(sl_chain_id, u64::MAX)` transaction immediately after the upgrade transaction in the same stream. No changes are needed in `L1UpgradeTxWatcher` or genesis startup injection. The sentinel `u64::MAX` is handled in `SlChainIdSubpool::on_canonical_state_change` by skipping `pop_wait` (the tx was never queued). The block executor's post-upgrade seal is relaxed to allow exactly one more tx before sealing.

**Tech Stack:** Rust, Tokio, alloy, `cargo nextest`, existing subpool and sequencer infrastructure.

---

## File Map

| File | Change |
|------|--------|
| `lib/mempool/src/subpools/sl_chain_id.rs` | Skip sentinel in `on_canonical_state_change` (guard before `pop_wait`); add unit tests |
| `lib/sequencer/src/execution/block_context_provider.rs` | Add `sl_chain_id` field; detect v31 crossing in `prepare_command`; chain bootstrap tx |
| `lib/sequencer/src/execution/execute_block_in_vm.rs` | Replace upgrade-tx `break` with flag; allow stream exhaustion after upgrade tx |
| `node/bin/src/lib.rs` | Pass `sl_chain_id` to `BlockContextProvider::new` |

---

## Task 1: Sentinel guard in `SlChainIdSubpool::on_canonical_state_change`

**Files:**
- Modify: `lib/mempool/src/subpools/sl_chain_id.rs`

### Why

The bootstrap tx is dynamically constructed in `BlockContextProvider` and never inserted into
`SlChainIdSubpool`. When it appears in the replay record, `on_canonical_state_change` must skip
`pop_wait` (which would deadlock on an empty deque) and skip migration tracking (to avoid
`u64::MAX + 1` overflow in `BlockContextProvider`). The guard must appear before `pop_wait`.

- [ ] **Step 1.1: Write the failing unit tests**

Add at the bottom of `lib/mempool/src/subpools/sl_chain_id.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn make_set_sl_chain_id_tx(migration_number: u64) -> SystemTxEnvelope {
        SystemTxEnvelope::set_sl_chain_id(1u64, migration_number)
    }

    #[tokio::test]
    async fn sentinel_skipped_without_pop_wait() {
        // The sentinel tx is never inserted into the subpool (it is dynamically constructed).
        // on_canonical_state_change must handle it without calling pop_wait.
        let subpool = SlChainIdSubpool::default();
        let tx = make_set_sl_chain_id_tx(u64::MAX);
        // NOTE: no subpool.insert() — sentinel was never queued
        let result = subpool.on_canonical_state_change(vec![&tx]).await;
        assert_eq!(result, None, "u64::MAX sentinel must not update migration tracking");
    }

    #[tokio::test]
    async fn real_migration_number_returned() {
        let subpool = SlChainIdSubpool::default();
        let tx = make_set_sl_chain_id_tx(42);
        subpool.insert(tx.clone()).await;
        let result = subpool.on_canonical_state_change(vec![&tx]).await;
        assert_eq!(result, Some(42));
    }
}
```

`SystemTxEnvelope` is already imported at the top of the file; `use super::*` brings everything
into the test module.

- [ ] **Step 1.2: Run tests — expect sentinel test to fail (deadlock or panic)**

```bash
cargo nextest run --release -p zksync_os_mempool 2>&1 | tail -20
```

Expected: `sentinel_skipped_without_pop_wait` hangs or panics (current code calls `pop_wait`
on an empty deque). Use a short timeout if needed.

- [ ] **Step 1.3: Apply the sentinel guard**

In `lib/mempool/src/subpools/sl_chain_id.rs`, change `on_canonical_state_change`. The existing
loop body is:
```rust
for tx in txs {
    let pending_tx = self.pop_wait().await;
    assert_eq!(tx, &pending_tx);
    if let SystemTxType::SetSLChainId(migration_number) = *tx.system_subtype() {
        last_migration_number = Some(migration_number);
    }
}
```

Replace with:
```rust
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

- [ ] **Step 1.4: Run tests — expect both to pass**

```bash
cargo nextest run --release -p zksync_os_mempool 2>&1 | tail -20
```

Expected: both tests pass.

- [ ] **Step 1.5: Commit**

```bash
git add lib/mempool/src/subpools/sl_chain_id.rs
git commit -m "fix(mempool): skip u64::MAX sentinel in SlChainIdSubpool::on_canonical_state_change"
```

---

## Task 2: Bootstrap injection in `BlockContextProvider`

**Files:**
- Modify: `lib/sequencer/src/execution/block_context_provider.rs`

### Why

`BlockContextProvider` is the natural place to detect the v30→v31 version crossing: it already
reads `upgrade_metadata` and tracks `self.protocol_version` in `prepare_command`. When the
crossing is detected, we chain a `SetSLChainId(sl_chain_id, u64::MAX)` tx directly after the
upgrade tx in the stream. This avoids all timing concerns — both txs are in the stream before
the block executor reads a single item.

- [ ] **Step 2.1: Add `sl_chain_id` field to `BlockContextProvider`**

In the struct definition, add after the existing fields:
```rust
/// Settlement layer chain ID, written into SystemContext on the v30→v31 upgrade block.
sl_chain_id: u64,
```

Add the corresponding parameter to `BlockContextProvider::new` and store it in `Self { ... }`.

- [ ] **Step 2.2: Chain the bootstrap tx in `prepare_command`**

In the `BlockCommand::Produce` branch of `prepare_command`, the current upgrade detection block
reads:
```rust
let force_preimages = if let Some(upgrade_metadata) = best_txs.upgrade_metadata
    && upgrade_metadata.protocol_version > self.protocol_version
{
    // ... timestamp check ...
    self.protocol_version = upgrade_metadata.protocol_version.clone();
    upgrade_metadata.force_preimages.clone()
} else {
    Vec::new()
};
```

Rework this block to also detect the v31 crossing and chain the bootstrap tx. Use
`.as_ref().and_then(...)` to extract what is needed without consuming the borrow, then act:

```rust
// Collect upgrade info without holding a borrow on best_txs so we can mutate best_txs.stream.
let upgrade_info = best_txs.upgrade_metadata.as_ref().and_then(|m| {
    (m.protocol_version > self.protocol_version).then(|| {
        let inject_bootstrap =
            m.protocol_version >= ProtocolSemanticVersion::new(0, 31, 0)
                && self.protocol_version < ProtocolSemanticVersion::new(0, 31, 0);
        (m.protocol_version.clone(), m.force_preimages.clone(), inject_bootstrap)
    })
});

let force_preimages = if let Some((new_version, preimages, inject_bootstrap)) = upgrade_info {
    // ... existing timestamp check using new_version ...
    anyhow::ensure!(
        new_version.timestamp <= current_timestamp,
        // existing message
    );

    if inject_bootstrap {
        // Chain SetSLChainId immediately after the upgrade tx.
        // currentSettlementLayerChainId is provably 0 here: gateway migrations only fire on
        // v31+, so no prior block could have set this value.
        let bootstrap: ZkTransaction =
            SystemTxEnvelope::set_sl_chain_id(self.sl_chain_id, u64::MAX).into();
        // std::mem::replace extracts the old stream without a borrow conflict.
        // `.stream` is the public BoxStream field — accessed directly because
        // MarkingTxStream does not implement Stream itself.
        let old_stream = std::mem::replace(
            &mut best_txs.stream,
            MarkingTxStream::unmarkable(futures::stream::empty()),
        );
        best_txs.stream = MarkingTxStream::unmarkable(
            old_stream.stream.chain(futures::stream::once(std::future::ready(bootstrap))),
        );
    }
    self.protocol_version = new_version;
    preimages
} else {
    Vec::new()
};
```

Note: the `tracing::info!` log that references `upgrade_metadata` may need to be updated to use
`new_version` since `upgrade_metadata` is no longer directly in scope. Check the existing log
statement and adjust field references accordingly.

- [ ] **Step 2.3: Add required imports**

Near the top of `block_context_provider.rs`, ensure the following are imported (add any missing):
```rust
use zksync_os_types::{SystemTxEnvelope, ZkTransaction};
// futures::stream is likely already imported; if not:
use futures::stream;
```

- [ ] **Step 2.4: Verify `zksync_os_sequencer` compiles**

```bash
cargo build -p zksync_os_sequencer 2>&1 | tail -30
```

Expected: compiles cleanly. Fix any type or borrow errors.

- [ ] **Step 2.5: Commit**

```bash
git add lib/sequencer/src/execution/block_context_provider.rs
git commit -m "feat(sequencer): inject SetSLChainId bootstrap in BlockContextProvider on v30→v31 upgrade"
```

---

## Task 3: Relax post-upgrade seal in `execute_block_in_vm`

**Files:**
- Modify: `lib/sequencer/src/execution/execute_block_in_vm.rs`

### Why

The block executor currently `break`s immediately after a successful upgrade tx. To allow the
chained bootstrap tx to execute in the same block, the loop must continue after the upgrade tx.
The flag approach preserves the existing seal contract: if the stream exhausts right after the
upgrade tx (no bootstrap chained), we still seal with `SealReason::UpgradeTx` — not an error.

- [ ] **Step 3.1: Add the `upgrade_tx_executed` flag**

Before the `loop {` that drives block execution, add:
```rust
let mut upgrade_tx_executed = false;
```

- [ ] **Step 3.2: Replace the upgrade-tx `break` with the flag**

Find the match arm (under `SealPolicy::Decide(..) | SealPolicy::UntilExhausted { allowed_to_finish_early: true }`):
```rust
tracing::debug!(block_number = ctx.block_number, "sealing block as upgrade tx was executed");
break SealReason::UpgradeTx;
```

Replace with:
```rust
tracing::debug!(block_number = ctx.block_number, "upgrade tx executed, continuing for bootstrap tx");
upgrade_tx_executed = true;
```

The `UntilExhausted { allowed_to_finish_early: false }` arm is unchanged.

- [ ] **Step 3.3: Guard stream exhaustion**

Find the stream-exhaustion branch inside the `select!`:
```rust
let Some(tx) = maybe_tx else {
    tracing::debug!(
        block_number = ctx.block_number,
        txs = executed_txs.len(),
        "stream exhausted → sealing"
    );
    break SealReason::TxStreamExhausted;
};
```

Replace with:
```rust
let Some(tx) = maybe_tx else {
    if upgrade_tx_executed {
        // Stream closed after upgrade tx with no bootstrap tx chained — seal normally.
        break SealReason::UpgradeTx;
    }
    tracing::debug!(
        block_number = ctx.block_number,
        txs = executed_txs.len(),
        "stream exhausted → sealing"
    );
    break SealReason::TxStreamExhausted;
};
```

- [ ] **Step 3.4: Verify `zksync_os_sequencer` compiles and tests pass**

```bash
cargo build -p zksync_os_sequencer 2>&1 | tail -30
cargo nextest run --release -p zksync_os_sequencer 2>&1 | tail -20
```

Expected: compiles and tests pass.

- [ ] **Step 3.5: Commit**

```bash
git add lib/sequencer/src/execution/execute_block_in_vm.rs
git commit -m "fix(sequencer): allow SetSLChainId to execute in same block as upgrade tx"
```

---

## Task 4: Wire `sl_chain_id` into `BlockContextProvider` in `node/bin`

**Files:**
- Modify: `node/bin/src/lib.rs`

### Why

`BlockContextProvider::new` now requires `sl_chain_id: u64`. `node_startup_state.l1_state.sl_chain_id`
is already in scope at the `BlockContextProvider::new` call site.

- [ ] **Step 4.1: Add `sl_chain_id` argument to `BlockContextProvider::new` call**

Find the `BlockContextProvider::new(...)` call in `node/bin/src/lib.rs` and add
`node_startup_state.l1_state.sl_chain_id` as the new `sl_chain_id` argument. The exact
position in the argument list mirrors the field order in the struct.

- [ ] **Step 4.2: Build the full workspace**

```bash
cargo build --workspace 2>&1 | tail -30
```

Expected: compiles cleanly.

- [ ] **Step 4.3: Commit**

```bash
git add node/bin/src/lib.rs
git commit -m "feat(node): pass sl_chain_id to BlockContextProvider"
```

---

## Task 5: Verify and open PR

- [ ] **Step 5.1: Format**

```bash
cargo fmt --all
cargo fmt --all --check
```

- [ ] **Step 5.2: Clippy**

```bash
cargo clippy --all-targets --all-features --workspace --exclude zksync_os_integration_tests -- -D warnings 2>&1 | tail -40
```

Fix any warnings before proceeding.

- [ ] **Step 5.3: Unit tests**

```bash
cargo nextest run --release --workspace --exclude zksync_os_integration_tests 2>&1 | tail -20
```

Expected: all pass, including the two new tests in `sl_chain_id.rs`.

- [ ] **Step 5.4: Integration tests**

```bash
cargo nextest run -p zksync_os_integration_tests 2>&1 | tail -40
```

Expected: all pass. The bootstrap is exercised by any integration test that starts a fresh v31
node (genesis case) or runs a v30→v31 upgrade (in-run case).

- [ ] **Step 5.5: Open PR against `main`**

Branch must be off `main`. The design doc and plan files must not be included — check
`git diff main` and unstage them if present.

```bash
gh pr create \
  --base main \
  --title "feat(sequencer): co-locate SetSLChainId bootstrap with v30→v31 upgrade transaction" \
  --body "$(cat <<'EOF'
## Summary

- Detects the v30→v31 protocol version crossing in `BlockContextProvider::prepare_command` and
  chains a `SetSLChainId(sl_chain_id, u64::MAX)` bootstrap transaction immediately after the
  upgrade transaction in the same block stream.
- Relaxes the post-upgrade seal in `execute_block_in_vm` to allow the bootstrap transaction to
  execute before sealing.
- Adds a sentinel guard to `SlChainIdSubpool::on_canonical_state_change` so the dynamically-
  constructed bootstrap tx (never inserted into the subpool) does not cause a deadlock or
  corrupt `starting_migration_number`.

Covers both genesis (v31 chain starting from block 1) and in-run upgrade (v30→v31 at runtime).
No changes to `L1UpgradeTxWatcher`, `EthCallHandler`, or genesis startup injection.

## Test plan

- [ ] Unit tests in `sl_chain_id.rs` cover sentinel skip (no pop_wait, returns `None`) and
      normal migration number (returns `Some`)
- [ ] `cargo nextest run --release --workspace --exclude zksync_os_integration_tests` passes
- [ ] `cargo nextest run -p zksync_os_integration_tests` passes

No tests added for in-run upgrade bootstrap end-to-end — an integration test for the full
v30→v31 upgrade path is deferred as a follow-up.
EOF
)"
```
