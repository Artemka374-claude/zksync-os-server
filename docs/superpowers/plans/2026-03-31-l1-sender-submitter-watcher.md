# L1 Sender Submitter + Watcher Split Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the monolithic `run_l1_sender` loop with a Submitter + Watcher split that retries transient failures, blocks on gas cap violations, and resubmits timed-out transactions — without crashing on recoverable errors.

**Architecture:** The entry-point `run_l1_sender` registers the operator, creates two internal channels (`in_flight` and `resubmit`), then drives a `Submitter` and `Watcher` concurrently via `try_join!`. The Submitter owns the inbound channel and all L1 send logic; the Watcher owns receipt polling and forwards confirmed commands downstream. A timed-out tx travels back from Watcher → Submitter via the `resubmit` channel; the Submitter decides whether to bump fees or re-watch the original hash.

**Tech Stack:** Rust, Tokio, Alloy (EIP-1559 / EIP-4844), `anyhow`, `tracing`, `cargo nextest`

---

## File Map

| Status | Path | Purpose |
|--------|------|---------|
| Create | `lib/l1_sender/src/types.rs` | `GasParams`, `InFlightTx`, `InFlightItem`, `ResubmitRequest`, `Backoff` |
| Create | `lib/l1_sender/src/submitter.rs` | `Submitter` struct, run loop, send/resubmit logic |
| Create | `lib/l1_sender/src/watcher.rs` | `Watcher` struct, receipt polling loop, validation |
| Modify | `lib/l1_sender/src/lib.rs` | thin entry point; declare new modules; delete dead code |
| Modify | `lib/l1_sender/src/config.rs` | add `transaction_timeout: Duration` field |

`validate_tx_receipt` stays in `lib.rs` as a `pub(crate)` helper used by the Watcher.
`tx_request_with_gas_fields` is deleted in Task 5 — the Submitter inlines that logic in `build_and_send`.

---

## Task 1: Shared types (`types.rs`)

**Files:**
- Create: `lib/l1_sender/src/types.rs`
- Modify: `lib/l1_sender/src/lib.rs` (add `mod types; pub(crate) use types::*;`)

- [ ] **Step 1 — Write failing tests**

Add at the bottom of `types.rs` (creating the file with just the test module first):

```rust
// lib/l1_sender/src/types.rs
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── GasParams::is_sufficient_replacement ──────────────────────────────────

    #[test]
    fn replacement_accepted_when_all_fees_at_exactly_110_percent() {
        let old = GasParams { max_fee_per_gas: 100, max_priority_fee_per_gas: 10, max_fee_per_blob_gas: None };
        let new = GasParams { max_fee_per_gas: 110, max_priority_fee_per_gas: 11, max_fee_per_blob_gas: None };
        assert!(old.is_sufficient_replacement(&new));
    }

    #[test]
    fn replacement_rejected_when_max_fee_below_threshold() {
        let old = GasParams { max_fee_per_gas: 100, max_priority_fee_per_gas: 10, max_fee_per_blob_gas: None };
        let new = GasParams { max_fee_per_gas: 109, max_priority_fee_per_gas: 11, max_fee_per_blob_gas: None };
        assert!(!old.is_sufficient_replacement(&new));
    }

    #[test]
    fn replacement_rejected_when_priority_fee_below_threshold() {
        let old = GasParams { max_fee_per_gas: 100, max_priority_fee_per_gas: 10, max_fee_per_blob_gas: None };
        let new = GasParams { max_fee_per_gas: 110, max_priority_fee_per_gas: 10, max_fee_per_blob_gas: None };
        assert!(!old.is_sufficient_replacement(&new));
    }

    #[test]
    fn replacement_accepted_for_blob_tx_when_blob_fee_also_sufficient() {
        let old = GasParams { max_fee_per_gas: 100, max_priority_fee_per_gas: 10, max_fee_per_blob_gas: Some(50) };
        let new = GasParams { max_fee_per_gas: 110, max_priority_fee_per_gas: 11, max_fee_per_blob_gas: Some(55) };
        assert!(old.is_sufficient_replacement(&new));
    }

    #[test]
    fn replacement_rejected_for_blob_tx_when_blob_fee_below_threshold() {
        let old = GasParams { max_fee_per_gas: 100, max_priority_fee_per_gas: 10, max_fee_per_blob_gas: Some(50) };
        let new = GasParams { max_fee_per_gas: 110, max_priority_fee_per_gas: 11, max_fee_per_blob_gas: Some(54) };
        assert!(!old.is_sufficient_replacement(&new));
    }

    #[test]
    fn replacement_rejected_when_new_has_no_blob_fee_but_old_does() {
        let old = GasParams { max_fee_per_gas: 100, max_priority_fee_per_gas: 10, max_fee_per_blob_gas: Some(50) };
        let new = GasParams { max_fee_per_gas: 110, max_priority_fee_per_gas: 11, max_fee_per_blob_gas: None };
        assert!(!old.is_sufficient_replacement(&new));
    }

    // ── Backoff ───────────────────────────────────────────────────────────────

    #[test]
    fn backoff_follows_doubling_sequence_capped_at_60s() {
        let mut b = Backoff::new();
        assert_eq!(b.current(), Duration::from_secs(5));
        b.advance(); assert_eq!(b.current(), Duration::from_secs(10));
        b.advance(); assert_eq!(b.current(), Duration::from_secs(20));
        b.advance(); assert_eq!(b.current(), Duration::from_secs(40));
        b.advance(); assert_eq!(b.current(), Duration::from_secs(60));
        b.advance(); assert_eq!(b.current(), Duration::from_secs(60), "must not exceed cap");
    }

    #[test]
    fn backoff_resets_to_initial_delay() {
        let mut b = Backoff::new();
        b.advance(); b.advance(); b.advance();
        b.reset();
        assert_eq!(b.current(), Duration::from_secs(5));
    }
}
```

- [ ] **Step 2 — Run to verify failure**

```bash
cargo nextest run -p zksync_os_l1_sender 2>&1 | head -30
```

Expected: compilation errors (types not defined yet).

- [ ] **Step 3 — Implement `types.rs`**

