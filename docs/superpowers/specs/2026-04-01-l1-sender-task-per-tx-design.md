# L1 Sender: Per-Transaction Task Design

**Status:** Proposal
**Replaces:** Submitter + Watcher split (PR #1128)

---

## Motivation

The Submitter+Watcher split in PR #1128 achieves the core goals (transient-error retry, fee-cap
blocking, timeout-triggered resubmission) but introduces indirection that is not strictly necessary:
two long-running structs, two coordination channels (`in_flight`, `resubmit`), and three shared
types (`InFlightItem`, `InFlightTx`, `ResubmitRequest`) whose sole purpose is to pass data between
the two halves.

The same behaviour can be achieved by spawning a short-lived Tokio task per transaction. Each task
owns the full submit → poll → resubmit loop for its transaction, and reports its result back via a
`oneshot` channel. The main loop awaits the receivers in submission order, preserving the ordering
guarantee without any cross-task communication.

---

## Architecture

```
run_l1_sender
│
│  (main loop)
│  1. receive up to command_limit commands
│  2. for each SendToL1 command:
│       a. estimate_gas_within_caps  ← fee-cap blocking stays here (global)
│       b. assign nonce
│       c. spawn submit_and_confirm task → oneshot::Receiver
│  3. forward Passthrough commands immediately
│  4. await receivers in order → send to outbound
```

```
submit_and_confirm task (per tx)
│
│  1. build_and_send(command, nonce, gas_params) → tx_hash
│  2. loop:
│       poll_for_receipt(tx_hash, timeout)
│       ├── Receipt  → validate, set MINED_STAGE, return envelopes
│       └── TimedOut → estimate_gas_params (raw, no cap check)
│                      resubmission_action(old, fresh)
│                      ├── SendReplacement → build_and_send(same nonce, fresh fees) → new tx_hash
│                      └── RewatchOriginal → continue loop with same tx_hash
```

---

## Key Design Decisions

### 1. Fee-cap blocking stays in the main loop

