use std::marker::PhantomData;

use crate::transaction::{system::envelope::SystemTransactionEnvelope, tx::SystemTransaction};
use alloy::primitives::{Address, Bytes, address};
use alloy::sol_types::SolCall;
use serde::{Deserialize, Serialize};
use zksync_os_contract_interface::{IMessageRoot::addInteropRootsInBatchCall, InteropRoot};

pub mod envelope;
pub mod tx;

pub const BOOTLOADER_FORMAL_ADDRESS: Address =
    address!("0x0000000000000000000000000000000000008001");
pub const L2_INTEROP_ROOT_STORAGE_ZKSYNC_OS_ADDRESS: Address =
    address!("0x0000000000000000000000000000000000010008");

pub type InteropRootsEnvelope = SystemTransactionEnvelope<InteropRootsTxType>;

impl InteropRootsEnvelope {
    pub fn from_interop_roots(interop_roots: Vec<InteropRoot>) -> Self {
        let calldata = addInteropRootsInBatchCall {
            interopRootsInput: interop_roots,
        }
        .abi_encode();

        let transaction = SystemTransaction {
            gas_limit: 0,
            to: L2_INTEROP_ROOT_STORAGE_ZKSYNC_OS_ADDRESS,
            input: Bytes::from(calldata),
            marker: PhantomData,
        };

        Self {
            hash: transaction.calculate_hash(),
            inner: transaction,
        }
    }

    pub fn interop_roots_count(&self) -> u64 {
        let interop_roots = addInteropRootsInBatchCall::abi_decode(&self.inner.input)
            .expect("Failed to decode interop roots calldata")
            .interopRootsInput;
        interop_roots.len() as u64
    }
}

pub trait SystemTxType: Clone + Send + Sync + std::fmt::Debug + 'static {
    const TX_TYPE: u8;
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub struct InteropRootsTxType;

impl SystemTxType for InteropRootsTxType {
    const TX_TYPE: u8 = 0x7d;
}

#[cfg(test)]
mod tests {
    use crate::InteropRootsEnvelope;
    use crate::transaction::tx::SystemTransaction;

    #[test]
    fn interop_roots_tx_serialization() {
        // Interop roots serialization should be consistent with Ethereum JSON-RPC spec
        // See https://ethereum.github.io/execution-apis/api-documentation/
        let tx = InteropRootsEnvelope {
            hash: Default::default(),
            inner: SystemTransaction {
                gas_limit: 0x10000,
                to: Default::default(),
                input: Default::default(),
                marker: Default::default(),
            },
        };
        assert_eq!(
            serde_json::to_string_pretty(&tx).unwrap(),
            r#"{
  "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "nonce": "0x0",
  "to": "0x0000000000000000000000000000000000000000",
  "gas": "0x10000",
  "value": "0x0",
  "input": "0x",
  "maxPriorityFeePerGas": "0x0",
  "maxFeePerGas": "0x0",
  "gasPrice": "0x0",
  "chainId": "0x270",
  "yParity": "0x0",
  "r": "0x0",
  "s": "0x0",
            }"#
        );
    }
}
