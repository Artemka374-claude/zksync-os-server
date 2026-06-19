# L1 Sender: Submitter + Watcher Split

**Date:** 2026-03-31
**Status:** Approved

---

## Problem

The L1 sender crashes on errors that depend on third parties (L1 provider unavailability, gas price spikes). This causes the entire sequencer to enter a crash loop, producing unnecessary downtime — not just delayed withdrawals but full API/execution outage.

---

## Goal

Make the L1 sender resilient to transient and recoverable failures without crashing the sequencer. Classify all errors so that only genuinely fatal conditions terminate the process.

---

## Architecture

### Channel Layout

```
inbound ──► Submitter ──► in_flight channel ──► Watcher ──► outbound
                ▲                                    │
                └────────── resubmit channel ────────┘
```

The existing `run_l1_sender` entry point creates both internal channels, spawns Submitter and Watcher as sibling tokio tasks, and joins them with `try_join!`. A fatal error in either task cancels the other and propagates upward — same crash semantics as today.

### Sequential Watcher

The Watcher processes in-flight items one at a time. This maintains output ordering trivially (FIFO through the `in_flight` channel) and keeps nonce replacement simple: when a timeout triggers a resubmit request, the Submitter handles it before the next tx is submitted, so there is never more than one tx pending replacement at a time.

The pipeline still achieves concurrency: the Submitter can estimate gas and send tx N+1 while the Watcher is awaiting confirmation of tx N.

### Passthrough ordering

Passthrough commands (batches already committed on L1) flow through the `in_flight` channel as an `InFlightItem::Passthrough` variant rather than going directly to `outbound`. The Watcher forwards them immediately. This preserves FIFO ordering between passthroughs and confirmed txs.

---

## Module Structure

```
lib/l1_sender/src/
├── lib.rs                  # entry point — creates channels, spawns tasks
├── submitter.rs            # Submitter task
├── watcher.rs              # Watcher task
├── types.rs                # InFlightItem, InFlightTx, ResubmitRequest, GasParams
├── config.rs               # unchanged
├── pipeline_component.rs   # unchanged
├── metrics.rs              # unchanged
├── batcher_metrics.rs      # unchanged
├── batcher_model.rs        # unchanged
└── commands/               # unchanged
```

---

## Shared Types

```rust
/// Flows from Submitter → Watcher through the in_flight channel.
enum InFlightItem<Input> {
    Tx(InFlightTx<Input>),
    Passthrough(Box<SignedBatchEnvelope<FriProof>>),
}

/// A submitted L1 transaction that the Watcher is responsible for.
struct InFlightTx<Input> {
    tx_hash: TxHash,
    gas_params: GasParams,
    /// Original command — needed to rebuild calldata on resubmission.
    command: Input,
    /// Nonce used when this tx was submitted — needed for replacement txs.
    nonce: u64,
}

/// Sent from Watcher → Submitter when a tx times out.
struct ResubmitRequest<Input> {
    original_tx_hash: TxHash,
    original_gas_params: GasParams,
    command: Input,
    nonce: u64,
}

/// Fee parameters attached to every submitted tx so they can be compared
/// at resubmission time.
struct GasParams {
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    /// None for non-blob transactions.
    max_fee_per_blob_gas: Option<u128>,
}
```

**Channel capacities:**
- `in_flight`: `command_limit` (existing config field) — backpressure prevents unbounded ahead-of-time submission.
- `resubmit`: 1 — at most one timed-out tx can be awaiting resubmission at a time given sequential watching.

---

## Submitter

### Work selection

The Submitter checks the resubmit channel first, then the inbound channel. A timed-out tx takes priority over new commands so the pipeline does not stall waiting for a replacement.

### Normal submission path

1. Estimate gas params via `estimate_eip1559_fees()` and (if blob tx) `get_blob_base_fee()`. Retry transient RPC failures with exponential backoff.
2. If any fee exceeds its configured cap, enter a blocked loop: sleep 60 s, re-estimate, repeat until fees are within caps.
3. Build and sign the transaction. Let `FillProvider` fill the nonce; record the resulting nonce in `GasParams`.
4. Send via `send_raw_transaction()`. Retry transient network failures with backoff.
5. Send `InFlightItem::Tx(InFlightTx { tx_hash, gas_params, command, nonce })` to the `in_flight` channel.

### Resubmission path (strategy A)

Triggered by a `ResubmitRequest` from the Watcher:

