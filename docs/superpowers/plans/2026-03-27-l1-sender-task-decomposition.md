# L1 Sender Task Decomposition — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the single-loop `L1SenderLoop` in `lib/l1_sender` with two independent async tasks (`Submitter` and `Watcher`) connected by bounded channels, so that receipt polling and downstream forwarding are never blocked by a submission error.

**Architecture:** The `Submitter` owns the L1 provider, estimates fees, builds and submits transactions, and forwards `InFlightTx` items to the `Watcher` via an `in_flight` channel. The `Watcher` polls all receipt futures concurrently via `FuturesOrdered`, forwards confirmed commands downstream, and sends timed-out or failed commands back to the `Submitter` via a `resubmit` channel. Both tasks run under `tokio::try_join!` inside `run_l1_sender`; a fatal error in either task cancels the other and crashes the binary (existing behaviour).

**Tech Stack:** Rust, Tokio, `futures::stream::FuturesOrdered`, alloy provider stack (already present).

**Base branch:** `feat/l1-sender-error-handling`
**Implementation branch:** `feat/l1-sender-task-decomp`

---

## Background Context

### What already exists on `feat/l1-sender-error-handling`

The branch already has:
- `lib/l1_sender/src/error.rs` — `L1SendError`, `RecoverableReason`, `classify_send_raw_error`
- `lib/l1_sender/src/metrics.rs` — `L1SenderState` (including `GasBlocked`, `BlobFeeBlocked`, `TransientBackoff`), `L1_SENDER_METRICS`
- `lib/l1_sender/src/lib.rs` — `L1SenderLoop` with `receive`, `send_pending`, `wait_for_inclusion`, `forward_downstream`, `handle_transient`, `handle_recoverable`, `ExponentialBackoff`, `InFlightTx`, `run_l1_sender`, `process_prepending_passthrough_commands`, `register_operator`, `validate_tx_receipt_reverted`, `reason_label`

The task is to **replace `L1SenderLoop`** with two structs (`Submitter` + `Watcher`) in two new files, then update `lib.rs` to wire them together. All supporting infrastructure (error types, metrics, helpers) stays.

### Key types you will use

```rust
// error.rs
pub enum L1SendError {
    Transient(anyhow::Error),
    Recoverable { reason: RecoverableReason, source: anyhow::Error },
    Fatal(anyhow::Error),
}
pub enum RecoverableReason { GasBlocked, BlobFeeBlocked, TxTimeout, NonceTooLow }
impl L1SendError {
    pub fn into_anyhow(self) -> anyhow::Error { ... }
    pub fn classify_send_raw_error(err: anyhow::Error) -> Self { ... }
}

// metrics.rs
pub enum L1SenderState {
    WaitingRecv, WaitingSend, SendingToL1, WaitingL1Inclusion,
    GasBlocked, BlobFeeBlocked, TransientBackoff,
}
pub static L1_SENDER_METRICS: vise::Global<L1SenderMetrics>;
// L1SenderMetrics has: .transient_errors.inc(), .recoverable_errors[&label].inc(),
//   .report_tx_receipt(&command, receipt), .report_l1_eip_1559_estimation(est),
//   .report_blob_base_fee(fee), .balance[&name].set(v), .nonce[&name].set(v),
//   .parallel_transactions[&name].set(n)

// commands/mod.rs
pub enum L1SenderCommand<C: SendToL1> { SendToL1(C), Passthrough(Box<SignedBatchEnvelope<FriProof>>) }
pub trait SendToL1: Into<Vec<SignedBatchEnvelope<FriProof>>> + AsRef<[...]> + AsMut<[...]> + Display {
    const NAME: &'static str;
    const SENT_STAGE: BatchExecutionStage;
    const MINED_STAGE: BatchExecutionStage;
    const PASSTHROUGH_STAGE: BatchExecutionStage;
    fn solidity_call(&self, gateway: bool, operator: &Address) -> Bytes;
    fn blob_sidecar(&self) -> Option<BlobTransactionSidecar> { None }
    fn display_range(cmds: &[Self]) -> String { ... }
}
```

### Channel topology

```
upstream PeekableReceiver<L1SenderCommand<Input>>
    ──► Submitter ──► mpsc::channel<InFlightTx<Input>> ──► Watcher ──► mpsc::Sender<SignedBatchEnvelope<FriProof>> (downstream)
            ▲                                                    │
            └──────── mpsc::channel<Input> (resubmit) ◄─────────┘
```

---

## File Map

| Action | Path | Responsibility |
|---|---|---|
| **Create** | `lib/l1_sender/src/submitter.rs` | `Submitter` struct and `run()` — all provider interaction, fee estimation, tx submission |
| **Create** | `lib/l1_sender/src/watcher.rs` | `Watcher` struct and `run()` — `FuturesOrdered`, receipt handling, resubmit routing |
| **Modify** | `lib/l1_sender/src/lib.rs` | Remove `L1SenderLoop`; keep shared types; update `run_l1_sender` to spawn both tasks |
| **Modify** | `node/bin/src/config/mod.rs` | Raise `max_priority_fee_per_gas` default from 1 gwei to 10 gwei |

---