```rust
// lib/l1_sender/src/types.rs
use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use alloy::primitives::TxHash;
use std::time::Duration;

/// Fee parameters attached to every submitted L1 transaction.
///
/// Carried through the `in_flight` channel so the Watcher can pass them back
/// to the Submitter when requesting a resubmission — the Submitter then
/// compares them against freshly estimated fees to decide whether a
/// replacement transaction is warranted.
#[derive(Clone, Debug)]
pub(crate) struct GasParams {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    /// `None` for non-blob (EIP-1559) transactions.
    pub max_fee_per_blob_gas: Option<u128>,
}

impl GasParams {
    /// Returns `true` when `other` is a sufficient fee bump to replace `self`
    /// in the mempool — every dimension must be at least 110% of the current value.
    pub fn is_sufficient_replacement(&self, other: &GasParams) -> bool {
        let fee_ok = other.max_fee_per_gas >= self.max_fee_per_gas * 11 / 10
            && other.max_priority_fee_per_gas >= self.max_priority_fee_per_gas * 11 / 10;

        let blob_ok = match (self.max_fee_per_blob_gas, other.max_fee_per_blob_gas) {
            (Some(old), Some(new)) => new >= old * 11 / 10,
            // Non-blob tx: blob fee is irrelevant.
            (None, _) => true,
            // Old tx had a blob fee but new estimate doesn't — can't replace.
            (Some(_), None) => false,
        };

        fee_ok && blob_ok
    }
}

/// Flows from Submitter → Watcher through the `in_flight` channel.
pub(crate) enum InFlightItem<Input> {
    /// A transaction that has been submitted to L1 and needs confirmation.
    Tx(InFlightTx<Input>),
    /// A batch that was already committed on L1; the Watcher forwards it
    /// immediately without awaiting a receipt.
    Passthrough(Box<SignedBatchEnvelope<FriProof>>),
}

/// A submitted L1 transaction awaiting confirmation.
pub(crate) struct InFlightTx<Input> {
    pub tx_hash: TxHash,
    pub gas_params: GasParams,
    /// Original command — kept so the Submitter can rebuild calldata on
    /// resubmission, and so the Watcher can forward it downstream on
    /// confirmation.
    pub command: Input,
    /// Nonce used when submitting this tx — required to issue a replacement
    /// transaction with the same nonce (EIP-1559 replacement rules).
    pub nonce: u64,
}

/// Sent from Watcher → Submitter when a tx confirmation times out.
pub(crate) struct ResubmitRequest<Input> {
    pub original_tx_hash: TxHash,
    pub original_gas_params: GasParams,
    pub command: Input,
    pub nonce: u64,
}

/// Exponential backoff for transient errors.
///
/// Delay sequence: 5 s → 10 s → 20 s → 40 s → 60 s (capped).
/// Call `reset()` after a successful operation to start from 5 s again.
pub(crate) struct Backoff {
    current: Duration,
}

impl Backoff {
    const INITIAL: Duration = Duration::from_secs(5);
    const MAX: Duration = Duration::from_secs(60);

    pub fn new() -> Self {
        Self { current: Self::INITIAL }
    }

    pub fn current(&self) -> Duration {
        self.current
    }

    /// Sleep for the current delay.
    pub async fn wait(&self) {
        tokio::time::sleep(self.current).await;
    }

    /// Double the delay, capped at `MAX`.
    pub fn advance(&mut self) {
        self.current = (self.current * 2).min(Self::MAX);
    }

    /// Reset to the initial delay.
    pub fn reset(&mut self) {
        self.current = Self::INITIAL;
    }
}
```

- [ ] **Step 4 — Declare the module in `lib.rs`**

Add at the top of `lib/l1_sender/src/lib.rs`, alongside the existing `mod` declarations:

```rust
mod types;
pub(crate) use types::{Backoff, GasParams, InFlightItem, InFlightTx, ResubmitRequest};
```

- [ ] **Step 5 — Run tests**

```bash
cargo nextest run -p zksync_os_l1_sender 2>&1
```

Expected: all tests in `types::tests` pass.

- [ ] **Step 6 — Commit**

```bash
git add lib/l1_sender/src/types.rs lib/l1_sender/src/lib.rs
git commit -m "feat(l1_sender): add shared channel types and Backoff"
```

---

## Task 2: Error classification (`error.rs` + `config.rs`)

**Files:**
- Create: `lib/l1_sender/src/error.rs`
- Modify: `lib/l1_sender/src/lib.rs` (add `mod error; pub(crate) use error::*;`)
- Modify: `lib/l1_sender/src/config.rs` (add `transaction_timeout` field)

- [ ] **Step 1 — Add `transaction_timeout` to `L1SenderConfig`**

In `config.rs`, add the new field (place it after `poll_interval`):

```rust
/// Maximum time to wait for a transaction to be included on L1 before
/// triggering a resubmission.  Defaults to 300 s in production; tests use a
/// shorter value to exercise the resubmission path without waiting.
pub transaction_timeout: Duration,
```

Update every place that constructs `L1SenderConfig` (search: `L1SenderConfig {`) to add
`transaction_timeout: Duration::from_secs(300)`.

Run `cargo build` to find all call sites:

```bash
cargo build -p zksync_os_l1_sender 2>&1 | grep "missing field"
```

- [ ] **Step 2 — Write failing tests for `error.rs`**

Create `lib/l1_sender/src/error.rs` with only the test module:

```rust
// lib/l1_sender/src/error.rs
#[cfg(test)]
mod tests {
    use super::*;
    use alloy::transports::{RpcError, TransportErrorKind};

    fn transport_err() -> anyhow::Error {
        anyhow::Error::new(RpcError::<TransportErrorKind>::Transport(
            TransportErrorKind::backend_gone(),
        ))
    }

    fn rpc_error_resp(message: &str) -> anyhow::Error {
        use alloy::rpc::json_rpc::{ErrorPayload, Id};
        anyhow::Error::new(RpcError::<TransportErrorKind>::ErrorResp(ErrorPayload {
            code: -32000,
            message: message.to_string().into(),
            data: None,
            id: Id::None,
        }))
    }

    #[test]
    fn transport_error_is_transient() {
        assert!(is_transient(&transport_err()));
    }

    #[test]
    fn rpc_error_resp_is_not_transient() {
        assert!(!is_transient(&rpc_error_resp("some mempool rejection")));
    }

    #[test]
    fn nonce_too_low_message_detected() {
        assert!(is_nonce_too_low(&rpc_error_resp("nonce too low")));
    }

    #[test]
    fn nonce_too_low_case_insensitive() {
        assert!(is_nonce_too_low(&rpc_error_resp("Nonce Too Low")));
    }

    #[test]
    fn unrelated_rpc_error_is_not_nonce_too_low() {
        assert!(!is_nonce_too_low(&rpc_error_resp("execution reverted")));
    }

    #[test]
    fn transport_error_is_not_nonce_too_low() {
        assert!(!is_nonce_too_low(&transport_err()));
    }
}
```

- [ ] **Step 3 — Run to verify failure**

```bash
cargo nextest run -p zksync_os_l1_sender 2>&1 | head -20
```

Expected: compilation errors (functions not defined).

- [ ] **Step 4 — Implement `error.rs`**

