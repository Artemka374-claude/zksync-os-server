# L1 Sender Architecture

## Overview

The L1 sender is a **generic, stateless transaction dispatcher** responsible for submitting batch-related Ethereum transactions to L1. It is instantiated three times in the main node pipeline — once each for committing, proving, and executing batches — each handling a distinct phase of batch finalization on-chain.

---

## Component Location

```
lib/l1_sender/src/
├── lib.rs                  # Core run_l1_sender() event loop and transaction logic
├── config.rs               # L1SenderConfig<Input>
├── pipeline_component.rs   # L1Sender struct implementing PipelineComponent
├── batcher_model.rs        # BatchEnvelope, BatchMetadata, FriProof, SignedBatchEnvelope
├── batcher_metrics.rs      # BatchExecutionStage enum, BatcherSubsystemMetrics
├── metrics.rs              # L1SenderState enum, L1SenderMetrics
├── upgrade_gatekeeper.rs   # UpgradeGatekeeper: gates commit sender on L1 protocol version
└── commands/
    ├── mod.rs              # SendToL1 trait, L1SenderCommand<C> enum
    ├── commit.rs           # CommitCommand: commits a single batch to L1
    ├── prove.rs            # ProofCommand: submits SNARK proof for N–M batches
    └── execute.rs          # ExecuteCommand: executes finalized batches on L1
```

---

## Core Abstractions

### `SendToL1` Trait (`commands/mod.rs`)

The central trait implemented by all three command types. Encapsulates how a command is serialized into an L1 transaction.

```rust
trait SendToL1 {
    const NAME: &'static str;
    const SENT_STAGE: BatchExecutionStage;
    const MINED_STAGE: BatchExecutionStage;
    const PASSTHROUGH_STAGE: BatchExecutionStage;

    fn solidity_call(&self) -> SomeCallBuilder;
    fn blob_sidecar(&self) -> Option<BlobTransactionSidecar>;
    fn display_range(&self) -> String;
}
```

### `L1SenderCommand<C: SendToL1>` Enum

```rust
enum L1SenderCommand<C: SendToL1> {
    // An L1 transaction must be submitted for this batch.
    SendToL1(C),
    // Batch was already committed/proved upstream; pass it downstream without L1 submission.
    Passthrough(Box<SignedBatchEnvelope<FriProof>>),
}
```

### `BatchEnvelope<E, S>` (`batcher_model.rs`)

Generic wrapper carrying a batch through the pipeline. `E` is the proof type (typically `FriProof`), `S` tracks whether the envelope is signed or unsigned.

- `BatchForSigning<E>` — unsigned, awaiting operator signature
- `SignedBatchEnvelope<E>` — signed, ready for L1 submission or passthrough

Contains a `latency_tracker` for end-to-end stage timing.

### `BatchMetadata` (`batcher_model.rs`)

Holds the per-batch data passed to L1 contracts:

- Batch number and state commitments
- L1 logs, messages, and multichain root
- `previous_stored_batch_info` and `batch_info` (L1 contract structs)
- Block numbers, transaction counts, protocol version, pubdata mode

### `L1Sender<F, P, C>` (`pipeline_component.rs`)

The `PipelineComponent` implementation. Stateless between runs.

| Field | Description |
|---|---|
| `provider` | Alloy Ethereum provider for RPC calls and tx submission |
| `config` | `L1SenderConfig<C>` (gas limits, signer, poll interval) |
| `to_address` | L1 contract address (validator timelock) |
| `gateway` | `true` if connected via gateway (protocol v31+) |

---

## Configuration (`config.rs`)

| Field | Default | Description |
|---|---|---|
| `operator_signer` | required | Signing key or KMS signer for L1 transactions |
| `max_fee_per_gas_wei` | 200 Gwei | EIP-1559 gas price cap |
| `max_priority_fee_per_gas_wei` | 1 Gwei | EIP-1559 priority fee cap |
| `max_fee_per_blob_gas_wei` | 2 Gwei | EIP-4844 blob fee cap |
| `command_limit` | 16 | Max commands processed in parallel per tick |
| `poll_interval` | 100ms | Interval for polling L1 for new state |
| `fusaka_upgrade_timestamp` | `u64::MAX` | Timestamp after which EIP-7594 blob format is used |

The node configures three independent `L1SenderConfig` instances, one per operation type, each with its own signing key (`operator_commit_sk`, `operator_prove_sk`, `operator_execute_sk`).

---

## Event Loop (`lib.rs`)

`run_l1_sender<Input: SendToL1>()` is the main async loop:

1. **Startup** — Register operator address; validate that account balance > 0.
2. **Drain passthroughs** — Consume any leading `Passthrough` commands before normal operation begins (via `process_prepending_passthrough_commands`).
3. **Main loop:**
   - Receive up to `command_limit` commands from the inbound channel.
   - For `Passthrough` commands, forward downstream immediately without hitting L1.
   - For `SendToL1` commands:
     - Fetch EIP-1559 gas estimates from provider; cap at configured limits.
     - For commits with blobs: fetch blob base fee and set `max_fee_per_blob_gas`.
     - Fill transactions (nonce, gas estimation).
     - If `fusaka_upgrade_timestamp` has passed, convert to EIP-7594 blob format.
     - Send raw transactions; collect receipt futures.
   - Wait for all receipt futures concurrently (300-second timeout).
   - Validate each receipt; on failure, fetch `debug_traceTransaction` and crash with detail.
   - Report metrics (balance, nonce, gas used, fees).
   - Send completed `SignedBatchEnvelope` values downstream.

