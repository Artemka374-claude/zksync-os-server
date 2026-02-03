use crate::ReadRpcStorage;
use crate::log_proof_utils::{
    batch_tree_proof, chain_proof_vector, find_sl_block_number_with_execute, get_chain_log_proof,
};
use crate::result::ToRpcResult;
use alloy::primitives::{Address, B256, BlockNumber, TxHash, U256, keccak256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Index;
use anyhow::Context;
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use std::sync::Arc;
use zksync_os_batch_types::DiscoveredCommittedBatch;
use zksync_os_genesis::{GenesisInput, GenesisInputSource};
use zksync_os_l1_watcher::CommittedBatchProvider;
use zksync_os_mini_merkle_tree::MiniMerkleTree;
use zksync_os_rpc_api::{types::BlockMetadata, types::L2ToL1LogProof, zks::ZksApiServer};
use zksync_os_storage_api::{RepositoryError, StateError, read_aggregated_root};
use zksync_os_types::L2_TO_L1_TREE_SIZE;

const LOG_PROOF_SUPPORTED_METADATA_VERSION: u8 = 1;

pub struct ZksNamespace<RpcStorage> {
    bridgehub_address: Address,
    bytecode_supplier_address: Address,
    committed_batch_provider: CommittedBatchProvider,
    storage: RpcStorage,
    genesis_input_source: Arc<dyn GenesisInputSource>,
    l2_chain_id: u64,
    gateway_provider: Option<DynProvider>,
}

impl<RpcStorage> ZksNamespace<RpcStorage> {
    pub fn new(
        bridgehub_address: Address,
        bytecode_supplier_address: Address,
        committed_batch_provider: CommittedBatchProvider,
        storage: RpcStorage,
        genesis_input_source: Arc<dyn GenesisInputSource>,
        l2_chain_id: u64,
        gateway_provider: Option<DynProvider>,
    ) -> Self {
        Self {
            bridgehub_address,
            bytecode_supplier_address,
            committed_batch_provider,
            storage,
            genesis_input_source,
            l2_chain_id,
            gateway_provider,
        }
    }
}

impl<RpcStorage: ReadRpcStorage> ZksNamespace<RpcStorage> {
    async fn get_l2_to_l1_log_proof_impl(
        &self,
        tx_hash: TxHash,
        index: Index,
    ) -> ZksResult<Option<L2ToL1LogProof>> {
        let Some(tx_meta) = self.storage.repository().get_transaction_meta(tx_hash)? else {
            return Ok(None);
        };
        let block_number = tx_meta.block_number;
        if self
            .storage
            .finality()
            .get_finality_status()
            .last_executed_block
            < block_number
        {
            return Err(ZksError::NotExecutedYet);
        }
        // Try fetching from `CommittedBatchProvider` first. This should be enough to answer requests
        // about recent blocks. Fallback to batch storage after which might not have the batch yet
        // if node is still indexing historical batches.
        let batch = match self
            .committed_batch_provider
            .get_by_block_number(block_number)
        {
            None => self
                .storage
                .batch()
                .get_batch_by_block_number(block_number)?
                .ok_or(ZksError::BlockNotAvailableYet)?,
            Some(batch) => batch,
        };
        let mut batch_index = None;
        let mut merkle_tree_leaves = vec![];
        let batch_number = batch.number();
        for block in batch.block_range.clone() {
            let Some(block) = self.storage.repository().get_block_by_number(block)? else {
                return Err(ZksError::BlockNotAvailable(block));
            };
            for block_tx_hash in block.unseal().body.transactions {
                let Some(receipt) = self
                    .storage
                    .repository()
                    .get_transaction_receipt(block_tx_hash)?
                else {
                    return Err(ZksError::TxNotAvailable(block_tx_hash));
                };
                let l2_to_l1_logs = receipt.into_l2_to_l1_logs();
                if block_tx_hash == tx_hash {
                    if index.0 >= l2_to_l1_logs.len() {
                        return Err(ZksError::IndexOutOfBounds(index.0, l2_to_l1_logs.len()));
                    }
                    batch_index.replace(merkle_tree_leaves.len() + index.0);
                }
                for l2_to_l1_log in l2_to_l1_logs {
                    merkle_tree_leaves.push(l2_to_l1_log.encode());
                }
            }
        }
        let l1_log_index = batch_index
            .expect("transaction not found in the batch that was supposed to contain it");

        let (local_root, proof) =
            MiniMerkleTree::new(merkle_tree_leaves.into_iter(), Some(L2_TO_L1_TREE_SIZE))
                .merkle_root_and_path(l1_log_index);

        let state = self.storage.state_view_at(*batch.block_range.end())?;
        // The result should be Keccak(l2_l1_local_root, aggregated_root).
        let aggregated_root = read_aggregated_root(state);
        let root = keccak256([local_root.0, aggregated_root.0].concat());

        let log_leaf_proof = proof
            .into_iter()
            .chain(std::iter::once(aggregated_root))
            .collect::<Vec<_>>();

        // todo: provide batch chain proof when ran on top of gateway
        let (batch_proof_len, batch_chain_proof, is_final_node) = match &self.gateway_provider {
            Some(gateway_provider) => {
                let Some(gateway_block_number_with_event) = find_sl_block_number_with_execute(
                    batch.sl_commit_block_number,
                    self.l2_chain_id,
                    batch_number,
                    gateway_provider,
                )
                .await?
                else {
                    return Ok(None);
                };
                let gateway_batch: DiscoveredCommittedBatch = gateway_provider
                    .raw_request(
                        "unstable_getBatchByBlockNumber".into(),
                        gateway_block_number_with_event,
                    )
                    .await
                    .context("unstable_getBatchByBlockNumber")?;

                let mut chain_log_proof = get_chain_log_proof(
                    self.l2_chain_id,
                    gateway_batch.last_block_number(),
                    gateway_provider,
                )
                .await?;

                let gw_local_root: B256 = gateway_provider
                    .raw_request("unstable_getLocalRoot".into(), gateway_batch.number())
                    .await
                    .context("unstable_getLocalRoot")?;

                // Chain tree is the right subtree of the aggregated tree.
                // We append root of the left subtree to form full proof.
                chain_log_proof.chain_id_leaf_proof_mask |=
                    U256::from(1u64 << chain_log_proof.chain_id_leaf_proof.len());
                chain_log_proof.chain_id_leaf_proof.push(gw_local_root);

                let chain_proof_vector =
                    chain_proof_vector(gateway_batch.number(), chain_log_proof, gateway_provider)
                        .await?;

                let (mut batch_chain_proof, batch_proof_len) = batch_tree_proof(
                    gateway_batch.block_range,
                    self.l2_chain_id,
                    batch_number,
                    gateway_provider,
                )
                .await?;
                batch_chain_proof.extend(chain_proof_vector);

                (batch_proof_len, batch_chain_proof, false)
            }
            None => (0, Vec::<B256>::new(), true),
        };

        let proof = {
            let mut metadata = [0u8; 32];
            metadata[0] = LOG_PROOF_SUPPORTED_METADATA_VERSION;
            metadata[1] = log_leaf_proof.len() as u8;
            metadata[2] = batch_proof_len;
            metadata[3] = if is_final_node { 1 } else { 0 };

            let mut result = vec![B256::new(metadata)];

            result.extend(log_leaf_proof);
            result.extend(batch_chain_proof);

            result
        };

        Ok(Some(L2ToL1LogProof {
            batch_number,
            proof,
            root,
            id: l1_log_index as u32,
        }))
    }

    async fn get_block_metadata_by_number_imp(
        &self,
        block_number: u64,
    ) -> ZksResult<Option<BlockMetadata>> {
        let Some(block) = self
            .storage
            .replay_storage()
            .get_replay_record(block_number)
        else {
            return Ok(None);
        };

        let pubdata_price_per_byte = block.block_context.pubdata_price;
        let native_price = block.block_context.native_price;
        let execution_version = block.block_context.execution_version;
        Ok(Some(BlockMetadata {
            pubdata_price_per_byte,
            native_price,
            execution_version,
        }))
    }
}

#[async_trait]
impl<RpcStorage: ReadRpcStorage> ZksApiServer for ZksNamespace<RpcStorage> {
    async fn get_bridgehub_contract(&self) -> RpcResult<Address> {
        Ok(self.bridgehub_address)
    }

    async fn get_bytecode_supplier_contract(&self) -> RpcResult<Address> {
        Ok(self.bytecode_supplier_address)
    }

    async fn get_l2_to_l1_log_proof(
        &self,
        tx_hash: TxHash,
        index: Index,
    ) -> RpcResult<Option<L2ToL1LogProof>> {
        self.get_l2_to_l1_log_proof_impl(tx_hash, index)
            .await
            .to_rpc_result()
    }

    async fn get_genesis(&self) -> RpcResult<GenesisInput> {
        self.genesis_input_source
            .genesis_input()
            .await
            .map_err(ZksError::GenesisSource)
            .to_rpc_result()
    }

    async fn get_block_metadata_by_number(
        &self,
        block_number: u64,
    ) -> RpcResult<Option<BlockMetadata>> {
        self.get_block_metadata_by_number_imp(block_number)
            .await
            .to_rpc_result()
    }
}

/// `zks` namespace result type.
pub type ZksResult<Ok> = Result<Ok, ZksError>;

/// General `zks` namespace errors
#[derive(Debug, thiserror::Error)]
pub enum ZksError {
    #[error("L1 batch containing the transaction has not been executed yet")]
    NotExecutedYet,
    /// Block is executed according to L1 but hasn't been indexed by this node yet. Client needs to
    /// retry after some time passes.
    #[error("L1 batch containing the transaction has not been indexed by this node yet")]
    BlockNotAvailableYet,
    /// Historical block could not be found on this node (e.g., pruned).
    #[error("historical block {0} is not available")]
    BlockNotAvailable(BlockNumber),
    /// Historical transaction could not be found on this node (e.g., pruned).
    #[error("historical transaction {0} is not available")]
    TxNotAvailable(TxHash),
    /// Historical transaction could not be found on this node (e.g., pruned).
    #[error(
        "provided L2->L1 log index ({0}) does not exist; there are only {1} L2->L1 logs in the transaction"
    )]
    IndexOutOfBounds(usize, usize),

    #[error(transparent)]
    Batch(#[from] anyhow::Error),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    GenesisSource(anyhow::Error),
    #[error(transparent)]
    State(#[from] StateError),
}