## Task 1: Set up the implementation branch

**Files:** none (branch setup)

- [ ] **Step 1: Create the branch**

```bash
git checkout feat/l1-sender-error-handling
git checkout -b feat/l1-sender-task-decomp
```

- [ ] **Step 2: Verify baseline compiles and tests pass**

```bash
cargo check -p zksync_os_l1_sender 2>&1
cargo nextest run --release --workspace --exclude zksync_os_integration_tests 2>&1 | tail -10
```

Expected: clean compile, all tests pass.

---

## Task 2: Create `watcher.rs`

The Watcher is simpler than the Submitter (no provider calls during normal operation) so we implement it first.

**Files:**
- Create: `lib/l1_sender/src/watcher.rs`
- Modify: `lib/l1_sender/src/lib.rs` (add `mod watcher;`)

- [ ] **Step 1: Create `watcher.rs` with the struct and `run()` loop**

Create `lib/l1_sender/src/watcher.rs` with this content:

```rust
use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::SendToL1;
use crate::error::{L1SendError, RecoverableReason};
use crate::lib::{InFlightTx, reason_label, validate_tx_receipt_reverted};
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use alloy::network::Ethereum;
use alloy::providers::{PendingTransactionError, Provider, WatchTxError};
use alloy::primitives::B256;
use alloy::rpc::types::TransactionReceipt;
use futures::stream::FuturesOrdered;
use futures::StreamExt;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use zksync_os_observability::ComponentStateHandle;

// ==============================================================================
// Watcher
// ==============================================================================

/// Watches in-flight L1 transactions for inclusion, using `FuturesOrdered` to
/// poll all receipt futures concurrently while delivering results in nonce order.
///
/// On a confirmed receipt the command is forwarded downstream. On timeout or
/// transient polling error the command is sent back to the Submitter via the
/// resubmit channel for resubmission. Fatal errors (tx revert) return `Err`.
pub(crate) struct Watcher<Input, P>
where
    Input: SendToL1,
    P: Provider<Ethereum>,
{
    pub in_flight_rx: mpsc::Receiver<InFlightTx<Input>>,
    pub resubmit_tx: mpsc::Sender<Input>,
    pub outbound: mpsc::Sender<SignedBatchEnvelope<FriProof>>,
    pub provider: P,
    pub latency_tracker: ComponentStateHandle<L1SenderState>,
}

/// The resolved output of one receipt future: the original command, its tx hash
/// (for logging even after the future is consumed), and the polling result.
type WatchResult<Input> = (Input, B256, Result<TransactionReceipt, PendingTransactionError>);

impl<Input, P> Watcher<Input, P>
where
    Input: SendToL1 + Send + 'static,
    P: Provider<Ethereum> + Clone,
{
    pub async fn run(mut self) -> anyhow::Result<()> {
        // All in-flight receipt futures are polled here concurrently.
        // FuturesOrdered yields results in submission (nonce) order.
        let mut pending: FuturesOrdered<Pin<Box<dyn Future<Output = WatchResult<Input>> + Send>>> =
            FuturesOrdered::new();

        loop {
            tokio::select! {
                // A new in-flight tx arrived from the Submitter.
                result = self.in_flight_rx.recv() => {
                    match result {
                        Some(tx) => {
                            self.latency_tracker.enter_state(L1SenderState::WaitingL1Inclusion);
                            pending.push(Box::pin(async move {
                                let receipt_result = tx.receipt_future.await;
                                (tx.command, tx.tx_hash, receipt_result)
                            }));
                        }
                        None => {
                            // Submitter died — drain any futures that are already in flight.
                            break;
                        }
                    }
                }
                // The next receipt resolved (in nonce order).
                Some(watch_result) = pending.next(), if !pending.is_empty() => {
                    let (command, tx_hash, receipt_result) = watch_result;
                    self.handle_receipt(command, tx_hash, receipt_result).await?;
                }
            }
        }

        // Drain remaining futures after the Submitter's channel closed.
        while let Some((command, tx_hash, receipt_result)) = pending.next().await {
            self.handle_receipt(command, tx_hash, receipt_result).await?;
        }

        Ok(())
    }

    async fn handle_receipt(
        &mut self,
        command: Input,
        tx_hash: B256,
        result: Result<TransactionReceipt, PendingTransactionError>,
    ) -> anyhow::Result<()> {
        match result {
            Ok(receipt) if receipt.status() => {
                // Tx confirmed — report metrics and forward downstream.
                self.latency_tracker.enter_state(L1SenderState::WaitingSend);
                L1_SENDER_METRICS.report_tx_receipt(&command, receipt);
                for mut envelope in command.into() {
                    envelope.set_stage(Input::MINED_STAGE);
                    self.outbound
                        .send(envelope)
                        .await
                        .map_err(|e| anyhow::anyhow!("outbound channel closed: {e}"))?;
                }
            }
            Ok(receipt) => {
                // Tx reverted on L1 — fatal, gas already burned.
                validate_tx_receipt_reverted(&self.provider, &command, receipt)
                    .await
                    .map_err(L1SendError::Fatal)?;
                unreachable!("validate_tx_receipt_reverted always returns Err");
            }
            Err(PendingTransactionError::TxWatcher(WatchTxError::Timeout)) => {
                // Receipt future timed out. Re-queue the command for resubmission.
                tracing::warn!(
                    ?tx_hash,
                    command_name = Input::NAME,
                    "tx timed out waiting for inclusion, re-queuing for resubmission"
                );
                L1_SENDER_METRICS.recoverable_errors[&reason_label(RecoverableReason::TxTimeout)]
                    .inc();
                self.resubmit_tx
                    .send(command)
                    .await
                    .map_err(|_| anyhow::anyhow!("resubmit channel closed (Submitter died)"))?;
            }
            Err(e) => {
                // Transient receipt polling error — re-queue for resubmission.
                tracing::warn!(
                    ?tx_hash,
                    ?e,
                    command_name = Input::NAME,
                    "transient error polling receipt, re-queuing for resubmission"
                );
                L1_SENDER_METRICS.transient_errors.inc();
                self.resubmit_tx
                    .send(command)
                    .await
                    .map_err(|_| anyhow::anyhow!("resubmit channel closed (Submitter died)"))?;
            }
        }
        Ok(())
    }
}
```