---

## Command Implementations

### `CommitCommand` (`commands/commit.rs`)

- **Scope:** One batch per L1 transaction.
- **L1 call:** `IMultisigCommitter::commitBatchesMultisig` (with operator signatures) or `IExecutor::commitBatchesSharedBridge` (without).
- **Blob sidecar:** Included (EIP-4844).
- **Signature handling:** Filters and encodes multisig operator signatures from L1 validator config.

### `ProofCommand` (`commands/prove.rs`)

- **Scope:** Batches N through M per L1 transaction.
- **L1 call:** `IExecutor::proveBatchesSharedBridge`.
- **Proof encoding:** Computes SNARK public input by hashing state commitments; selects verifier version (4, 5, or 6).
- **Testing support:** Supports fake proofs for local/test environments.

### `ExecuteCommand` (`commands/execute.rs`)

- **Scope:** Multiple finalized batches per L1 transaction.
- **L1 call:** `IExecutor::executeBatchesSharedBridge`.
- **Calldata encoding varies by protocol version:**
  - v29–30: `StoredBatchInfo` + `PriorityOps` + `InteropRoots`
  - v31–32: Adds `L2Logs`, `Messages`, `MultichainRoots` (if via gateway), and operator address.
- **Priority ops:** Sourced from L1 watcher.

---

## Pipeline Position

The L1 sender appears three times in the main node pipeline:

```
Blocks
  └─► BlockExecutor (sequencer)
        └─► Batcher
              └─► BatchSigner
                    └─► FriProver
                          └─► GaplessCommitter
                                └─► [UpgradeGatekeeper]
                                      └─► L1Sender<CommitCommand>  ──► L1
                                            └─► SnarkProver
                                                  └─► GaplessL1ProofSender
                                                        └─► L1Sender<ProofCommand>  ──► L1
                                                              └─► PriorityTreePipelineStep
                                                                    └─► L1Sender<ExecuteCommand>  ──► L1
                                                                          └─► (results stored/published)
```

### Surrounding Components

| Component | Role |
|---|---|
| `GaplessCommitter` | Ensures batches are committed in order; produces `L1SenderCommand<CommitCommand>` |
| `GaplessL1ProofSender` | Orders out-of-sequence proofs before submission |
| `UpgradeGatekeeper` | Wraps the commit L1 sender; blocks submission if L1 contract protocol version lags |
| `PriorityTreePipelineStep` | Prepares priority queue data before execution |
| `L1Watcher` | Independently monitors L1 for priority transactions; feeds `ExecuteCommand` |

---

## State and Statefulness

The L1 sender holds **no persistent local state**. Between runs it relies on:

- **Upstream buffering** — `GaplessCommitter` and `GaplessL1ProofSender` handle ordering and retry.
- **L1 nonce tracking** — The provider auto-increments; the sender reads current nonce each tick.
- **Receipt-based correctness** — Every transaction outcome is validated before signalling downstream.

Per-tick transient state:

| Variable | Description |
|---|---|
| `cmd_buffer` | Commands received in the current tick |
| `pending_txs` | In-flight `(TransactionReceiptFuture, command)` pairs |

---

## Error Handling

- All fallible operations use `.context("...")` (anyhow) for structured error messages.
- **Transaction failure:** If a receipt indicates revert, `debug_traceTransaction` is fetched and the node crashes with the full call frame detail.
- **Receipt timeout:** 300-second hard timeout per batch of transactions.
- **Zero balance on startup:** Node panics if the operator account has no funds.
- **Known limitation:** Does not detect in-flight transactions from a prior run on startup; this is safe because nonce tracking on the provider side prevents double-submission, and the node recovers cleanly on restart.

---

## Observability

### Component States (`L1SenderState`)

| State | Meaning |
|---|---|
| `WaitingRecv` | Idle, no commands in flight |
| `SendingToL1` | Building and signing transactions |
| `WaitingL1Inclusion` | Waiting for transaction receipts |
| `WaitingSend` | Forwarding results downstream |

### Metrics (`L1SenderMetrics`)

| Metric | Description |
|---|---|
| `l1_operator_address` | Per-operation operator Ethereum address |
| `balance` | Current operator wallet balance (ETH) |
| `parallel_transactions` | Number of concurrent in-flight L1 transactions |
| `l1_transaction_fee_ether` | Histogram of transaction costs |
| `l1_transaction_fee_per_l2_tx_ether` | Cost normalized per L2 transaction |
| `gas_used` | Total gas consumed per submission |
| `blob_base_fee_gwei` | EIP-4844 base fee at time of submission |
| `effective_blob_gas_price_gwei` | Actual blob gas price paid |
| `blob_gas_used` | Total blob gas consumed |
| `effective_gas_price_gwei` | Actual gas price paid |
| `estimated_max_fee_per_gas_gwei` | EIP-1559 max fee estimate |
| `estimated_max_priority_fee_per_gas_gwei` | EIP-1559 priority fee estimate |
| `nonce` | Last used operator nonce |

### Batch Execution Stages (`BatchExecutionStage`)

Tracks each batch through commit → prove → execute across both the "sent" and "mined" events, with a `Passthrough` variant for each phase when no L1 submission is needed.
