# L1 Sender — Operational Failure Modes

This document catalogues the ways the L1 sender component can crash, hang, or enter a crash loop in production, along with recovery guidance for each.

See [l1-sender-architecture.md](l1-sender-architecture.md) for a general architectural overview.

---

## Quick Reference

| # | Failure | Severity | Crash? | Crash Loop? | Hang? | Recoverable on Restart? |
|---|---------|----------|--------|-------------|-------|------------------------|
| 1 | L1 block gap from RPC provider | High | Yes | Possible | No | Only if RPC recovers |
| 2 | In-flight tx on startup (nonce reuse) | High | Yes | Yes | No | Manual nonce investigation |
| 3 | Unsupported protocol version in `ExecuteCommand` | High | Yes | No | No | Code change required |
| 4 | Unsupported execution version in `ProveCommand` | High | Yes | No | No | Code change required |
| 5 | Transaction reverted on L1 | Critical | Yes | No | No | Manual investigation |
| 6 | Transaction inclusion timeout (300s) | Critical | Yes | Yes | No | Only if L1 congestion clears |
| 7 | Nonce conflict with external sender | Critical | Yes | Yes | No | Manual — stop external sender |
| 8 | Zero operator balance at startup | High | Yes | No | No | Fund address and restart |
| 9 | Insufficient balance during gas estimation | Medium | Yes | No | No | Fund address and restart |
| 10 | RPC call failure (any provider call) | Critical | Yes | Yes | No | Only if RPC recovers |
| 11 | Pending block returned as null by provider | Medium | Yes | No | No | Restart (provider-dependent) |
| 12 | Protocol version wait — indefinite hang | Critical | No | No | Yes | Manual — requires investigation |
| 13 | Protocol version mismatch (batch > L1) | Critical | Yes | No | No | Wait for L1 upgrade to deploy |
| 14 | UpgradeGatekeeper RPC polling failure | High | Yes | Yes | No | Only if RPC recovers |
| 15 | Inbound channel closed | High | Yes | No | No | Restart upstream |
| 16 | Unexpected passthrough ordering | Medium | Yes | No | No | Fix upstream pipeline logic |
| 17 | Malformed proof bytes (not multiple of 32) | Medium | Yes | No | No | Fix proof data |
| 18 | Metrics wei-to-gwei conversion failure | Low | Yes | No | No | Restart (may recur on large values) |
| 19 | Gas or blob fee cap below network price | Medium | No (directly) | Yes (via #6) | No | Raise cap in config and restart |

---

## Failure Modes in Detail

### 1. L1 Block Gap from RPC Provider

**Trigger:** The RPC provider (e.g., Infura) returns a gap in L1 block numbers during block polling.

**Location:** `lib.rs:55` — explicitly documented as a known issue.

**Outcome:** Crash. No automatic recovery.

**Operational impact:** All L1 submissions stall until the node is restarted and the provider is serving consistent blocks again.

**Recovery:** Restart the node. If the RPC is still exhibiting gaps, switch to a more reliable provider or wait.

---

### 2. In-Flight Transaction on Startup (Nonce Reuse Crash Loop)

**Trigger:** The node crashed while a transaction was pending in the L1 mempool. The transaction was subsequently mined. On restart, the component does not check for already-mined transactions and attempts to reuse the same nonce.

**Location:** `lib.rs:56` — explicitly documented as a known issue.

**Outcome:** The transaction is rejected (nonce too low) or the receipt poll finds a mined transaction the component does not expect. This propagates as an error and crashes the node. Because the component restarts into the same state, this can become a crash loop.

**Recovery:** Manual. Inspect the L1 explorer for the operator address to confirm what was mined. Adjust the node's internal state (e.g., stored batch counters) to match L1, then restart.

---

### 3. Unsupported Protocol Version in `ExecuteCommand`

**Trigger:** `ExecuteCommand::to_calldata_suffix()` is called for a batch with a protocol version outside the supported range [29, 30, 31, 32].

**Location:** `commands/execute.rs:146–149`

```rust
_ => panic!(
    "Unsupported protocol version: {}",
    self.batches.first().unwrap().batch.protocol_version
),
```

**Outcome:** Immediate panic. Not recoverable by restarting — the same batch will be presented again.

**Recovery:** A code update is required to handle the new protocol version, followed by redeployment.

---

### 4. Unsupported Execution Version in `ProveCommand`

**Trigger:** `ProveCommand::to_calldata_suffix()` is called with an execution version outside [4, 5, 6].

**Location:** `commands/prove.rs:148–150`

```rust
Some(execution_version) => panic!(
    "unsupported or old execution version: {execution_version}; there's no verifier defined for it"
),
```

**Outcome:** Immediate panic. Same batch is retried on restart, producing the same panic.

**Recovery:** Code update required to register the new verifier version.

---

### 5. Transaction Reverted on L1

**Trigger:** An L1 contract call (commit, prove, or execute) reverts. Common causes:

- Invalid or incorrectly encoded calldata
- Signature verification failure in the L1 contract
- Batch sequence violation (e.g., non-consecutive batch numbers)
- Contract paused or access control check failed
- State root mismatch

**Location:** `lib.rs:352–394`

```rust
if !receipt.status() {
    // fetches debug_traceTransaction, then:
    anyhow::bail!("{} L1 command transaction failed ...", ...);
}
```

**Outcome:** Crash after attempting to fetch a call trace for debugging. The transaction is permanently on-chain and cannot be undone. **This is the most serious operational failure** — it indicates a bug in calldata generation or a state inconsistency.

**Recovery:** Manual investigation of the transaction trace on an L1 explorer. The root cause must be understood before restarting, as a restart will attempt to send the same (or equivalent) transaction again.

---

### 6. Transaction Inclusion Timeout (300 seconds)

**Trigger:** A submitted L1 transaction is not mined within 300 seconds.

**Location:** `lib.rs:38, 192` — `TRANSACTION_TIMEOUT = Duration::from_secs(300)`.

**Outcome:** `PendingTransactionError` propagates upward and crashes the component. The transaction may still be in the mempool or may get mined after the crash.

**Common causes:**
- L1 network congestion
- Gas price cap set below current network price (transaction ignored by miners)
- Blob base fee spike above configured cap

**Recovery:** Restart. If congestion is the cause, the transaction may self-resolve once the network clears. If the gas cap is too low, the configuration must be updated. On restart, the in-flight transaction scenario (failure mode 2) may apply.

---

### 7. Nonce Conflict with External Sender

**Trigger:** Another process or wallet sends a transaction from the same operator address while the L1 sender is running. The L1 sender has already prepared a transaction with the same nonce.

**Location:** `lib.rs:50–52` — documented requirement: "the same provider (sender address) must not be used outside this process."

**Outcome:** The L1 sender's transaction is rejected (nonce already used) or the competing transaction is rejected. Either way, the L1 sender crashes. Because the nonce state is now out of sync, this can lead to a crash loop on restart.

**Recovery:** Ensure only one process controls each operator address. After the conflict, verify L1 state, reconcile batch acknowledgement state, and restart.

---

### 8. Zero Operator Balance at Startup

**Trigger:** The configured operator address has 0 ETH at node startup.

**Location:** `lib.rs:334–341`

```rust
if balance.is_zero() {
    anyhow::bail!("L1 sender's address {address} has zero balance");
}
```

**Outcome:** Crash immediately after startup, before any batch is processed.

**Recovery:** Fund the operator address with ETH and restart.

---

### 9. Insufficient Balance During Gas Estimation

**Trigger:** The operator address has some ETH but not enough to cover the estimated gas cost for a transaction. `provider.fill()` → `estimate_gas()` returns an error.

**Location:** `lib.rs:160`

**Outcome:** Crash. The batch is not submitted.

**Recovery:** Fund the operator address and restart. Because no transaction was sent, there is no nonce conflict on restart.

---

### 10. RPC Call Failure

**Trigger:** Any RPC call to the L1 provider fails — network timeout, rate limit, provider error, or connection drop.

**Location:** Multiple `.await?` call sites throughout `lib.rs` and `upgrade_gatekeeper.rs`. Key sites:

| Call | Location |
|------|----------|
| `estimate_eip1559_fees()` | `lib.rs:281` |
| `get_blob_base_fee()` | `lib.rs:142` |
| `provider.fill()` (nonce + gas) | `lib.rs:160` |
| `get_block(pending)` | `lib.rs:162` |
| `send_raw_transaction()` | `lib.rs:183` |
| `get_receipt()` | `lib.rs:209` |
| `get_balance()` | `lib.rs:214` |
| `get_transaction_count()` | `lib.rs:215` |

**Outcome:** Any of these crashing on a transient error will crash the component. Because these calls happen in the main event loop, a persistently flaky RPC creates a crash loop.

**Recovery:** Restart. If the RPC is flaky, switch to a more reliable endpoint. There is no built-in per-call retry logic — errors propagate immediately.

**Note:** A crash after `send_raw_transaction()` but before `get_receipt()` or downstream acknowledgement creates an in-flight transaction (failure mode 2).

---

### 11. Pending Block Returned as Null

**Trigger:** `provider.get_block(BlockId::pending())` succeeds at the RPC level but returns `None`.

**Location:** `lib.rs:162`

```rust
let pending_block = provider.get_block(BlockId::pending()).await?.expect("no pending block");
```

**Outcome:** Panic. Not all L1 RPC implementations support the `pending` block tag reliably.

**Recovery:** Restart. If the provider does not support pending blocks, switch providers.

---

### 12. Protocol Version Wait — Indefinite Hang

**Trigger:** `UpgradeGatekeeper` is waiting for the L1 contract's protocol version to reach the version of the batch being committed. The L1 upgrade is delayed indefinitely (deployment not executed, wrong version configured, etc.).

**Location:** `upgrade_gatekeeper.rs:42–76` — polling loop with 10-second sleep, no timeout.

**Outcome:** The component neither crashes nor progresses. It polls L1 every 10 seconds forever. All subsequent batches are blocked in the inbound channel.

**Symptoms:** The node appears healthy (process running, no errors) but no batches are committed to L1.

**Recovery:** Manual. Determine whether the L1 upgrade has been deployed. If the batch has the wrong protocol version, the upstream pipeline configuration must be corrected. There is no self-healing mechanism.

---

### 13. Protocol Version Mismatch (Batch Ahead of L1)

**Trigger:** A batch's protocol version is *higher* than the current L1 contract protocol version, and the gatekeeper detects this as an error rather than a wait condition.

**Location:** `upgrade_gatekeeper.rs:50–56`

```rust
Ordering::Greater => {
    anyhow::bail!(
        "Protocol version on the contract {current_protocol_version} is greater than ..."
    );
}
```

**Note:** The `Ordering` comparison direction determines whether this is "batch ahead of L1" or "L1 ahead of batch." Read the code carefully for the exact condition.

**Outcome:** Crash.

**Recovery:** Ensure the batch protocol version and L1 contract deployment are consistent. A restart will hit the same condition until the underlying mismatch is resolved.

---

### 14. UpgradeGatekeeper RPC Polling Failure

**Trigger:** The `current_protocol_version()` call inside the version polling loop fails due to an RPC error.

**Location:** `upgrade_gatekeeper.rs:74`

```rust
current_protocol_version = self.current_protocol_version().await?;
```

**Outcome:** Crash from the polling loop. A flaky RPC during a protocol upgrade window produces a crash loop: the gatekeeper restarts, tries to poll, fails, crashes again.

**Recovery:** Stabilise the RPC connection and restart.

---

### 15. Inbound Channel Closed

**Trigger:** The upstream component (e.g., `GaplessCommitter` for the commit sender) crashes or shuts down, closing its side of the channel.

**Location:** `lib.rs:99–101, 117–119`

```rust
if received == 0 {
    anyhow::bail!("inbound channel closed");
}
```

**Outcome:** Crash. No batches are in flight at this point (the receive returned cleanly), so there is no nonce risk.

**Recovery:** Restart the upstream component and the L1 sender.

---

### 16. Unexpected Passthrough Command Ordering

**Trigger:** A `Passthrough` command appears in the inbound stream *after* a `SendToL1` command. The component expects all passthroughs to come first (during the startup drain phase).

**Location:** `lib.rs:105–114`

```rust
L1SenderCommand::Passthrough(batch) => anyhow::bail!(
    "Unexpected passthrough command for batch {:?}. \
No passthrough commands are expected after the first `SendToL1`.",
    ...
),
```

**Outcome:** Crash. Indicates a bug in the upstream pipeline component that produces the commands.

**Recovery:** Investigate and fix the upstream command ordering logic.

---

### 17. Malformed Proof Bytes

**Trigger:** `ProveCommand` receives a real (non-fake) proof whose byte length is not a multiple of 32.

**Location:** `commands/prove.rs:176–178`

```rust
let arr: [u8; 32] = chunk
    .try_into()
    .expect("proof bytes must be a multiple of 32");
```

**Outcome:** Panic. The same proof will be retried on restart, producing the same panic.

**Recovery:** Investigate the proof generation pipeline for the malformed proof. The proof data must be corrected before the node can progress past this batch.

---

### 18. Metrics Conversion Failure

**Trigger:** After a transaction is successfully mined, the metrics reporter converts gas fees from wei to gwei. This can fail if `format_units()` returns an unparseable string, or if the value is so large it overflows `f64`.

**Location:** `metrics.rs:166–171`

**Outcome:** Crash after a *successful* transaction — the batch was committed to L1, but the node crashes before signalling downstream.

**Recovery:** Restart. The in-flight transaction scenario (failure mode 2) now applies: the batch is on L1 but the downstream pipeline did not receive the acknowledgement.

---

### 19. Gas or Blob Fee Cap Below Network Price

**Trigger:** `max_fee_per_gas` or `max_fee_per_blob_gas` in configuration is set below the current L1 network price.

**Location:** `lib.rs:147–153, 289–312` — warns but does not crash; uses the configured cap anyway.

**Outcome:** No crash. The transaction is submitted with the capped fee and may sit in the mempool indefinitely if miners ignore it. This eventually triggers the 300-second timeout (failure mode 6) and crashes.

**Symptoms:** Warning log: `"L1 sender's configured maxFeePerBlobGas is lower..."`, followed eventually by a timeout crash.

**Recovery:** Increase the fee caps in configuration and restart.

---

## Crash Loop Conditions

The following combinations are the most likely to produce sustained crash loops in production:

**Flaky RPC provider** → Any `.await?` RPC call fails → crash → restart → same call fails → crash → ...

**Gas fee cap below network price** → Transaction stuck in mempool → 300s timeout (failure mode 6) → crash → restart → same transaction resent (in-flight tx scenario) → crash → ...

**Transaction timeout during congestion** → Crash → restart → in-flight tx scenario → crash → ...

**Protocol version mismatch in gatekeeper** → Crash or hang → if crash: restart hits same mismatch → crash → ...

**Nonce conflict from external process** → Crash → restart → wrong nonce again → crash → ...

---

## Known Design Limitations

**No in-flight transaction tracking.** The component does not record sent transaction hashes across restarts. A crash between `send_raw_transaction` and downstream acknowledgement leaves the system in an ambiguous state that requires manual reconciliation.

**No per-call RPC retry.** Every RPC failure propagates immediately as a fatal error. A single transient timeout crashes the node.

**No adaptive timeout.** The 300-second receipt timeout is fixed. During L1 congestion it may be too short; during normal operation it may mask a stuck transaction.

**Protocol upgrade wait has no timeout.** The gatekeeper will wait forever if the L1 upgrade is not deployed, with no alerting beyond debug-level log lines.
