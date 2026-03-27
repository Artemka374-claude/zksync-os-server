# L1 Sender: Phase-Based Design with Task Decomposition

This document unifies two design iterations:

1. **Phase-based struct** (Approach B from the earlier error-handling redesign) — replaces
   the crash-on-any-error model with a structured retry loop and typed error categories.
2. **Task decomposition** — extends the phase-based struct by splitting the Submitter and
   Watcher concerns into independent tasks, eliminating phase-blocking and enabling
   concurrent receipt polling.

The phase-based struct is the foundation. Task decomposition is the architectural
improvement that makes the phases genuinely independent.

---

## Problem

The original L1 sender crashes on any error. Every error — transient RPC timeout, gas
spike, pending block gap — propagates as `Err` from `run_l1_sender`, hits
`.expect("pipeline segment failed")` in `builder.rs`, panics the critical task, and kills
the entire binary.

**Impact:**
- Kubernetes sees repeated crashes → CrashLoopBackOff (exponential restart delay up to 5 min)
- The only alert fires on user impact ("no batches committed in 1.5h") — up to 1.5h silent
  downtime
- Each restart risks double-sending (no in-flight tx detection)
- API, mempool, sequencer all die alongside the batcher

**Additionally**, even after introducing retry logic, a sequential loop creates a
phase-blocking problem: a non-fatal error in phase N causes `continue`, skipping all later
phases. This means:

- A transient error submitting the 5th of 5 txs prevents receipt checking for the 4
  already in-flight txs.
- A timeout on the 5th of 5 in-flight txs prevents forwarding the 4 already-confirmed txs
  downstream.

---

## Scope

**In scope:**
- Error categorization for all error paths in `run_l1_sender`
- Retry/backoff logic for transient and recoverable errors
- `GasBlocked` / `BlobFeeBlocked` states with dedicated metrics for early alerting
- Fix the `expect("no pending block")` Infura crash
- Phase independence via task decomposition
- Concurrent receipt polling via `FuturesOrdered`

**Out of scope:**
- Transaction replacement (re-submit at same nonce with higher gas) — follow-up
- Pre-flight `eth_call` simulation — follow-up
- In-flight transaction detection on startup — follow-up
- Decoupling sequencer from batcher pipeline — separate effort

---

## Error Categories

Every error path in `run_l1_sender` falls into one of three categories.

**Note on startup errors:** `register_operator()` and
`process_prepending_passthrough_commands()` run before the main loop and remain Fatal —
if the operator cannot be registered, the sender cannot function.

### Transient — retry with backoff

Temporary infrastructure issues that resolve on their own.

| Error | Notes |
|---|---|
| `provider.estimate_eip1559_fees()` | RPC timeout/rate limit |
| `provider.get_blob_base_fee()` | RPC timeout/rate limit |
| `provider.fill(tx_request)` (RPC portion) | Nonce fetch or gas estimation failure |
| `provider.get_block(BlockId::pending())` | Falls back to latest block if `None` |
| `provider.send_raw_transaction()` (network failure) | Not a mempool rejection |
| Receipt polling RPC failures | Alloy watcher RPC errors during polling |

**Behavior:** Log warning, sleep with exponential backoff (5s → 10s → 20s → 40s → 60s,
capped at 60s), retry. Reset backoff on success.

**Important:** `try_into_envelope()` and `try_into_pooled()` are local conversion errors —
if these fail it indicates a code bug or data corruption, not an RPC issue. Classify as
**Fatal**.

### Recoverable — wait for external condition to change

Not a bug, but the sender cannot proceed right now.

| Error | Condition |
|---|---|
| Gas fees above configured cap | Network congestion — enter `GasBlocked`, wait |
| Blob fees above configured cap | Blob demand spike — enter `BlobFeeBlocked`, wait |
| Tx timeout (300s) | Tx stuck in mempool — resubmit via Submitter |
| Nonce too low (mempool rejection) | Prior tx mined; detected by parsing RPC error |

**Behavior:** Enter specific state (`GasBlocked`, `BlobFeeBlocked`), emit dedicated metric,
sleep 30s, re-check condition.

**Note on nonce detection:** `send_raw_transaction` returns a generic transport error on
mempool rejection. To distinguish "nonce too low" from other rejections, pattern-match on
the RPC error message or code (e.g. code -32000, message "nonce too low"). If the error
does not match a known pattern, treat as Transient.

### Fatal — crash (correct behavior)

Unrecoverable without code or infrastructure changes.