- [ ] **Step 2: Add `mod watcher;` to `lib.rs`**

In `lib/l1_sender/src/lib.rs`, add after the existing `mod` declarations at the top:

```rust
mod watcher;
```

- [ ] **Step 3: Verify the new module compiles**

```bash
cargo check -p zksync_os_l1_sender 2>&1
```

Fix any import errors. The file references `crate::lib::*` which will need to be adjusted once `lib.rs` is cleaned up — for now, just get the module to be accepted by the compiler. If `validate_tx_receipt_reverted` is not yet accessible, add `pub(crate)` to it in `lib.rs`.

- [ ] **Step 4: Commit**

```bash
git add lib/l1_sender/src/watcher.rs lib/l1_sender/src/lib.rs
git commit -m "feat(l1_sender): add Watcher task with FuturesOrdered receipt polling"
```

---

## Task 3: Create `submitter.rs`

The Submitter contains all the tx-building and submission logic currently in `L1SenderLoop::send_pending` and `L1SenderLoop::receive`.

**Files:**
- Create: `lib/l1_sender/src/submitter.rs`
- Modify: `lib/l1_sender/src/lib.rs` (add `mod submitter;`)

- [ ] **Step 1: Create `submitter.rs`**

Create `lib/l1_sender/src/submitter.rs`:

