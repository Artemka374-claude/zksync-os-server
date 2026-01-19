use crate::transaction::Transaction;
use crate::transaction::tx::InteropRootsTx;
use alloy::consensus::transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx};
use alloy::eips::eip2718::{Eip2718Error, Eip2718Result};
use alloy::eips::{Decodable2718, Encodable2718, Typed2718};
use alloy::primitives::ChainId;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256, address};
use alloy::rpc::types::{AccessList, SignedAuthorization};
use alloy::sol_types::SolCall;
use alloy_rlp::{BufMut, Decodable, Encodable};
use serde::{Deserialize, Serialize};
use zksync_os_contract_interface::IMessageRoot::addInteropRootCall;
use zksync_os_contract_interface::{IMessageRoot::addInteropRootsInBatchCall, InteropRoot};

pub mod tx;

pub const BOOTLOADER_FORMAL_ADDRESS: Address =
    address!("0x0000000000000000000000000000000000008001");
pub const L2_INTEROP_ROOT_STORAGE_ZKSYNC_OS_ADDRESS: Address =
    address!("0x0000000000000000000000000000000000010008");

pub const INTEROP_ROOTS_TX_TYPE_ID: u8 = 125;

// todo: Check if this value is good enough
const DEFAULT_GAS_LIMIT: u64 = 72_000_000;

#[derive(Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub struct InteropRootsEnvelope {
    /// Hash of the transaction
    /// Stored in an envelope and calculated separately from transaction as hash of transaction is not part of transaction itself.
    #[serde(skip)]
    pub hash: B256,
    /// Log index of the last event from which the transaction was created.
    /// In case we use demo-version, it is the index of the only one event in transaction.
    /// Stored in an envelope to be able to easier keep track of it, but it is not part of the transaction
    #[serde(skip)]
    pub last_log_index: InteropRootsLogIndex,
    #[serde(flatten)]
    pub inner: InteropRootsTx,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub struct InteropRootsLogIndex {
    pub block_number: u64,
    pub index_in_block: u64,
}

impl Encodable for InteropRootsLogIndex {
    fn encode(&self, out: &mut dyn BufMut) {
        self.block_number.encode(out);
        self.index_in_block.encode(out);
    }
}

impl Decodable for InteropRootsLogIndex {
    fn decode(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        Ok(Self {
            block_number: Decodable::decode(buf)?,
            index_in_block: Decodable::decode(buf)?,
        })
    }
}

impl InteropRootsEnvelope {
    pub fn from_interop_roots(
        interop_roots: Vec<InteropRoot>,
        last_log_index: InteropRootsLogIndex,
        is_gateway: bool,
    ) -> Self {
        let calldata = if is_gateway {
            addInteropRootsInBatchCall {
                interopRootsInput: interop_roots,
            }
            .abi_encode()
        } else {
            // interop roots amount should be 1 for non-gateway transactions
            assert_eq!(interop_roots.len(), 1);
            let interop_root = interop_roots[0].clone();

            addInteropRootCall {
                chainId: interop_root.chainId,
                blockOrBatchNumber: interop_root.blockOrBatchNumber,
                sides: interop_root.sides,
            }
            .abi_encode()
        };

        let transaction = InteropRootsTx {
            gas_limit: DEFAULT_GAS_LIMIT,
            to: L2_INTEROP_ROOT_STORAGE_ZKSYNC_OS_ADDRESS,
            input: Bytes::from(calldata),
        };

        Self {
            hash: transaction.calculate_hash(),
            last_log_index,
            inner: transaction,
        }
    }

    pub fn interop_roots_count(&self) -> u64 {
        if let Ok(interop_roots) = addInteropRootsInBatchCall::abi_decode(&self.inner.input) {
            interop_roots.interopRootsInput.len() as u64
        } else {
            let interop_root = addInteropRootCall::abi_decode(&self.inner.input)
                .expect("Failed to decode interop root calldata");
            // todo: should be 1 if i remember correctly
            assert_eq!(interop_root.sides.len(), 1);
            1
        }
    }

    pub fn hash(&self) -> &B256 {
        &self.hash
    }
}

impl Typed2718 for InteropRootsEnvelope {
    fn ty(&self) -> u8 {
        INTEROP_ROOTS_TX_TYPE_ID
    }
}