| Error | Reason |
|---|---|
| Zero operator balance at startup | Needs manual funding |
| Signer registration failure | KMS/key config broken |
| Tx reverted on L1 | Contract/calldata bug, gas already burned |
| Unsupported protocol version | Needs code update |
| Invalid blob data / `try_into_eip7594` | Data corruption |
| `try_into_envelope()` / `try_into_pooled()` failure | Malformed tx data |
| Passthrough after `SendToL1` | Pipeline protocol violation |
| Inbound channel closed unexpectedly | Upstream crashed |
| Outbound channel closed (receiver dropped) | Downstream crashed |

**Behavior:** Log error with full context, return `Err`.

### Non-propagating — log and ignore

These currently propagate via `?` but should never crash the binary.

| Error | Reason |
|---|---|
| `format_ether(balance).parse::<f64>()` | Informational balance reporting |
| Metrics formatting failures | Never affect the send loop |

---

## Architecture

### Context: How the Pipeline Works

Every component in the sequencer implements `PipelineComponent` and connects to its
neighbours via bounded `mpsc` channels. The channel capacity controls backpressure. The
L1 sender is already one such component. The proposed design applies the same pattern one
level deeper: the sender's internal phases become two independent tasks connected by
bounded channels.

### Phase-Based State

Commands flow through three collections on their way from the upstream channel to L1
confirmation:

```
upstream channel
    → pending_commands   (received, not yet submitted)
    → in_flight          (submitted to L1, receipt pending)
    → downstream channel (confirmed, forwarded)
```

Each collection survives errors independently. A transient RPC failure during submission
leaves already-submitted commands in `in_flight` and unsubmitted ones in
`pending_commands`. On retry, only the unsubmitted ones need to be sent.

### Two Independent Tasks

```
upstream channel
    ──► Submitter ──► in_flight channel ──► Watcher ──► downstream channel
            ▲                                   │
            └────── resubmit channel ◄──────────┘
```

#### Submitter

Responsible for everything that touches the L1 provider:

- Reads `L1SenderCommand<Input>` from the upstream channel.
- Handles passthrough commands before the main loop starts (same as today).
- Estimates EIP-1559 fees and blob base fee; enforces configured caps
  (`GasBlocked`, `BlobFeeBlocked`).
- Builds, signs, and submits transactions via `send_raw_transaction`.
- Sends each submitted `InFlightTx` (command + tx_hash + receipt_future) to the Watcher
  via the in-flight channel.
- Listens on the resubmit channel for commands the Watcher could not confirm (timeout or
  transient receipt error), and prepends them to `pending_commands` for resubmission.
- Maintains its own `ExponentialBackoff` for transient RPC errors.

The Submitter is the **only** task that holds a reference to the L1 provider.

#### Watcher

Responsible for observing receipt futures and routing results:

- Receives `InFlightTx` items from the Submitter via the in-flight channel.
- Tracks all outstanding receipt futures in a `FuturesOrdered`, which polls them
  **concurrently** and yields results in nonce order.
- On a confirmed receipt: sets `MINED_STAGE` on the command, forwards it downstream.
- On a timeout (`WatchTxError::Timeout`) or transient error: sends the command back to
  the Submitter via the resubmit channel for resubmission.
- On a fatal error (tx reverted on L1): returns `Err`.

The Watcher does **not** hold a reference to the L1 provider. All resubmission goes
through the Submitter.

#### Why `FuturesOrdered` and Not `FuturesUnordered`

L1 transactions are nonce-ordered: tx N+1 cannot be mined before tx N. Yielding results
out of nonce order would require the downstream to reorder them. With `FuturesOrdered`,
the Watcher yields results in submission order automatically, while still polling all
futures concurrently.

### Channel Summary

| Channel | Item type | Capacity | Direction |
|---|---|---|---|
| upstream | `L1SenderCommand<Input>` | existing | → Submitter |
| in_flight | `InFlightTx<Input>` | `command_limit` | Submitter → Watcher |
| resubmit | `Input` | `command_limit` | Watcher → Submitter |
| downstream | `SignedBatchEnvelope<FriProof>` | existing | Watcher → |

### InFlightTx

```rust
struct InFlightTx<Input> {
    command: Input,
    tx_hash: B256,
    receipt_future: TransactionReceiptFuture,
}
```

`tx_hash` is tracked separately from the receipt future so that when the future times out
(and is consumed), the hash is still available for logging and resubmission tracking.

---

## Error Handling Per Task

### Submitter