1. Estimate fresh gas params (same transient retry).
2. If `new_fee >= old_fee × 1.1` for all fee dimensions:
   - Send a replacement transaction with **the same nonce** (set explicitly, bypassing FillProvider's nonce fill).
   - Send the new `InFlightTx` (new hash, new gas params, same command + nonce) to `in_flight`.
3. Otherwise (fees have not risen enough to justify replacement):
   - Wrap `original_tx_hash` back into an `InFlightTx` with the original gas params and send it to `in_flight` for the Watcher to re-watch.
   - No new transaction is submitted.

### Backoff schedule

Applies to transient errors only. Delays: 5 s → 10 s → 20 s → 40 s → 60 s (capped at 60 s). Reset to 5 s on the next successful operation.

### Nonce tracking

`next_nonce: u64` is initialised at startup from `provider.get_transaction_count(operator_address)` (existing behaviour). The Submitter records the nonce used for each tx. Replacement txs set nonce explicitly via the transaction builder rather than relying on FillProvider.

---

## Watcher

The Watcher loops over `InFlightItem` values from the `in_flight` channel sequentially.

### Passthrough

Forward the envelope to `outbound` immediately and continue.

### Tx watching

1. Start a 300 s timeout.
2. Poll for the receipt. Retry transient RPC polling failures with the same backoff schedule.
3. **Receipt received, status ok:** run existing `validate_tx_receipt` logic, emit metrics, send the completed command to `outbound`.
4. **Receipt received, status reverted:** fatal error — crash both tasks.
5. **Timeout (300 s):** send a `ResubmitRequest` to the resubmit channel, then block on `in_flight` for the replacement item from the Submitter. The replacement arrives as a normal `InFlightTx` (either a new tx hash or the original one re-wrapped), so the watching loop restarts without special-casing.
6. **Block reorg detected** (receipt's block hash is no longer in the canonical chain): fatal error — crash both tasks.

---

## Error Classification

All errors are **fatal by default**. Only the following are non-fatal.

### Transient — retry with backoff

| Error | Notes |
|---|---|
| `estimate_eip1559_fees()` failure | RPC timeout / rate limit |
| `get_blob_base_fee()` failure | RPC timeout / rate limit |
| `fill(tx_request)` failure | Nonce fetch or gas estimation RPC failure |
| `provider.get_block(BlockId::pending())` returns `None` | Fall back to latest block |
| `send_raw_transaction()` network failure | Distinguish from mempool rejection by RPC error code |
| Receipt polling RPC failures | Alloy watcher errors during polling |
| `provider.get_balance()` during operation | RPC failure mid-loop |
| `provider.get_transaction_count()` during operation | RPC failure mid-loop |
| `get_raw_protocol_version()` in UpgradeGatekeeper | RPC call to L1 |

### Recoverable — wait for condition to change

| Error | Condition |
|---|---|
| Gas fees above configured cap | Sleep 60 s, re-estimate |
| Blob fees above configured cap | Sleep 60 s, re-estimate |
| Tx timeout (300 s) | Send `ResubmitRequest`, await replacement |
| Nonce too low (mempool rejection) | Prior tx already mined; re-fetch nonce and retry |

### Non-propagating — log and continue

| Error | Notes |
|---|---|
| `format_ether(balance).parse::<f64>()` | Balance reporting only — never affects send loop |
| `wei_to_gwei()` failures in metrics | Metrics formatting |
| `report_blob_base_fee()` formatting | Metrics formatting |

### Fatal — crash both tasks

| Error | Notes |
|---|---|
| Transaction revert | Unrecoverable on-chain failure |
| Block reorg | Chain reorganisation |
| Channel closed (`SendError`) | Downstream shut down |
| `BatchVerificationError` (both variants) | Bad upstream data — signing or config error |
| `BlobTransactionValidationError` | Invalid blob sidecar — programming or upstream error |
| `ProtocolSemanticVersion::try_from()` failure | L1 returned an unparseable protocol version |
| `current_protocol_version > target_protocol_version` | Protocol version regression — operator misconfiguration |
| Peeked / received command mismatch | Invariant violation — programming error |
| Zero balance at startup | Operator misconfiguration |

---

## Testing

### Unit tests

**Resubmission strategy (in `submitter.rs`):**
- Fee bump sufficient (`new_fee >= old_fee × 1.1`): Submitter sends a replacement tx with the same nonce.
- Fee bump insufficient (`new_fee < old_fee × 1.1`): Submitter wraps `original_tx_hash` back into an `InFlightTx` without sending a new tx.
- Replacement tx carries the correct (explicitly set) nonce.
- Resubmit request is prioritised over new inbound commands.

**Backoff (in `submitter.rs`):**
- Delays step through 5 s → 10 s → 20 s → 40 s → 60 s.
- Backoff resets to 5 s after a successful operation.

**Gas cap blocking (in `submitter.rs`):**
- Submitter enters blocked loop when fee exceeds cap, exits when fee drops below cap.

### Integration tests (in `zksync_os_integration_tests`)

**Resubmission flows:**
- Timeout → resubmit → confirmation: Watcher fires timeout, Submitter resubmits (fees rose enough), second tx confirms, command forwarded downstream.
- Timeout → re-watch → confirmation: fees have not risen enough; Watcher re-watches original hash, original tx eventually confirms, command forwarded downstream.
- Double timeout: first resubmit also times out, triggers a second resubmit cycle, command eventually forwarded downstream.