```rust
use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderConfig;
use crate::error::{L1SendError, RecoverableReason};
use crate::lib::{
    ExponentialBackoff, InFlightTx, reason_label, report_balance_and_nonce,
    validate_tx_receipt_reverted,
};
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use crate::watcher::Watcher;
use alloy::consensus::BlobTransactionValidationError;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::eips::{BlockId, Encodable2718};
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder, TransactionBuilder4844};
use alloy::primitives::{Address, B256};
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{PendingTransactionError, Provider, WalletProvider, WatchTxError};
use alloy::rpc::types::TransactionRequest;
use futures::FutureExt;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_observability::ComponentStateHandle;
use zksync_os_pipeline::PeekableReceiver;

const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(300);

// ==============================================================================
// Submitter
// ==============================================================================

/// Reads commands from the upstream channel and the resubmit channel, estimates
/// L1 fees, builds and submits transactions, and forwards `InFlightTx` items to
/// the `Watcher`.
///
/// The Submitter is the only task that holds a reference to the L1 provider.
/// All resubmission (after timeout or transient receipt error) comes back through
/// the `resubmit_rx` channel, which is checked before blocking on the upstream.
pub(crate) struct Submitter<Input, F, P>
where
    Input: SendToL1,
    F: TxFiller<Ethereum>,
    P: Provider<Ethereum>,
{
    pub inbound: PeekableReceiver<L1SenderCommand<Input>>,
    pub resubmit_rx: mpsc::Receiver<Input>,
    pub in_flight_tx: mpsc::Sender<InFlightTx<Input>>,
    pub provider: FillProvider<F, P>,
    pub config: L1SenderConfig<Input>,
    pub to_address: Address,
    pub operator_address: Address,
    pub gateway: bool,
    pub pending_commands: Vec<Input>,
    pub latency_tracker: ComponentStateHandle<L1SenderState>,
    pub backoff: ExponentialBackoff,
    pub cmd_buffer: Vec<L1SenderCommand<Input>>,
}

impl<Input, F, P> Submitter<Input, F, P>
where
    Input: SendToL1,
    F: TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
    P: Provider<Ethereum> + Clone + 'static,
{
    // ==============================================================================
    // Main loop
    // ==============================================================================

    pub async fn run(mut self) -> anyhow::Result<()> {
        let command_name = Input::NAME;
        loop {
            // Wait for commands if there is nothing pending.
            // Resubmit commands (from Watcher) are prioritised over new upstream commands.
            if self.pending_commands.is_empty() {
                match self.receive().await {
                    Ok(()) => {}
                    Err(e) => return Err(e.into_anyhow()),
                }
            }

            match self.send_pending().await {
                Ok(()) => {
                    report_balance_and_nonce::<_, Input>(&self.provider, self.operator_address)
                        .await;
                    self.backoff.reset();
                }
                Err(L1SendError::Fatal(e)) => return Err(e),
                Err(L1SendError::Transient(e)) => {
                    self.handle_transient(e, "during send").await;
                }
                Err(L1SendError::Recoverable { reason, source }) => {
                    self.handle_recoverable(reason, source, "during send").await;
                }
            }
        }
    }

    // ==============================================================================
    // Receive
    // ==============================================================================

    /// Fills `pending_commands` from either the resubmit channel (priority) or
    /// the upstream inbound channel. Blocks until at least one command is available.
    async fn receive(&mut self) -> Result<(), L1SendError> {
        self.latency_tracker.enter_state(L1SenderState::WaitingRecv);

        // Drain any already-queued resubmit commands without blocking.
        let mut got_resubmit = false;
        while let Ok(cmd) = self.resubmit_rx.try_recv() {
            self.pending_commands.push(cmd);
            got_resubmit = true;
        }
        if got_resubmit {
            return Ok(());
        }

        // Block on whichever channel fires first, preferring resubmit.
        tokio::select! {
            biased;
            Some(cmd) = self.resubmit_rx.recv() => {
                self.pending_commands.push(cmd);
                // Drain any additional resubmits that arrived concurrently.
                while let Ok(cmd) = self.resubmit_rx.try_recv() {
                    self.pending_commands.push(cmd);
                }
            }
            received = self.inbound.recv_many(&mut self.cmd_buffer, self.config.command_limit) => {
                if received == 0 {
                    return Err(L1SendError::Fatal(anyhow::anyhow!("inbound channel closed")));
                }
                let range = Input::display_range(
                    &self.cmd_buffer.iter()
                        .filter_map(|c| if let L1SenderCommand::SendToL1(c) = c { Some(c) } else { None })
                        .collect::<Vec<_>>()
                );
                for cmd in self.cmd_buffer.drain(..) {
                    match cmd {
                        L1SenderCommand::SendToL1(c) => self.pending_commands.push(c),
                        L1SenderCommand::Passthrough(batch) => {
                            return Err(L1SendError::Fatal(anyhow::anyhow!(
                                "Unexpected passthrough command for batch {:?}. \
                                 No passthrough commands are expected after the first `SendToL1`.",
                                batch.batch_number()
                            )));
                        }
                    }
                }
                tracing::info!(
                    command_name = Input::NAME,
                    range,
                    count = self.pending_commands.len(),
                    "received commands from upstream"
                );
                L1_SENDER_METRICS.parallel_transactions[&Input::NAME]
                    .set(self.pending_commands.len() as u64);
            }
        }
        Ok(())
    }

    // ==============================================================================
    // Send pending
    // ==============================================================================

    /// Submits each pending command as an L1 transaction, sending each
    /// `InFlightTx` to the Watcher on success. Partial progress is preserved:
    /// commands that have been sent move out of `pending_commands` one at a time.
    async fn send_pending(&mut self) -> Result<(), L1SendError> {
        self.latency_tracker.enter_state(L1SenderState::SendingToL1);

        // Estimate EIP-1559 fees once for the whole batch.
        let eip1559_est = self
            .provider
            .estimate_eip1559_fees()
            .await
            .map_err(|e| L1SendError::Transient(anyhow::Error::from(e)))?;

        L1_SENDER_METRICS.report_l1_eip_1559_estimation(eip1559_est);

        if eip1559_est.max_fee_per_gas > self.config.max_fee_per_gas_wei {
            return Err(L1SendError::Recoverable {
                reason: RecoverableReason::GasBlocked,
                source: anyhow::anyhow!(
                    "network max_fee_per_gas {} exceeds configured cap {}",
                    eip1559_est.max_fee_per_gas,
                    self.config.max_fee_per_gas_wei
                ),
            });
        }

        let max_fee_per_gas = eip1559_est.max_fee_per_gas;
        let max_priority_fee_per_gas = eip1559_est
            .max_priority_fee_per_gas
            .min(self.config.max_priority_fee_per_gas_wei);

        while !self.pending_commands.is_empty() {
            let cmd = &self.pending_commands[0];

            let mut tx_request = TransactionRequest::default()
                .with_from(self.operator_address)
                .with_to(self.to_address)
                .with_input(cmd.solidity_call(self.gateway, &self.operator_address))
                .with_max_fee_per_gas(max_fee_per_gas)
                .with_max_priority_fee_per_gas(max_priority_fee_per_gas)
                .with_gas_limit(15_000_000);

            if let Some(blob_sidecar) = cmd.blob_sidecar() {
                let fee_per_blob_gas = self
                    .provider
                    .get_blob_base_fee()
                    .await
                    .map_err(|e| L1SendError::Transient(anyhow::Error::from(e)))?;

                L1_SENDER_METRICS.report_blob_base_fee(fee_per_blob_gas);

                if fee_per_blob_gas > self.config.max_fee_per_blob_gas_wei {
                    return Err(L1SendError::Recoverable {
                        reason: RecoverableReason::BlobFeeBlocked,
                        source: anyhow::anyhow!(
                            "blob base fee {} exceeds configured cap {}",
                            fee_per_blob_gas,
                            self.config.max_fee_per_blob_gas_wei
                        ),
                    });
                }

                tx_request.set_max_fee_per_blob_gas(self.config.max_fee_per_blob_gas_wei);
                tx_request.set_blob_sidecar(blob_sidecar);
            }

            let envelope = self
                .provider
                .fill(tx_request)
                .await
                .map_err(|e| L1SendError::Transient(anyhow::Error::from(e)))?
                .try_into_envelope()
                .map_err(|e| L1SendError::Fatal(anyhow::Error::from(e)))?
                .try_into_pooled()
                .map_err(|e| L1SendError::Fatal(anyhow::Error::from(e)))?;

            // Fetch the pending block to decide whether to use EIP-7594 blob format.
            let pending_block = self
                .provider
                .get_block(BlockId::pending())
                .await
                .map_err(|e| L1SendError::Transient(anyhow::Error::from(e)))?;
            let block = match pending_block {
                Some(b) => b,
                None => self
                    .provider
                    .get_block(BlockId::latest())
                    .await
                    .map_err(|e| L1SendError::Transient(anyhow::Error::from(e)))?
                    .ok_or_else(|| {
                        L1SendError::Transient(anyhow::anyhow!(
                            "no pending or latest block available"
                        ))
                    })?,
            };

            let tx = if self.config.fusaka_upgrade_timestamp <= block.header.timestamp {
                envelope
                    .try_map_eip4844(|tx| {
                        tx.try_map_sidecar(|sidecar| {
                            Ok::<_, BlobTransactionValidationError>(
                                BlobTransactionSidecarVariant::Eip7594(sidecar.try_into_eip7594()?),
                            )
                        })
                    })
                    .map_err(|e| L1SendError::Fatal(anyhow::anyhow!("{e:?}")))?
            } else {
                envelope
            };

            let pending_builder = self
                .provider
                .send_raw_transaction(&tx.encoded_2718())
                .await
                .map_err(|e| L1SendError::classify_send_raw_error(anyhow::Error::from(e)))?;

            let tx_hash = *pending_builder.tx_hash();
            let receipt_future = pending_builder
                .with_required_confirmations(1)
                .with_timeout(Some(TRANSACTION_TIMEOUT))
                .get_receipt()
                .boxed();

            // Command successfully submitted — move it to in_flight via the Watcher channel.
            let mut cmd = self.pending_commands.remove(0);
            cmd.as_mut()
                .iter_mut()
                .for_each(|envelope| envelope.set_stage(Input::SENT_STAGE));

            self.in_flight_tx
                .send(InFlightTx { command: cmd, tx_hash, receipt_future })
                .await
                .map_err(|_| L1SendError::Fatal(anyhow::anyhow!("in_flight channel closed (Watcher died)")))?;
        }

        Ok(())
    }

    // ==============================================================================
    // Error helpers
    // ==============================================================================

    async fn handle_transient(&mut self, e: anyhow::Error, context: &str) {
        tracing::warn!(
            ?e,
            command_name = Input::NAME,
            "transient error {context}, entering backoff"
        );
        L1_SENDER_METRICS.transient_errors.inc();
        self.latency_tracker.enter_state(L1SenderState::TransientBackoff);
        let delay = self.backoff.next();
        tokio::time::sleep(delay).await;
    }

    async fn handle_recoverable(
        &mut self,
        reason: RecoverableReason,
        source: anyhow::Error,
        context: &str,
    ) {
        let state = match reason {
            RecoverableReason::GasBlocked => L1SenderState::GasBlocked,
            RecoverableReason::BlobFeeBlocked => L1SenderState::BlobFeeBlocked,
            _ => L1SenderState::TransientBackoff,
        };
        tracing::warn!(
            ?source,
            ?reason,
            command_name = Input::NAME,
            "recoverable error {context}"
        );
        L1_SENDER_METRICS.recoverable_errors[&reason_label(reason)].inc();
        self.latency_tracker.enter_state(state);
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}
```