impl RlpEcdsaEncodableTx for InteropRootsEnvelope {
    fn rlp_encoded_fields_length(&self) -> usize {
        self.inner.rlp_encoded_fields_length()
    }

    fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.inner.rlp_encode_fields(out);
    }
}

impl RlpEcdsaDecodableTx for InteropRootsEnvelope {
    const DEFAULT_TX_TYPE: u8 = INTEROP_ROOTS_TX_TYPE_ID;

    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        let transaction = InteropRootsTx::rlp_decode_fields(buf)?;
        Ok(Self {
            hash: transaction.calculate_hash(),
            last_log_index: InteropRootsLogIndex::default(),
            inner: transaction,
        })
    }
}

impl Encodable for InteropRootsEnvelope {
    fn encode(&self, out: &mut dyn BufMut) {
        self.inner.encode(out);
    }

    fn length(&self) -> usize {
        self.inner.length()
    }
}

impl Encodable2718 for InteropRootsEnvelope {
    fn encode_2718_len(&self) -> usize {
        self.inner.encode_2718_len()
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        self.inner.encode_2718(out);
    }
}

impl Decodable2718 for InteropRootsEnvelope {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        if ty != INTEROP_ROOTS_TX_TYPE_ID {
            return Err(Eip2718Error::UnexpectedType(ty));
        }

        let transaction = InteropRootsTx::rlp_decode(buf)
            .map_err(|_| Eip2718Error::RlpError(alloy::rlp::Error::Custom("decode failed")))?;

        let hash = transaction.calculate_hash();

        Ok(Self {
            hash,
            last_log_index: InteropRootsLogIndex::default(),
            inner: transaction,
        })
    }

    fn fallback_decode(_buf: &mut &[u8]) -> Eip2718Result<Self> {
        // Do not try to decode untyped transactions
        Err(Eip2718Error::UnexpectedType(0))
    }
}

impl Transaction for InteropRootsEnvelope {
    fn chain_id(&self) -> Option<ChainId> {
        self.inner.chain_id()
    }

    fn nonce(&self) -> u64 {
        self.inner.nonce()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn gas_price(&self) -> Option<u128> {
        self.inner.gas_price()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.inner.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.inner.max_priority_fee_per_gas()
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.inner.max_fee_per_blob_gas()
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.inner.priority_fee_or_price()
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        self.inner.effective_gas_price(base_fee)
    }

    fn is_dynamic_fee(&self) -> bool {
        self.inner.is_dynamic_fee()
    }

    fn kind(&self) -> TxKind {
        self.inner.kind()
    }

    fn is_create(&self) -> bool {
        self.inner.is_create()
    }

    fn value(&self) -> U256 {
        self.inner.value()
    }

    fn input(&self) -> &Bytes {
        self.inner.input()
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.inner.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.inner.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        self.inner.authorization_list()
    }
}

#[cfg(test)]
mod tests {
    use crate::InteropRootsEnvelope;
    use crate::transaction::InteropRootsLogIndex;
    use crate::transaction::tx::InteropRootsTx;

    #[test]
    fn interop_roots_tx_serialization() {
        // Interop roots serialization should be consistent with Ethereum JSON-RPC spec
        // See https://ethereum.github.io/execution-apis/api-documentation/

        let transaction = InteropRootsTx {
            gas_limit: 0x10000,
            to: Default::default(),
            input: Default::default(),
        };

        let tx = InteropRootsEnvelope {
            hash: transaction.calculate_hash(),
            last_log_index: InteropRootsLogIndex::default(),
            inner: transaction,
        };

        assert_eq!(
            serde_json::to_string_pretty(&tx).unwrap(),
            r#"{
  "hash": "0xd06b6df7ff36db8daee83e3a8d5d0b1e349e57968054b3da83192341e195a848",
  "initiator": "0x0000000000000000000000000000000000008001",
  "to": "0x0000000000000000000000000000000000000000",
  "gas": "0x10000",
  "maxFeePerGas": "0x0",
  "maxPriorityFeePerGas": "0x0",
  "nonce": "0x0",
  "value": "0x0",
  "input": "0x",
  "v": "0x0",
  "r": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "s": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "yParity": "0x0"
}"#
        );
    }
}