```rust
// lib/l1_sender/src/error.rs

use alloy::transports::{RpcError, TransportErrorKind};

/// Returns `true` for transient infrastructure failures that are worth retrying
/// with backoff (network timeouts, provider restarts, rate limits).
///
/// Mempool rejections and other definitive L1-level errors return `false` and
/// should propagate as fatal errors.
pub(crate) fn is_transient(err: &anyhow::Error) -> bool {
    if let Some(rpc) = err.downcast_ref::<RpcError<TransportErrorKind>>() {
        return matches!(rpc, RpcError::Transport(_));
    }
    false
}

/// Returns `true` when the provider rejected the transaction because our nonce
/// is already used — a prior tx was mined between our nonce fetch and our send.
///
/// Detection is message-based because the EVM error code (-32000) is shared
/// across many error classes.
pub(crate) fn is_nonce_too_low(err: &anyhow::Error) -> bool {
    if let Some(rpc) = err.downcast_ref::<RpcError<TransportErrorKind>>() {
        if let RpcError::ErrorResp(payload) = rpc {
            return payload.message.to_ascii_lowercase().contains("nonce too low");
        }
    }
    false
}

#[cfg(test)]
mod tests {
    // ... (test module written in step 2)
}
```

- [ ] **Step 5 — Declare module in `lib.rs`**

```rust
mod error;
pub(crate) use error::{is_nonce_too_low, is_transient};
```

- [ ] **Step 6 — Run tests**

```bash
cargo nextest run -p zksync_os_l1_sender 2>&1
```

Expected: all tests pass (types + error).

- [ ] **Step 7 — Commit**

```bash
git add lib/l1_sender/src/error.rs lib/l1_sender/src/lib.rs lib/l1_sender/src/config.rs
git commit -m "feat(l1_sender): add error classification helpers and configurable tx timeout"
```

---

## Task 3: Submitter (`submitter.rs`)

**Files:**
- Create: `lib/l1_sender/src/submitter.rs`
- Modify: `lib/l1_sender/src/lib.rs` (add `mod submitter; pub(crate) use submitter::Submitter;`)

### Resubmission decision test first

- [ ] **Step 1 — Write failing unit tests for resubmission action**

Create `lib/l1_sender/src/submitter.rs` with only the test module. The tests cover the decision point in `handle_resubmit`: given old gas params and fresh estimates, do we submit a replacement or re-watch the original?

```rust
// lib/l1_sender/src/submitter.rs

/// Outcome returned by `resubmission_action`.
#[derive(Debug, PartialEq)]
pub(crate) enum ResubmitAction {
    /// Fees rose enough — send a replacement tx with the same nonce.
    SendReplacement,
    /// Fees have not risen enough — re-watch the original tx hash.
    RewatchOriginal,
}

/// Pure decision function: given old and freshly-estimated gas params, decide
/// whether to submit a replacement transaction or re-watch the original hash.
pub(crate) fn resubmission_action(old: &GasParams, fresh_estimate: &GasParams) -> ResubmitAction {
    if old.is_sufficient_replacement(fresh_estimate) {
        ResubmitAction::SendReplacement
    } else {
        ResubmitAction::RewatchOriginal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::GasParams;

    fn params(fee: u128, priority: u128) -> GasParams {
        GasParams { max_fee_per_gas: fee, max_priority_fee_per_gas: priority, max_fee_per_blob_gas: None }
    }

    fn blob_params(fee: u128, priority: u128, blob: u128) -> GasParams {
        GasParams { max_fee_per_gas: fee, max_priority_fee_per_gas: priority, max_fee_per_blob_gas: Some(blob) }
    }

    // ── Strategy A: new_fee >= old_fee * 1.1 ─────────────────────────────────

    #[test]
    fn sends_replacement_when_fees_sufficiently_higher() {
        let old = params(100, 10);
        let fresh = params(120, 15);
        assert_eq!(resubmission_action(&old, &fresh), ResubmitAction::SendReplacement);
    }

    #[test]
    fn rewatches_original_when_fees_not_risen_enough() {
        let old = params(100, 10);
        let fresh = params(105, 11); // base fee only rose 5%, not 10%
        assert_eq!(resubmission_action(&old, &fresh), ResubmitAction::RewatchOriginal);
    }

    #[test]
    fn rewatches_original_when_fees_unchanged() {
        let old = params(100, 10);
        assert_eq!(resubmission_action(&old, &old.clone()), ResubmitAction::RewatchOriginal);
    }

    #[test]
    fn sends_replacement_for_blob_tx_when_all_fees_sufficient() {
        let old = blob_params(100, 10, 50);
        let fresh = blob_params(110, 11, 55);
        assert_eq!(resubmission_action(&old, &fresh), ResubmitAction::SendReplacement);
    }

    #[test]
    fn rewatches_original_for_blob_tx_when_blob_fee_not_sufficient() {
        let old = blob_params(100, 10, 50);
        let fresh = blob_params(110, 11, 54); // blob fee only rose 8%
        assert_eq!(resubmission_action(&old, &fresh), ResubmitAction::RewatchOriginal);
    }

    #[test]
    fn resubmit_priority_set_correctly_fresh_params_are_used_not_old() {
        // Submitter must use fresh_estimate, not a copy of old params, when building
        // the replacement tx. This test ensures the decision reads the right variable.
        let old = params(100, 10);
        let stale_copy = params(100, 10); // same as old — should NOT replace
        assert_eq!(resubmission_action(&old, &stale_copy), ResubmitAction::RewatchOriginal);
        let bumped = params(115, 12); // above threshold — should replace
        assert_eq!(resubmission_action(&old, &bumped), ResubmitAction::SendReplacement);
    }
}
```

- [ ] **Step 2 — Run to verify failure**

```bash
cargo nextest run -p zksync_os_l1_sender submitter 2>&1 | head -20
```

Expected: compilation errors.

- [ ] **Step 3 — Implement `resubmission_action` and run tests**

Ensure the two items above the `#[cfg(test)]` block are present (`ResubmitAction` enum and `resubmission_action` function). The test module in Step 1 already shows the correct body — just make sure the file compiles.

```bash
cargo nextest run -p zksync_os_l1_sender 2>&1
```

Expected: all tests pass.

### Submitter implementation

- [ ] **Step 4 — Implement the full `Submitter`**

Replace the content of `submitter.rs` with the full implementation (keeping the test module):