- [ ] **Step 2: Add `mod submitter;` to `lib.rs`**

In `lib/l1_sender/src/lib.rs`, add after the other `mod` declarations:

```rust
mod submitter;
```

- [ ] **Step 3: Verify it compiles**

```bash
cargo check -p zksync_os_l1_sender 2>&1
```

Expect import errors for `crate::lib::*` functions that don't exist yet — those get resolved in Task 4.

- [ ] **Step 4: Commit**

```bash
git add lib/l1_sender/src/submitter.rs lib/l1_sender/src/lib.rs
git commit -m "feat(l1_sender): add Submitter task with backoff and resubmit support"
```

---

## Task 4: Update `lib.rs` — wire tasks, remove `L1SenderLoop`

**Files:**
- Modify: `lib/l1_sender/src/lib.rs`

- [ ] **Step 1: Replace the contents of `lib.rs`**

Replace `lib/l1_sender/src/lib.rs` entirely with the following. This keeps `ExponentialBackoff`, `InFlightTx`, and all helper free functions; removes `L1SenderLoop`; and updates `run_l1_sender` to create channels and join both tasks.

```rust
pub mod batcher_metrics;
pub mod batcher_model;
pub mod commands;
pub mod config;
pub mod error;
mod metrics;
pub mod pipeline_component;
mod submitter;
mod watcher;
pub mod upgrade_gatekeeper;

pub use error::{L1SendError, RecoverableReason};

use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderConfig;
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use crate::submitter::Submitter;
use crate::watcher::Watcher;
use alloy::consensus::BlobTransactionValidationError;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::eips::{BlockId, Encodable2718};
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder, TransactionBuilder4844};
use alloy::primitives::utils::format_ether;
use alloy::primitives::{Address, B256};
use alloy::providers::ext::DebugApi;
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{PendingTransactionError, Provider, WalletProvider, WatchTxError};
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use futures::FutureExt;
use futures::future::BoxFuture;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_observability::{ComponentStateHandle, ComponentStateReporter};
use zksync_os_operator_signer::SignerConfig;
use zksync_os_pipeline::PeekableReceiver;

/// Maximum time to wait for a transaction to be included on L1.
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(300);

/// Future that resolves into a (fallible) transaction receipt.
pub(crate) type TransactionReceiptFuture =
    BoxFuture<'static, Result<TransactionReceipt, PendingTransactionError>>;

// ==============================================================================
// Exponential Backoff
// ==============================================================================

/// Simple exponential backoff with a configurable initial delay, multiplier, and cap.
pub(crate) struct ExponentialBackoff {
    initial: Duration,
    current: Duration,
    max: Duration,
}

impl ExponentialBackoff {
    pub(crate) fn new(initial: Duration, max: Duration) -> Self {
        Self { initial, current: initial, max }
    }

    /// Returns the current delay and doubles it (capped at `max`) for the next call.
    pub(crate) fn next(&mut self) -> Duration {
        let delay = self.current;
        self.current = std::cmp::min(self.current * 2, self.max);
        delay
    }

    /// Resets the delay to the initial value after a successful cycle.
    pub(crate) fn reset(&mut self) {
        self.current = self.initial;
    }
}

// ==============================================================================
// InFlightTx
// ==============================================================================

/// A transaction that has been submitted to L1 but not yet confirmed.
///
/// `tx_hash` is tracked separately from the receipt future so that if the future
/// times out (and is consumed), the hash is still available for logging.
pub(crate) struct InFlightTx<Input> {
    pub command: Input,
    pub tx_hash: B256,
    pub receipt_future: TransactionReceiptFuture,
}

// ==============================================================================
// Public entry point
// ==============================================================================

/// Runs the L1 sender for one command type (commit, prove, or execute).
///
/// Handles operator registration and passthrough commands before spawning the
/// Submitter and Watcher tasks under `tokio::try_join!`.
pub async fn run_l1_sender<Input: SendToL1>(
    mut inbound: PeekableReceiver<L1SenderCommand<Input>>,
    outbound: mpsc::Sender<SignedBatchEnvelope<FriProof>>,
    to_address: Address,
    mut provider: FillProvider<
        impl TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
        impl Provider<Ethereum> + Clone + 'static,
    >,
    config: L1SenderConfig<Input>,
    gateway: bool,
) -> anyhow::Result<()> {
    let latency_tracker =
        ComponentStateReporter::global().handle_for(Input::NAME, L1SenderState::WaitingRecv);

    let operator_address =
        register_operator::<_, Input>(&mut provider, config.operator_signer.clone()).await?;

    if process_prepending_passthrough_commands(
        &mut inbound,
        &outbound,
        &latency_tracker,
        Input::NAME,
    )
    .await?
    .is_none()
    {
        tracing::info!(
            command_name = Input::NAME,
            "inbound channel closed during passthrough phase"
        );
        return Ok(());
    }

    let channel_capacity = config.command_limit;
    let cmd_buffer_capacity = config.command_limit;

    let (in_flight_tx, in_flight_rx) = mpsc::channel(channel_capacity);
    let (resubmit_tx, resubmit_rx) = mpsc::channel(channel_capacity);

    // The Watcher gets a provider clone only for debug-tracing reverted txs.
    let watcher_provider = provider.clone();

    let submitter = Submitter {
        inbound,
        resubmit_rx,
        in_flight_tx,
        provider,
        config,
        to_address,
        operator_address,
        gateway,
        pending_commands: Vec::new(),
        latency_tracker: latency_tracker.clone(),
        backoff: ExponentialBackoff::new(Duration::from_secs(5), Duration::from_secs(60)),
        cmd_buffer: Vec::with_capacity(cmd_buffer_capacity),
    };

    let watcher = Watcher {
        in_flight_rx,
        resubmit_tx,
        outbound,
        provider: watcher_provider,
        latency_tracker,
    };

    tokio::try_join!(submitter.run(), watcher.run())?;
    Ok(())
}

// ==============================================================================
// Helper free functions (shared between submitter.rs and watcher.rs)
// ==============================================================================

/// Converts a `RecoverableReason` to the Prometheus label string.
pub(crate) fn reason_label(reason: RecoverableReason) -> &'static str {
    match reason {
        RecoverableReason::GasBlocked => "gas_blocked",
        RecoverableReason::BlobFeeBlocked => "blob_fee_blocked",
        RecoverableReason::TxTimeout => "tx_timeout",
        RecoverableReason::NonceTooLow => "nonce_too_low",
    }
}

/// Reports operator balance and nonce after a successful send cycle.
/// RPC failures are logged at WARN and do not propagate.
pub(crate) async fn report_balance_and_nonce<P: Provider, Input: SendToL1>(
    provider: &P,
    operator_address: Address,
) {
    match provider.get_balance(operator_address).await {
        Ok(balance) => {
            let balance_str = format_ether(balance);
            if let Ok(v) = balance_str.parse::<f64>() {
                L1_SENDER_METRICS.balance[&Input::NAME].set(v);
            } else {
                tracing::warn!("failed to parse balance for metrics");
            }
            tracing::info!(
                command_name = Input::NAME,
                balance = balance_str,
                "operator balance after send cycle"
            );
        }
        Err(e) => tracing::warn!(?e, "failed to fetch operator balance"),
    }

    match provider.get_transaction_count(operator_address).await {
        Ok(nonce) => {
            L1_SENDER_METRICS.nonce[&Input::NAME].set(nonce);
        }
        Err(e) => tracing::warn!(?e, "failed to fetch operator nonce"),
    }
}

pub(crate) async fn process_prepending_passthrough_commands<Input: SendToL1>(
    inbound: &mut PeekableReceiver<L1SenderCommand<Input>>,
    outbound: &mpsc::Sender<SignedBatchEnvelope<FriProof>>,
    latency_tracker: &ComponentStateHandle<L1SenderState>,
    command_name: &str,
) -> anyhow::Result<Option<()>> {
    loop {
        latency_tracker.enter_state(L1SenderState::WaitingRecv);
        match inbound
            .peek_recv(|command| matches!(command, L1SenderCommand::Passthrough(_)))
            .await
        {
            None => return Ok(None),
            Some(false) => return Ok(Some(())),
            Some(true) => {
                let Some(next_command) = inbound.recv().await else {
                    return Ok(None);
                };
                match next_command {
                    L1SenderCommand::SendToL1(_) => {
                        anyhow::bail!("Mismatch between peeked and received command")
                    }
                    L1SenderCommand::Passthrough(batch) => {
                        tracing::info!(
                            command_name,
                            batch_number = batch.batch_number(),
                            "Not actually sending to L1, just passing through"
                        );
                        latency_tracker.enter_state(L1SenderState::WaitingSend);
                        outbound
                            .send((*batch).with_stage(Input::PASSTHROUGH_STAGE))
                            .await?;
                    }
                }
            }
        }
    }
}

pub(crate) async fn register_operator<
    P: Provider + WalletProvider<Wallet = EthereumWallet>,
    Input: SendToL1,
>(
    provider: &mut P,
    signer_config: SignerConfig,
) -> anyhow::Result<Address> {
    let address = signer_config
        .register_with_wallet(provider.wallet_mut())
        .await?;

    let balance = provider.get_balance(address).await?;
    if let Ok(v) = format_ether(balance).parse::<f64>() {
        L1_SENDER_METRICS.balance[&Input::NAME].set(v);
    } else {
        tracing::warn!("failed to parse operator balance for metrics");
    }
    let address_string: &'static str = address.to_string().leak();
    L1_SENDER_METRICS.l1_operator_address[&(Input::NAME, address_string)].set(1);

    if balance.is_zero() {
        anyhow::bail!("L1 sender's address {address} has zero balance");
    }

    tracing::info!(
        command_name = Input::NAME,
        balance_eth = format_ether(balance),
        %address,
        "initialized L1 sender",
    );
    Ok(address)
}

/// Logs full diagnostic info for a reverted L1 transaction and returns `Err`.
pub(crate) async fn validate_tx_receipt_reverted<Input: SendToL1>(
    provider: &impl Provider,
    command: &Input,
    receipt: TransactionReceipt,
) -> anyhow::Result<()> {
    tracing::error!(
        %command,
        tx_hash = ?receipt.transaction_hash,
        l1_block_number = receipt.block_number.unwrap(),
        "Transaction failed on L1",
    );
    if let Ok(trace) = provider
        .debug_trace_transaction(
            receipt.transaction_hash,
            GethDebugTracingOptions::call_tracer(CallConfig::default()),
        )
        .await
    {
        let call_frame = trace
            .try_into_call_frame()
            .expect("requested call tracer but received a different call frame type");
        tracing::error!(
            ?call_frame.output,
            ?call_frame.error,
            ?call_frame.revert_reason,
            "Failed transaction's top-level call frame"
        );
    }
    anyhow::bail!(
        "{} L1 command transaction failed, see L1 transaction's trace for more details (tx_hash='{:?}')",
        command,
        receipt.transaction_hash
    );
}
```

