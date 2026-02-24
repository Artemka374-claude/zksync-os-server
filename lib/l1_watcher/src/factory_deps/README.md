# Factory dependencies fallback

The watcher now fetches force-deployment bytecodes from `BytecodeSupplier` logs first.
This directory remains as a compatibility fallback for environments where supplier
publications are incomplete or missing.

To update the contracts, go to the `zksync-os-stable` branch of `era-contracts` and do the following:

```
# in era-contracts/l1-contracts
yarn write-factory-deps-zksync-os --output <path to the contracts.json>
```

The fact that the hashes in this code correspond to the actual hashes used in the
upgrade is to be checked by the person that prepares the upgrade (e.g. via
[protocol upgrade verification tool](https://github.com/matter-labs/protocol-upgrade-verification-tool)).