```rust
// lib/l1_sender/src/submitter.rs

use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderConfig;
use crate::error::{is_nonce_too_low, is_transient};
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use crate::types::{Backoff, GasParams, InFlightItem, InFlightTx, ResubmitRequest};
use crate::validate_tx_receipt;
use alloy::consensus::BlobTransactionValidationError;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::eips::{BlockId, Encodable2718};
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder, TransactionBuilder4844};
use alloy::primitives::{Address, TxHash};
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{Provider, WalletProvider};
use alloy::rpc::types::TransactionRequest;
use tokio::sync::mpsc;
use zksync_os_observability::ComponentStateHandle;
use zksync_os_pipeline::PeekableReceiver;

/// Outcome returned by `resubmission_action`.
#[derive(Debug, PartialEq)]
pub(crate) enum ResubmitAction {
    SendReplacement,
    RewatchOriginal,
}

/// Pure decision function — determines whether fresh fees justify a replacement
/// transaction or whether we should re-watch the original hash.
pub(crate) fn resubmission_action(old: &GasParams, fresh_estimate: &GasParams) -> ResubmitAction {
    if old.is_sufficient_replacement(fresh_estimate) {
        ResubmitAction::SendReplacement
    } else {
        ResubmitAction::RewatchOriginal
    }
}

/// Responsible for all L1 submission logic:
///
/// - Reads commands from `inbound` (and resubmit requests from `resubmit_rx`,
///   which take priority).
/// - Estimates gas, enforces fee caps, and sends raw transactions.
/// - Puts each submitted transaction into `in_flight_tx` for the Watcher.
/// - On resubmission: either sends a replacement tx (same nonce, higher fees)
///   or re-wraps the original hash so the Watcher re-watches it.
pub(crate) struct Submitter<F, P, Input>
where
    F: TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
    P: Provider<Ethereum>,
    Input: SendToL1,
{
    pub inbound: PeekableReceiver<L1SenderCommand<Input>>,
    pub in_flight_tx: mpsc::Sender<InFlightItem<Input>>,
    pub resubmit_rx: mpsc::Receiver<ResubmitRequest<Input>>,
    pub to_address: Address,
    pub provider: FillProvider<F, P>,
    pub config: L1SenderConfig<Input>,
    pub gateway: bool,
    pub operator_address: Address,
    pub next_nonce: u64,
    pub latency_tracker: ComponentStateHandle<L1SenderState>,
}

impl<F, P, Input> Submitter<F, P, Input>
where
    F: TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
    P: Provider<Ethereum> + Clone,
    Input: SendToL1 + Send + 'static,
{
    pub async fn run(mut self) -> anyhow::Result<()> {
        loop {
            self.latency_tracker.enter_state(L1SenderState::WaitingRecv);

            // Resubmit requests take priority over new commands so that a
            // timed-out tx is replaced before the pipeline moves on.
            let work = tokio::select! {
                biased;
                resubmit = self.resubmit_rx.recv() => match resubmit {
                    Some(req) => Work::Resubmit(req),
                    None => { tracing::info!("resubmit channel closed"); return Ok(()); }
                },
                cmd = self.inbound.recv() => match cmd {
                    Some(c) => Work::New(c),
                    None => { tracing::info!(command_name = Input::NAME, "inbound channel closed"); return Ok(()); }
                },
            };

            match work {
                Work::New(L1SenderCommand::Passthrough(envelope)) => {
                    tracing::info!(
                        command_name = Input::NAME,
                        batch_number = envelope.batch_number(),
                        "Not actually sending to L1, just passing through",
                    );
                    self.in_flight_tx
                        .send(InFlightItem::Passthrough(envelope))
                        .await
                        .map_err(|_| anyhow::anyhow!("in_flight channel closed"))?;
                }
                Work::New(L1SenderCommand::SendToL1(cmd)) => {
                    self.submit_new(cmd).await?;
                }
                Work::Resubmit(req) => {
                    self.handle_resubmit(req).await?;
                }
            }
        }
    }

    /// Estimate gas params, enforce caps, build and send a new transaction.
    async fn submit_new(&mut self, mut command: Input) -> anyhow::Result<()> {
        self.latency_tracker.enter_state(L1SenderState::SendingToL1);
        let range = Input::display_range(std::slice::from_ref(&command));
        tracing::info!(command_name = Input::NAME, range, "sending L1 transaction");

        let gas_params = self.estimate_gas_within_caps().await?;
        let (tx_hash, nonce) = self.build_and_send(&command, &gas_params, None).await?;

        command.as_mut().iter_mut().for_each(|env| env.set_stage(Input::SENT_STAGE));

        self.in_flight_tx
            .send(InFlightItem::Tx(InFlightTx { tx_hash, gas_params, command, nonce }))
            .await
            .map_err(|_| anyhow::anyhow!("in_flight channel closed"))?;

        Ok(())
    }

    /// Handle a resubmission request from the Watcher (strategy A from the spec).
    ///
    /// If fresh estimates show a ≥10% fee bump: send a replacement transaction
    /// with the same nonce.  Otherwise: re-wrap the original hash so the Watcher
    /// continues watching the original transaction.
    async fn handle_resubmit(&mut self, req: ResubmitRequest<Input>) -> anyhow::Result<()> {
        tracing::info!(
            command_name = Input::NAME,
            tx_hash = ?req.original_tx_hash,
            nonce = req.nonce,
            "handling resubmission request",
        );

        let fresh = self.estimate_gas_within_caps().await?;

        let item = match resubmission_action(&req.original_gas_params, &fresh) {
            ResubmitAction::SendReplacement => {
                tracing::info!(
                    command_name = Input::NAME,
                    nonce = req.nonce,
                    "fees rose enough — sending replacement transaction",
                );
                let (tx_hash, nonce) =
                    self.build_and_send(&req.command, &fresh, Some(req.nonce)).await?;
                InFlightItem::Tx(InFlightTx {
                    tx_hash,
                    gas_params: fresh,
                    command: req.command,
                    nonce,
                })
            }
            ResubmitAction::RewatchOriginal => {
                tracing::info!(
                    command_name = Input::NAME,
                    tx_hash = ?req.original_tx_hash,
                    "fees have not risen enough — re-watching original tx",
                );
                InFlightItem::Tx(InFlightTx {
                    tx_hash: req.original_tx_hash,
                    gas_params: req.original_gas_params,
                    command: req.command,
                    nonce: req.nonce,
                })
            }
        };

        self.in_flight_tx
            .send(item)
            .await
            .map_err(|_| anyhow::anyhow!("in_flight channel closed"))?;

        Ok(())
    }

    /// Estimate EIP-1559 (and optionally blob) fees, blocking with backoff if
    /// estimates exceed configured caps.
    async fn estimate_gas_within_caps(&mut self) -> anyhow::Result<GasParams> {
        let mut backoff = Backoff::new();
        loop {
            let gas_params = match self.estimate_gas_params().await {
                Ok(p) => p,
                Err(e) if is_transient(&e) => {
                    tracing::warn!(error = %e, delay = ?backoff.current(), "transient error estimating gas, retrying");
                    backoff.wait().await;
                    backoff.advance();
                    continue;
                }
                Err(e) => return Err(e),
            };
            backoff.reset();

            // Check fee caps — enter blocked state if exceeded.
            if gas_params.max_fee_per_gas > self.config.max_fee_per_gas_wei {
                tracing::warn!(
                    estimated = gas_params.max_fee_per_gas,
                    cap = self.config.max_fee_per_gas_wei,
                    "gas fee exceeds cap, waiting 60s before re-estimating",
                );
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            }
            if let Some(blob_fee) = gas_params.max_fee_per_blob_gas {
                if blob_fee > self.config.max_fee_per_blob_gas_wei {
                    tracing::warn!(
                        estimated = blob_fee,
                        cap = self.config.max_fee_per_blob_gas_wei,
                        "blob fee exceeds cap, waiting 60s before re-estimating",
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    continue;
                }
            }

            return Ok(gas_params);
        }
    }

    async fn estimate_gas_params(&self) -> anyhow::Result<GasParams> {
        // Return the raw network estimate — do NOT clamp here.
        // estimate_gas_within_caps() compares raw values against the config caps
        // and blocks if they are exceeded; clamping before the check would
        // prevent the cap-blocking logic from ever triggering.
        let eip1559_est = self.provider.estimate_eip1559_fees().await
            .map_err(anyhow::Error::new)?;
        L1_SENDER_METRICS.report_l1_eip_1559_estimation(eip1559_est)?;

        Ok(GasParams {
            max_fee_per_gas: eip1559_est.max_fee_per_gas,
            max_priority_fee_per_gas: eip1559_est.max_priority_fee_per_gas,
            max_fee_per_blob_gas: None, // blob fee fetched in build_and_send when needed
        })
    }

    /// Build a signed transaction envelope and send it.  Returns `(tx_hash, nonce)`.
    ///
    /// `explicit_nonce`: when `Some`, sets the nonce directly (used for replacement txs).
    /// When `None`, the nonce is taken from `self.next_nonce` and incremented on success.
    ///
    /// `gas_params` must already be within configured caps (ensured by
    /// `estimate_gas_within_caps`).
    async fn build_and_send(
        &mut self,
        command: &Input,
        gas_params: &GasParams,
        explicit_nonce: Option<u64>,
    ) -> anyhow::Result<(TxHash, u64)> {
        let nonce = explicit_nonce.unwrap_or(self.next_nonce);

        // Build the base request inline — gas fields come from the already-estimated
        // GasParams rather than re-fetching from the provider here.
        let mut tx_request = TransactionRequest::default()
            .with_from(self.operator_address)
            .with_max_fee_per_gas(gas_params.max_fee_per_gas)
            .with_max_priority_fee_per_gas(gas_params.max_priority_fee_per_gas)
            .with_gas_limit(15_000_000)
            .with_nonce(nonce)
            .with_to(self.to_address)
            .with_input(command.solidity_call(self.gateway, &self.operator_address));

        let mut gas_params_with_blob = gas_params.clone();
        if let Some(blob_sidecar) = command.blob_sidecar() {
            let fee_per_blob_gas = self.provider.get_blob_base_fee().await
                .map_err(anyhow::Error::new)?;
            L1_SENDER_METRICS.report_blob_base_fee(fee_per_blob_gas)?;
            let max_fee_per_blob_gas = self.config.max_fee_per_blob_gas_wei;
            if fee_per_blob_gas > max_fee_per_blob_gas {
                tracing::warn!(
                    max_fee_per_blob_gas,
                    fee_per_blob_gas,
                    "L1 sender's configured maxFeePerBlobGas is lower than network estimate",
                );
            }
            tx_request.set_max_fee_per_blob_gas(max_fee_per_blob_gas);
            tx_request.set_blob_sidecar(blob_sidecar);
            gas_params_with_blob.max_fee_per_blob_gas = Some(max_fee_per_blob_gas);
        }

        let envelope = self.provider.fill(tx_request).await?.try_into_envelope()?.try_into_pooled()?;

        let pending_block = self.provider.get_block(BlockId::pending()).await
            .map_err(anyhow::Error::new)?
            .expect("no pending block");

        let tx = if self.config.fusaka_upgrade_timestamp <= pending_block.header.timestamp {
            envelope.try_map_eip4844(|tx| {
                tx.try_map_sidecar(|sidecar| {
                    Ok::<_, BlobTransactionValidationError>(
                        BlobTransactionSidecarVariant::Eip7594(sidecar.try_into_eip7594()?),
                    )
                })
            })?
        } else {
            envelope
        };

        let mut backoff = Backoff::new();
        let tx_hash = loop {
            match self.provider.send_raw_transaction(&tx.encoded_2718()).await {
                Ok(pending) => {
                    let hash = *pending.tx_hash();
                    break hash;
                }
                Err(e) if is_nonce_too_low(&anyhow::Error::new(e)) => {
                    tracing::warn!(nonce, "nonce too low, re-fetching");
                    self.next_nonce = self
                        .provider
                        .get_transaction_count(self.operator_address)
                        .await
                        .map_err(anyhow::Error::new)?;
                    // Re-build with new nonce on next iteration of the outer call.
                    // This is a fatal path for replacement txs (nonce is fixed by caller).
                    anyhow::bail!("nonce too low on resubmission — caller must retry");
                }
                Err(e) if is_transient(&anyhow::Error::new(e.clone())) => {
                    tracing::warn!(error = %e, delay = ?backoff.current(), "transient send error, retrying");
                    backoff.wait().await;
                    backoff.advance();
                }
                Err(e) => return Err(anyhow::Error::new(e)).context("send_raw_transaction"),
            }
        };

        L1_SENDER_METRICS.nonce[&Input::NAME].set(nonce);

        if explicit_nonce.is_none() {
            self.next_nonce += 1;
        }

        Ok((tx_hash, nonce))
    }
}

enum Work<Input: SendToL1> {
    New(L1SenderCommand<Input>),
    Resubmit(ResubmitRequest<Input>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::GasParams;

    fn params(fee: u128, priority: u128) -> GasParams {
        GasParams { max_fee_per_gas: fee, max_priority_fee_per_gas: priority, max_fee_per_blob_gas: None }
    }

    fn blob_params(fee: u128, priority: u128, blob: u128) -> GasParams {
        GasParams { max_fee_per_gas: fee, max_priority_fee_per_gas: priority, max_fee_per_blob_gas: Some(blob) }
    }

    #[test]
    fn sends_replacement_when_fees_sufficiently_higher() {
        assert_eq!(resubmission_action(&params(100, 10), &params(120, 15)), ResubmitAction::SendReplacement);
    }

    #[test]
    fn rewatches_original_when_fees_not_risen_enough() {
        assert_eq!(resubmission_action(&params(100, 10), &params(105, 11)), ResubmitAction::RewatchOriginal);
    }

    #[test]
    fn rewatches_original_when_fees_unchanged() {
        let p = params(100, 10);
        assert_eq!(resubmission_action(&p, &p.clone()), ResubmitAction::RewatchOriginal);
    }

    #[test]
    fn sends_replacement_for_blob_tx_when_all_fees_sufficient() {
        assert_eq!(resubmission_action(&blob_params(100, 10, 50), &blob_params(110, 11, 55)), ResubmitAction::SendReplacement);
    }

    #[test]
    fn rewatches_original_for_blob_tx_when_blob_fee_insufficient() {
        assert_eq!(resubmission_action(&blob_params(100, 10, 50), &blob_params(110, 11, 54)), ResubmitAction::RewatchOriginal);
    }

    #[test]
    fn resubmit_uses_fresh_params_not_old() {
        let old = params(100, 10);
        assert_eq!(resubmission_action(&old, &old.clone()), ResubmitAction::RewatchOriginal);
        assert_eq!(resubmission_action(&old, &params(115, 12)), ResubmitAction::SendReplacement);
    }
}
```