> **Note on `ComponentStateHandle::clone`:** If `ComponentStateHandle` does not implement `Clone`, replace `latency_tracker.clone()` with a second call to `ComponentStateReporter::global().handle_for(Input::NAME, L1SenderState::WaitingL1Inclusion)` for the Watcher. Check by running `cargo check`.

- [ ] **Step 2: Fix import paths in `submitter.rs` and `watcher.rs`**

The `use crate::lib::*` references in the earlier tasks need to be updated. In both files, replace `use crate::lib::...` with the actual module where the symbols now live — they are all defined in `lib.rs` (the crate root), so import them as `use crate::ExponentialBackoff`, `use crate::InFlightTx`, `use crate::validate_tx_receipt_reverted`, `use crate::reason_label`, `use crate::report_balance_and_nonce`.

Update `submitter.rs` imports:
```rust
use crate::{ExponentialBackoff, InFlightTx, reason_label, report_balance_and_nonce};
```

Update `watcher.rs` imports:
```rust
use crate::{InFlightTx, reason_label, validate_tx_receipt_reverted};
```

- [ ] **Step 3: Remove the old `TRANSACTION_TIMEOUT` from `submitter.rs` if you duplicated it**

`TRANSACTION_TIMEOUT` is now defined in `lib.rs` and accessible as `crate::TRANSACTION_TIMEOUT`. Remove the duplicate from `submitter.rs` and add `use crate::TRANSACTION_TIMEOUT;`.