| Error | Behaviour |
|---|---|
| Transient RPC failure (fee estimation, fill, send) | Log, exponential backoff, retry |
| `GasBlocked` | Enter `GasBlocked` state, emit metric, sleep 30s |
| `BlobFeeBlocked` | Enter `BlobFeeBlocked` state, emit metric, sleep 30s |
| `NonceTooLow` | Treat as transient (prior resubmit landed); retry with backoff |
| Fatal (envelope conversion, revert at send) | Return `Err` |

### Watcher

| Error | Behaviour |
|---|---|
| `WatchTxError::Timeout` | Send command to resubmit channel, continue |
| Transient receipt polling error | Send command to resubmit channel, continue |
| Tx reverted on L1 | Log trace, return `Err` (fatal) |
| Resubmit channel closed (Submitter died) | Return `Err` |
| Downstream channel closed | Return `Err` |

### Fatal Error Propagation

When the Watcher returns `Err`:
- Its downstream channel sender is dropped.
- The downstream component detects the closed channel and shuts down.

When the Submitter returns `Err`:
- Its in-flight channel sender is dropped.
- The Watcher's `in_flight_rx.recv()` returns `None`.
- The Watcher drains any remaining futures in-order, then returns `Err`.

Channel closure cascades shutdown naturally — consistent with how the rest of the pipeline
handles component failure. No explicit cancellation token is needed.

---

## Backoff Independence

Each task has its own `ExponentialBackoff` instance (initial: 5s, max: 60s):

- A transient RPC failure in the Submitter does not pause receipt polling in the Watcher.
- A slow or stuck inclusion does not block new tx submissions in the Submitter.
- Each task resets its own backoff on a successful operation.

---

## Passthrough Commands

The passthrough phase runs before either task starts, exactly as today: drain any leading
`Passthrough` commands from the upstream channel, forward them directly to the downstream
channel. Once the first `SendToL1` command arrives, both tasks are spawned. The passthrough
logic is not affected by this change.

---

## What This Fixes

| Problem | Before | After |
|---|---|---|
| Any error crashes the binary | Yes | No — categorized retry/backoff |
| Phase 2 error blocks phase 3 | Yes | No — tasks are independent |
| Phase 3 error blocks forwarding | Yes | No — Watcher forwards each confirmed tx immediately |
| Sequential receipt polling | Yes | No — `FuturesOrdered` polls all futures concurrently |
| Slow tx blocks detection of others | Yes | No — `FuturesOrdered` yields as each completes |
| `expect("no pending block")` crash | Yes | No — falls back to latest block |

---

## What This Enables (Follow-ups)

- **Transaction replacement:** The Submitter already tracks `tx_hash` per in-flight tx.
  Explicit nonce management is the natural next step for replacing stuck txs at the same
  nonce with a higher gas price.
- **Per-task metrics:** Each task can report its own latency and error counters, giving
  finer-grained observability than a single-component metric set.
- **Pre-flight simulation:** Adding an `eth_call` in the Submitter before
  `send_raw_transaction` is a natural extension once the Submitter is isolated.

---

## Testing

1. **Transient errors do not crash.** Mock provider returns RPC errors for N calls, then
   succeeds. Verify the Submitter retries and eventually succeeds.
2. **Commands are not lost.** Inject a transient error mid-submission. Verify all commands
   are eventually sent or remain in `pending_commands` for retry.
3. **Gas-blocked state emits metrics.** Configure a low gas cap, mock high fees. Verify
   `GasBlocked` state is entered and the metric fires.
4. **Fatal errors crash.** Mock a tx revert receipt. Verify the Watcher returns `Err` and
   the Submitter shuts down via channel closure.
5. **Partial submission progress.** Submit 3 of 5 commands, inject RPC error. Verify the
   remaining 2 stay in `pending_commands` and the 3 are sent to the Watcher.
6. **Confirmed txs forwarded independently.** 4 of 5 in-flight txs confirm; 5th times out.
   Verify the 4 are forwarded downstream and the 5th is sent to the resubmit channel.
7. **Backoff resets on success.** After a successful submission cycle, verify the
   Submitter's backoff returns to the initial delay.

---

## Files to Change

| File | Change |
|---|---|
| `lib/l1_sender/src/lib.rs` | Replace `L1SenderLoop` with `Submitter` and `Watcher` structs; update `run_l1_sender` to spawn both and handle passthrough before starting tasks |
| `lib/l1_sender/src/error.rs` | Existing `L1SendError` / `RecoverableReason` types unchanged; may add Watcher-specific variants |
| `lib/l1_sender/src/metrics.rs` | Add per-task state labels; add `GasBlocked`, `BlobFeeBlocked`, `TransientBackoff` states |
| `node/bin/src/config/mod.rs` | Raise `max_priority_fee_per_gas` default from 1 gwei to 10 gwei |