- [ ] **Step 5 — Declare module in `lib.rs`**

```rust
mod submitter;
pub(crate) use submitter::{resubmission_action, ResubmitAction, Submitter};
```

- [ ] **Step 6 — Run tests**

```bash
cargo nextest run -p zksync_os_l1_sender 2>&1
```

Expected: all unit tests pass.

- [ ] **Step 7 — Commit**

```bash
git add lib/l1_sender/src/submitter.rs lib/l1_sender/src/lib.rs
git commit -m "feat(l1_sender): implement Submitter with retry, cap-blocking, and resubmission"
```

---

## Task 4: Watcher (`watcher.rs`)

**Files:**
- Create: `lib/l1_sender/src/watcher.rs`
- Modify: `lib/l1_sender/src/lib.rs` (add `mod watcher; pub(crate) use watcher::Watcher;`)

- [ ] **Step 1 — Implement `watcher.rs`**

```rust
// lib/l1_sender/src/watcher.rs

use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::SendToL1;
use crate::error::is_transient;
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use crate::types::{Backoff, InFlightItem, ResubmitRequest};
use crate::validate_tx_receipt;
use alloy::network::Ethereum;
use alloy::primitives::utils::format_ether;
use alloy::providers::Provider;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_observability::ComponentStateHandle;

/// Responsible for waiting for submitted transactions to be mined and
/// forwarding confirmed commands downstream.
///
/// Processes in-flight items sequentially (one at a time) to preserve output
/// ordering — the `in_flight` channel is FIFO, so items arrive in submission
/// order, and the Watcher emits them to `outbound` in the same order.
///
/// On a 300 s timeout: sends a `ResubmitRequest` to the Submitter and waits
/// for a replacement item from `in_flight` before continuing.
pub(crate) struct Watcher<P, Input>
where
    P: Provider<Ethereum>,
    Input: SendToL1,
{
    pub in_flight_rx: mpsc::Receiver<InFlightItem<Input>>,
    pub resubmit_tx: mpsc::Sender<ResubmitRequest<Input>>,
    pub outbound: mpsc::Sender<SignedBatchEnvelope<FriProof>>,
    pub provider: P,
    pub operator_address: alloy::primitives::Address,
    pub poll_interval: Duration,
    pub transaction_timeout: Duration,
    pub latency_tracker: ComponentStateHandle<L1SenderState>,
}

impl<P, Input> Watcher<P, Input>
where
    P: Provider<Ethereum>,
    Input: SendToL1 + Send + 'static,
{
    pub async fn run(mut self) -> anyhow::Result<()> {
        while let Some(item) = self.in_flight_rx.recv().await {
            self.process_item(item).await?;
        }
        tracing::info!(command_name = Input::NAME, "in_flight channel closed");
        Ok(())
    }

    async fn process_item(&mut self, item: InFlightItem<Input>) -> anyhow::Result<()> {
        match item {
            InFlightItem::Passthrough(envelope) => {
                self.outbound
                    .send(*envelope)
                    .await
                    .map_err(|_| anyhow::anyhow!("outbound channel closed"))?;
            }
            InFlightItem::Tx(in_flight) => {
                self.latency_tracker.enter_state(L1SenderState::WaitingL1Inclusion);
                self.watch_until_confirmed(in_flight).await?;
            }
        }
        Ok(())
    }

    /// Poll for a receipt, handling timeout by triggering resubmission.
    ///
    /// On timeout: sends `ResubmitRequest` to Submitter, then reads the
    /// replacement item from `in_flight` and watches that instead —
    /// looping until the tx is eventually confirmed.
    async fn watch_until_confirmed(
        &mut self,
        mut in_flight: crate::types::InFlightTx<Input>,
    ) -> anyhow::Result<()> {
        loop {
            match self.poll_for_receipt(in_flight.tx_hash).await? {
                PollOutcome::Receipt(receipt) => {
                    validate_tx_receipt(&self.provider, &in_flight.command, receipt).await?;
                    self.report_post_confirmation(Input::NAME).await;
                    self.latency_tracker.enter_state(L1SenderState::WaitingSend);
                    for mut envelope in in_flight.command.into() {
                        envelope.set_stage(Input::MINED_STAGE);
                        self.outbound
                            .send(envelope)
                            .await
                            .map_err(|_| anyhow::anyhow!("outbound channel closed"))?;
                    }
                    return Ok(());
                }
                PollOutcome::TimedOut => {
                    // Destructure fully so the move into ResubmitRequest is explicit
                    // and the borrow checker can see in_flight is completely moved
                    // before we reassign it below.
                    let InFlightTx { tx_hash: orig_hash, gas_params: orig_gas, command, nonce } =
                        in_flight;
                    tracing::warn!(
                        command_name = Input::NAME,
                        tx_hash = ?orig_hash,
                        nonce,
                        "transaction timed out, requesting resubmission",
                    );
                    self.resubmit_tx
                        .send(ResubmitRequest {
                            original_tx_hash: orig_hash,
                            original_gas_params: orig_gas,
                            command,
                            nonce,
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("resubmit channel closed"))?;

                    // Block until the Submitter puts a replacement item on in_flight.
                    let replacement = self
                        .in_flight_rx
                        .recv()
                        .await
                        .ok_or_else(|| anyhow::anyhow!("in_flight channel closed during resubmit wait"))?;

                    match replacement {
                        InFlightItem::Tx(new_in_flight) => {
                            in_flight = new_in_flight;
                            // Continue the outer loop to watch the replacement.
                        }
                        InFlightItem::Passthrough(_) => {
                            anyhow::bail!("unexpected passthrough received while waiting for resubmission replacement");
                        }
                    }
                }
            }
        }
    }

    /// Poll `get_transaction_receipt` with backoff until a receipt arrives or
    /// `transaction_timeout` elapses.
    async fn poll_for_receipt(
        &self,
        tx_hash: alloy::primitives::TxHash,
    ) -> anyhow::Result<PollOutcome> {
        let deadline = tokio::time::Instant::now() + self.transaction_timeout;
        let mut backoff = Backoff::new();

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(PollOutcome::TimedOut);
            }

            match self.provider.get_transaction_receipt(tx_hash).await {
                Ok(Some(receipt)) => return Ok(PollOutcome::Receipt(receipt)),
                Ok(None) => {
                    backoff.reset();
                    tokio::time::sleep(self.poll_interval.min(remaining)).await;
                }
                Err(e) if is_transient(&anyhow::Error::new(e)) => {
                    tracing::warn!(delay = ?backoff.current(), "transient error polling receipt, retrying");
                    backoff.wait().await;
                    backoff.advance();
                }
                Err(e) => {
                    return Err(anyhow::Error::new(e)).context("get_transaction_receipt");
                }
            }
        }
    }

    async fn report_post_confirmation(&self, command_name: &'static str) {
        // Balance and nonce reporting after confirmation — non-fatal if they fail.
        if let Ok(balance) = self.provider.get_balance(self.operator_address).await {
            let balance_str = format_ether(balance);
            if let Ok(val) = balance_str.parse::<f64>() {
                L1_SENDER_METRICS.balance[&command_name].set(val);
            }
        }
        if let Ok(nonce) = self.provider.get_transaction_count(self.operator_address).await {
            L1_SENDER_METRICS.nonce[&command_name].set(nonce);
        }
    }
}

enum PollOutcome {
    Receipt(alloy::rpc::types::TransactionReceipt),
    TimedOut,
}
```