- [ ] **Step 4: Verify compilation**

```bash
cargo check -p zksync_os_l1_sender 2>&1
```

Work through any remaining type errors. Common issues:
- `ComponentStateHandle` may not be `Clone` — see the note in Step 1.
- The `Provider` trait bound on `Watcher` may need to match whatever `validate_tx_receipt_reverted` requires. Since it takes `&impl Provider`, any `P: Provider` works.
- The `FuturesOrdered` item type — if the compiler complains, try adding `+ Send + 'static` to the future bound.

- [ ] **Step 5: Run linter**

```bash
cargo clippy -p zksync_os_l1_sender 2>&1
```

Fix all warnings.

- [ ] **Step 6: Run unit tests**

```bash
cargo nextest run --release -p zksync_os_l1_sender 2>&1
```

- [ ] **Step 7: Commit**

```bash
git add lib/l1_sender/src/lib.rs lib/l1_sender/src/submitter.rs lib/l1_sender/src/watcher.rs
git commit -m "feat(l1_sender): replace L1SenderLoop with Submitter + Watcher tasks"
```

---

## Task 5: Update `max_priority_fee_per_gas` default

**Files:**
- Modify: `node/bin/src/config/mod.rs`

- [ ] **Step 1: Change the default**

Find this line in `node/bin/src/config/mod.rs` (around line 489):