`estimate_gas_within_caps` (which blocks when network fees exceed the operator's configured cap)
is called **before** spawning each task. This keeps fee-cap blocking global: while fees are high,
no new transactions are submitted and no nonces are consumed. Doing it inside each task would allow
multiple tasks to consume nonces and then all stall indefinitely, which is harder to recover from.

### 2. Resubmission estimation inside the task (no cap check)

When a task times out and needs to decide whether to send a replacement transaction, it calls
`estimate_gas_params` directly — **not** `estimate_gas_within_caps`. The nonce slot is already
reserved; blocking on a fee cap here would leave the pipeline stuck with an unconfirmed transaction.
The cap check served its purpose before submission; now the task should just try its best.

### 3. Nonce assignment in the main loop

Nonces are assigned by the main loop before spawning, and `next_nonce` is incremented immediately.
This is safe because tasks are awaited in order — if task N fails, the error propagates before
tasks N+1, N+2, … are awaited, and the entire `run_l1_sender` returns an error (triggering a
restart that re-fetches the on-chain nonce). No nonce gaps are introduced that aren't already
present in the Submitter model.

### 4. Passthrough commands bypass the task entirely

Passthrough commands are forwarded to `outbound` in the main loop, immediately before the batch
of spawned tasks is awaited. This matches the old behaviour: passthroughs are ordered before the
real transactions in the same batch.

---

## File Map

| Action | File | Notes |
|--------|------|-------|
| **Modify** | `lib/l1_sender/src/lib.rs` | Replace `run_l1_sender` body; add `submit_and_confirm`; keep `register_operator`, `validate_tx_receipt` |
| **Modify** | `lib/l1_sender/src/types.rs` | Remove `InFlightItem`, `InFlightTx`, `ResubmitRequest`; keep `GasParams`, `Backoff` |
| **Delete** | `lib/l1_sender/src/submitter.rs` | All logic either moves into `submit_and_confirm` or was already in helpers |
| **Delete** | `lib/l1_sender/src/watcher.rs` | `poll_for_receipt` and `report_post_confirmation` move to `lib.rs` or a new `transaction.rs` |
| **Keep** | `lib/l1_sender/src/error.rs` | Unchanged |
| **Keep** | `lib/l1_sender/src/config.rs` | Unchanged (`transaction_timeout` stays) |

`resubmission_action` and `ResubmitAction` move from `submitter.rs` into `lib.rs` (or a small
`transaction.rs` module if `lib.rs` grows unwieldy).

---

## `run_l1_sender` Pseudocode

```rust
pub async fn run_l1_sender<Input: SendToL1 + Clone + Send + 'static>(
    mut inbound: PeekableReceiver<L1SenderCommand<Input>>,
    outbound: Sender<SignedBatchEnvelope<FriProof>>,
    to_address: Address,
    mut provider: FillProvider<impl TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
                               impl Provider<Ethereum> + Clone>,
    config: L1SenderConfig<Input>,
    gateway: bool,
) -> anyhow::Result<()> {
    let (operator_address, mut next_nonce) =
        register_operator::<_, Input>(&mut provider, config.operator_signer.clone()).await?;

    let mut cmd_buffer = Vec::with_capacity(config.command_limit);

    loop {
        let received = inbound.recv_many(&mut cmd_buffer, config.command_limit).await;
        if received == 0 {
            tracing::info!(command_name = Input::NAME, "inbound channel closed");
            return Ok(());
        }

        // Oneshot receivers collected in submission order for in-order awaiting.
        let mut receivers = Vec::with_capacity(cmd_buffer.len());

        for cmd in cmd_buffer.drain(..) {
            match cmd {
                L1SenderCommand::Passthrough(envelope) => {
                    // Forward immediately — no L1 transaction needed.
                    let mut e = *envelope;
                    e.set_stage(Input::PASSTHROUGH_STAGE);
                    outbound.send(e).await.context("outbound channel closed")?;
                }
                L1SenderCommand::SendToL1(mut command) => {
                    // Estimate fees in the main loop so fee-cap blocking is global.
                    let gas_params = estimate_gas_within_caps(&provider, &config).await?;
                    let nonce = next_nonce;
                    next_nonce += 1;

                    command.as_mut().iter_mut().for_each(|e| e.set_stage(Input::SENT_STAGE));

                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let provider   = provider.clone();
                    let config     = config.clone();
                    let to_address = to_address;
                    tokio::spawn(async move {
                        let result = submit_and_confirm(
                            command, nonce, gas_params, provider, config,
                            to_address, operator_address, gateway,
                        ).await;
                        let _ = tx.send(result);
                    });
                    receivers.push(rx);
                }
            }
        }

        // Await in submission order to guarantee ordering of `outbound`.
        for rx in receivers {
            let envelopes = rx.await.context("submit_and_confirm task panicked")??;
            for envelope in envelopes {
                outbound.send(envelope).await.context("outbound channel closed")?;
            }
        }
    }
}
```

---

## `submit_and_confirm` Pseudocode

```rust
async fn submit_and_confirm<Input: SendToL1>(
    command: Input,
    nonce: u64,
    initial_gas_params: GasParams,
    provider: impl Provider<Ethereum> + Clone,
    config: L1SenderConfig<Input>,
    to_address: Address,
    operator_address: Address,
    gateway: bool,
) -> anyhow::Result<Vec<SignedBatchEnvelope<FriProof>>> {
    let (mut tx_hash, mut gas_used) =
        build_and_send(&command, &initial_gas_params, nonce, &provider, &config,
                       to_address, operator_address, gateway).await?;

    loop {
        match poll_for_receipt(tx_hash, config.transaction_timeout, &provider).await? {
            PollOutcome::Receipt(receipt) => {
                validate_tx_receipt(&provider, &command, receipt).await?;
                report_post_confirmation(Input::NAME, operator_address, &provider).await;
                let mut envelopes: Vec<_> = command.into();
                envelopes.iter_mut().for_each(|e| e.set_stage(Input::MINED_STAGE));
                return Ok(envelopes);
            }
            PollOutcome::TimedOut => {
                tracing::warn!(
                    command_name = Input::NAME,
                    tx_hash = ?tx_hash,
                    nonce,
                    "transaction timed out — evaluating resubmission",
                );
                // Raw estimate — no cap blocking (nonce slot already reserved).
                let fresh = estimate_gas_params(&provider).await?;
                if let ResubmitAction::SendReplacement =
                    resubmission_action(&gas_used, &fresh)
                {
                    tracing::info!(command_name = Input::NAME, nonce,
                                   "fees rose enough — sending replacement tx");
                    let (new_hash, new_gas) =
                        build_and_send(&command, &fresh, nonce, &provider, &config,
                                       to_address, operator_address, gateway).await?;
                    tx_hash  = new_hash;
                    gas_used = new_gas;
                } else {
                    tracing::info!(command_name = Input::NAME,
                                   tx_hash = ?tx_hash,
                                   "fees unchanged — re-watching original tx");
                }
            }
        }
    }
}
```

---

## Comparison with Submitter+Watcher (PR #1128)

| | Submitter+Watcher | Per-Transaction Task |
|---|---|---|
| **Lines of new code** | ~973 (types + submitter + watcher) | ~400 (est.) |
| **Coordination channels** | 2 (`in_flight`, `resubmit`) | 0 |
| **Shared channel types** | 3 (`InFlightItem`, `InFlightTx`, `ResubmitRequest`) | 0 |
| **Fee-cap blocking** | Global (in Submitter) | Global (in main loop) |
| **Resubmission logic location** | Split (decision in Submitter, timeout in Watcher) | Co-located in `submit_and_confirm` |
| **Ordering guarantee** | Sequential Watcher | Await receivers in order |
| **Parallelism within `command_limit`** | Sequential (Watcher processes one at a time) | Parallel (tasks run concurrently) |
| **`Input` bound** | `Send + 'static` | `Clone + Send + 'static` (`Input` must be `Clone` so the task can own it while the nonce tracking stays in the main loop) |

The main additional bound is `Input: Clone` — required because the spawned task must own
`command` while the main loop may already have moved on to the next command in the buffer.
Check that the existing concrete `Input` types (`CommitCommand`, `ProveCommand`, `ExecuteCommand`)
implement `Clone`.

---

## What Gets Removed

- `lib/l1_sender/src/submitter.rs` (527 lines)
- `lib/l1_sender/src/watcher.rs` (224 lines)
- `InFlightItem`, `InFlightTx`, `ResubmitRequest` from `types.rs` (~60 lines)
- `in_flight` and `resubmit` channel setup in `lib.rs`
- The `biased select!` and `Work` enum
- `mod submitter; mod watcher;` declarations

---

## Open Questions

1. **Do `CommitCommand`, `ProveCommand`, `ExecuteCommand` implement `Clone`?**
   If not, they need to be derived before this refactor can proceed.

2. **Is parallel task execution within a batch desirable?**
   The old monolithic loop did submit multiple txs in parallel (within `command_limit`). The
   Submitter+Watcher design made it sequential. This proposal restores parallelism. Confirm this
   is acceptable — in particular, that L1 tx ordering within a batch is guaranteed by nonce (it
   is), not by submission order.

3. **`report_post_confirmation` inside the task.**
   Balance/nonce metrics are reported inside `submit_and_confirm`. If multiple tasks complete
   close together, these metrics are updated concurrently — this is benign (metrics are best-effort)
   but worth noting.