- [ ] **Step 2 — Declare module in `lib.rs`**

```rust
mod watcher;
pub(crate) use watcher::Watcher;
```

- [ ] **Step 3 — Compile**

```bash
cargo build -p zksync_os_l1_sender 2>&1
```

Expected: clean build (no tests to run for Watcher yet — the integration tests in Task 6 cover its behaviour).

- [ ] **Step 4 — Commit**

```bash
git add lib/l1_sender/src/watcher.rs lib/l1_sender/src/lib.rs
git commit -m "feat(l1_sender): implement sequential Watcher with timeout-triggered resubmission"
```

---

## Task 5: Wire up `lib.rs`

**Files:**
- Modify: `lib/l1_sender/src/lib.rs`

Replace the body of `run_l1_sender` and update the public API. The helpers `tx_request_with_gas_fields`, `register_operator`, and `validate_tx_receipt` stay in `lib.rs` as `pub(crate)`.

- [ ] **Step 1 — Replace `run_l1_sender` body**

Keep the function signature exactly as-is (no public API break). Replace everything inside the function:

```rust
pub async fn run_l1_sender<Input: SendToL1 + Send + 'static>(
    mut inbound: PeekableReceiver<L1SenderCommand<Input>>,
    outbound: Sender<SignedBatchEnvelope<FriProof>>,
    to_address: Address,
    mut provider: FillProvider<
        impl TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
        impl Provider<Ethereum> + Clone,
    >,
    config: L1SenderConfig<Input>,
    gateway: bool,
) -> anyhow::Result<()> {
    let command_name = Input::NAME;
    let latency_tracker =
        ComponentStateReporter::global().handle_for(command_name, L1SenderState::WaitingRecv);

    let (operator_address, next_nonce) =
        register_operator::<_, Input>(&mut provider, config.operator_signer.clone()).await?;

    // Capacity = command_limit: the Submitter can get at most `command_limit`
    // items ahead of the Watcher before backpressure kicks in.
    let (in_flight_tx, in_flight_rx) = tokio::sync::mpsc::channel(config.command_limit);
    // Capacity = 1: the Watcher can queue at most one resubmit request; the
    // Submitter must handle it before the Watcher proceeds.
    let (resubmit_tx, resubmit_rx) = tokio::sync::mpsc::channel(1);

    let submitter = Submitter {
        inbound,
        in_flight_tx,
        resubmit_rx,
        to_address,
        provider: provider.clone(),
        config: config.clone(),
        gateway,
        operator_address,
        next_nonce,
        latency_tracker: latency_tracker.clone(),
    };

    let watcher = Watcher {
        in_flight_rx,
        resubmit_tx,
        outbound,
        provider,
        operator_address,
        poll_interval: config.poll_interval,
        transaction_timeout: config.transaction_timeout,
        latency_tracker,
    };

    tokio::try_join!(submitter.run(), watcher.run())?;
    Ok(())
}
```