```rust
#[config(default_t = 1 * EtherUnit::Gwei)]
pub max_priority_fee_per_gas: EtherAmount,
```

Change to:

```rust
#[config(default_t = 10 * EtherUnit::Gwei)]
pub max_priority_fee_per_gas: EtherAmount,
```

- [ ] **Step 2: Verify**

```bash
cargo check -p zksync_os_bin 2>&1
```

- [ ] **Step 3: Commit**

```bash
git add node/bin/src/config/mod.rs
git commit -m "feat(config): raise max_priority_fee_per_gas default from 1 to 10 gwei"
```

---

## Task 6: Run full checks and integration tests

- [ ] **Step 1: Format**

```bash
cargo fmt --all --check 2>&1
```

If it fails, run `cargo fmt --all` then re-check.

- [ ] **Step 2: Full lint**

```bash
cargo clippy --all-targets --all-features --workspace -- -D warnings 2>&1
```

- [ ] **Step 3: Unit tests**

```bash
cargo nextest run --release --workspace --exclude zksync_os_integration_tests 2>&1
```

- [ ] **Step 4: Integration tests**

```bash
cargo nextest run -p zksync_os_integration_tests 2>&1
```

These tests manage their own L1/node — no live anvil needed. Expect them to exercise the full commit/prove/execute pipeline through the new Submitter+Watcher design.

- [ ] **Step 5: Final commit if any fmt fixes were needed**

```bash
git add -u && git commit -m "chore: apply fmt"
```

---

## Task 7: Open PR

- [ ] **Step 1: Push and open PR to `matter-labs/zksync-os-server`**

```bash
git push -u origin feat/l1-sender-task-decomp
gh pr create \
  --repo matter-labs/zksync-os-server \
  --base main \
  --title "feat(l1_sender): replace L1SenderLoop with Submitter + Watcher tasks" \
  --body "$(cat <<'EOF'
## Summary

- Replaces the single-loop `L1SenderLoop` with two independent async tasks: `Submitter` (provider interaction, tx submission) and `Watcher` (`FuturesOrdered` receipt polling, downstream forwarding)
- Eliminates phase-blocking: a submission error no longer prevents receipt checking for already in-flight txs; a receipt timeout no longer prevents forwarding already-confirmed txs
- Adds concurrent receipt polling via `FuturesOrdered` (previously sequential with `first_mut()`)
- Raises `max_priority_fee_per_gas` default from 1 gwei to 10 gwei

## Design doc

`docs/superpowers/specs/2026-03-27-l1-sender-task-decomposition.md`

## Testing

Existing integration tests cover the full commit/prove/execute pipeline.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```