- [ ] **Step 2 — Update `register_operator` to return nonce**

Change `register_operator` to also return the initial nonce:

```rust
async fn register_operator<
    P: Provider + WalletProvider<Wallet = EthereumWallet>,
    Input: SendToL1,
>(
    provider: &mut P,
    signer_config: SignerConfig,
) -> anyhow::Result<(Address, u64)> {
    let address = signer_config
        .register_with_wallet(provider.wallet_mut())
        .await?;

    let balance = provider.get_balance(address).await?;
    L1_SENDER_METRICS.balance[&Input::NAME].set(format_ether(balance).parse()?);
    let address_string: &'static str = address.to_string().leak();
    L1_SENDER_METRICS.l1_operator_address[&(Input::NAME, address_string)].set(1);

    if balance.is_zero() {
        anyhow::bail!("L1 sender's address {address} has zero balance");
    }

    let nonce = provider.get_transaction_count(address).await?;
    L1_SENDER_METRICS.nonce[&Input::NAME].set(nonce);

    tracing::info!(
        command_name = Input::NAME,
        balance_eth = format_ether(balance),
        nonce,
        %address,
        "initialized L1 sender",
    );
    Ok((address, nonce))
}
```

- [ ] **Step 3 — Delete `tx_request_with_gas_fields`**

The Submitter inlines this logic directly in `build_and_send` (Task 3). Remove the function
and all its imports from `lib.rs` entirely. Run `cargo build` to confirm no other callers exist:

```bash
cargo build -p zksync_os_l1_sender 2>&1 | grep "unused\|error"
```

- [ ] **Step 4 — Remove dead code**

Delete:
- `process_prepending_passthrough_commands` (Submitter handles passthroughs uniformly)
- `TRANSACTION_TIMEOUT` constant (moved to config)
- `TransactionReceiptFuture` type alias (no longer used)

Remove the corresponding imports.

- [ ] **Step 5 — Build and test**

```bash
cargo build --workspace --exclude zksync_os_integration_tests 2>&1
cargo nextest run --workspace --exclude zksync_os_integration_tests 2>&1
```

Expected: clean build and all existing tests pass.

- [ ] **Step 6 — Commit**

```bash
git add lib/l1_sender/src/lib.rs
git commit -m "feat(l1_sender): wire Submitter + Watcher into run_l1_sender entry point"
```

---

## Task 6: Resubmission integration tests

**Files:**
- Create: `integration-tests/tests/node/l1_sender_resubmission.rs`
- Modify: `integration-tests/tests/node/mod.rs` (add `mod l1_sender_resubmission;`)

These tests verify the full resubmission loop end-to-end by:
1. Starting a node with a very short `transaction_timeout` (2 s).
2. Pausing anvil auto-mining via `anvil_setAutomine(false)` so txs sit in the mempool.
3. Waiting for the timeout to fire and the resubmission to be issued.
4. Re-enabling mining and confirming the batch eventually lands on L1.

- [ ] **Step 1 — Understand the test harness**

Read `integration-tests/src/lib.rs` and `integration-tests/tests/node/batcher.rs` to understand:
- How `Tester::setup_with_overrides` configures the node.
- How to call raw L1 RPC methods (look for `l1_provider().client().request(...)` patterns).
- Where `batcher_config` fields live (follow the `L1SenderConfig` construction in the node setup).

```bash
grep -n "transaction_timeout\|L1SenderConfig\|anvil_setAutomine\|evm_mine" \
    integration-tests/src/lib.rs integration-tests/tests/node/*.rs 2>/dev/null | head -30
```

- [ ] **Step 2 — Write tests**

```rust
// integration-tests/tests/node/l1_sender_resubmission.rs

use std::time::Duration;
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};

/// Verifies that when a submitted L1 transaction times out (because mining
/// is paused), the L1 sender resubmits it, and after mining resumes the
/// batch is eventually finalized.
///
/// Uses a 2 s `transaction_timeout` so the test completes in a few seconds
/// rather than waiting the production 300 s.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn resubmitted_tx_is_eventually_confirmed() -> anyhow::Result<()> {
    let tester = Tester::setup_with_overrides(|config| {
        // Short timeout so the Watcher fires a resubmission quickly.
        config.batcher_config.l1_sender_transaction_timeout = Duration::from_secs(2);
        config.sequencer_config.block_time = Duration::from_millis(100);
    })
    .await?;

    // Pause L1 auto-mining so submitted transactions sit in the mempool.
    tester
        .l1_provider()
        .client()
        .request::<_, ()>("anvil_setAutomine", (false,))
        .await?;

    // Send one L2 transaction to trigger batch production and an L1 commit tx.
    use alloy::network::TransactionBuilder;
    use alloy::primitives::{Address, U256};
    use alloy::rpc::types::TransactionRequest;
    use zksync_os_integration_tests::assert_traits::ReceiptAssert;
    tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1u64)),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    let l2_block = tester.l2_provider.get_block_number().await?;

    // Wait long enough for the Watcher to time out (2 s timeout + margin).
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Re-enable mining — the resubmitted (or re-watched) tx should now mine.
    tester
        .l1_provider()
        .client()
        .request::<_, ()>("anvil_setAutomine", (true,))
        .await?;

    // Mine a block to include the pending tx.
    tester
        .l1_provider()
        .client()
        .request::<_, ()>("anvil_mine", (1u64, 1u64))
        .await?;

    // The node must eventually finalize the L2 block.
    use zksync_os_integration_tests::assert_traits::DEFAULT_TIMEOUT;
    use zksync_os_integration_tests::provider::ZksyncApi;
    tester
        .l2_zk_provider
        .wait_finalized_with_timeout(l2_block, DEFAULT_TIMEOUT)
        .await?;

    Ok(())
}

/// Verifies that when fees have not risen enough to justify a replacement
/// transaction, the L1 sender re-watches the original hash and still
/// confirms the batch once the block is mined.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn rewatch_path_confirms_original_tx() -> anyhow::Result<()> {
    // Same setup as above but we mine the block *after* the timeout rather
    // than before, so no fee spike happens and the re-watch path is taken.
    let tester = Tester::setup_with_overrides(|config| {
        config.batcher_config.l1_sender_transaction_timeout = Duration::from_secs(2);
        config.sequencer_config.block_time = Duration::from_millis(100);
        // Keep fee estimates artificially low so fresh estimate < old_fee * 1.1.
        config.batcher_config.commit_operator_config.max_fee_per_gas_wei = 1;
    })
    .await?;

    tester
        .l1_provider()
        .client()
        .request::<_, ()>("anvil_setAutomine", (false,))
        .await?;

    use alloy::network::TransactionBuilder;
    use alloy::primitives::{Address, U256};
    use alloy::rpc::types::TransactionRequest;
    use zksync_os_integration_tests::assert_traits::ReceiptAssert;
    tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1u64)),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    let l2_block = tester.l2_provider.get_block_number().await?;

    // Wait for the Watcher to fire the resubmit request and for the Submitter
    // to send back an InFlightTx with the *original* hash (re-watch path).
    tokio::time::sleep(Duration::from_secs(5)).await;

    tester
        .l1_provider()
        .client()
        .request::<_, ()>("anvil_setAutomine", (true,))
        .await?;
    tester
        .l1_provider()
        .client()
        .request::<_, ()>("anvil_mine", (1u64, 1u64))
        .await?;

    use zksync_os_integration_tests::assert_traits::DEFAULT_TIMEOUT;
    use zksync_os_integration_tests::provider::ZksyncApi;
    tester
        .l2_zk_provider
        .wait_finalized_with_timeout(l2_block, DEFAULT_TIMEOUT)
        .await?;

    Ok(())
}
```

- [ ] **Step 3 — Expose `transaction_timeout` in the node config**

Search for where `L1SenderConfig` is constructed in the node batcher configuration:

```bash
grep -rn "L1SenderConfig\|transaction_timeout\|l1_sender" \
    node/ integration-tests/ --include="*.rs" | grep -v target | head -30
```

Add the `l1_sender_transaction_timeout` override path so the test harness can inject it.

- [ ] **Step 4 — Run integration tests**

```bash
cargo nextest run -p zksync_os_integration_tests --profile no-pig \
    -E 'test(l1_sender_resubmission)' 2>&1
```

Expected: both tests pass.

- [ ] **Step 5 — Run full CI suite**

```bash
cargo fmt --all --check 2>&1 && \
cargo clippy --all-targets --all-features --workspace -- -D warnings 2>&1 && \
cargo nextest run --release --workspace --exclude zksync_os_integration_tests 2>&1 && \
cargo nextest run -p zksync_os_integration_tests --profile no-pig 2>&1
```

Expected: all checks clean.

- [ ] **Step 6 — Commit**

```bash
git add integration-tests/tests/node/l1_sender_resubmission.rs \
        integration-tests/tests/node/mod.rs
git commit -m "test(l1_sender): add resubmission integration tests"
```
