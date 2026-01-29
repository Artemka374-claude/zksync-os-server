use std::sync::Arc;

use alloy::primitives::BlockNumber;
use alloy::providers::Provider;
use alloy::rpc::types::Log;
use alloy::{primitives::Address, providers::DynProvider};
use tokio::sync::mpsc;
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;
use zksync_os_contract_interface::{Bridgehub, InteropRoot, MessageRoot};
use zksync_os_types::IndexedInteropRoot;

use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event};

pub struct InteropWatcher {
    contract_address: Address,
    output: mpsc::Sender<IndexedInteropRoot>,
    starting_interop_root_id: u64,
}

impl InteropWatcher {
    pub async fn create_watcher(
        bridgehub: Bridgehub<DynProvider>,
        config: L1WatcherConfig,
        output: mpsc::Sender<IndexedInteropRoot>,
        starting_interop_root_id: u64,
    ) -> anyhow::Result<L1Watcher> {
        let contract_address = bridgehub.message_root_address().await?;

        tracing::info!(
            contract_address = ?contract_address,
            starting_interop_root_id = ?starting_interop_root_id,
            "initializing interop watcher"
        );

        let this = Self {
            contract_address,
            output,
            starting_interop_root_id,
        };

        let next_l1_block =
            find_l1_block_by_interop_root_id(bridgehub.clone(), starting_interop_root_id).await?;

        Ok(L1Watcher::new(
            bridgehub.provider().clone(),
            next_l1_block,
            config.max_blocks_to_process,
            config.poll_interval,
            this.into(),
        ))
    }
}

async fn find_l1_block_by_interop_root_id(
    bridgehub: Bridgehub<DynProvider>,
    next_interop_root_id: u64,
) -> anyhow::Result<BlockNumber> {
    let message_root_address = bridgehub.message_root_address().await?;
    let message_root = MessageRoot::new(message_root_address, bridgehub.provider().clone());

    find_l1_block_by_predicate(
        Arc::new(message_root),
        0,
        move |message_root, block| async move {
            let res = message_root
                .total_published_interop_roots(block.into())
                .await?;
            Ok(res >= next_interop_root_id)
        },
    )
    .await
}

pub async fn find_l1_block_by_predicate<Fut: Future<Output = anyhow::Result<bool>>>(
    message_root: Arc<MessageRoot<DynProvider>>,
    start_block_number: BlockNumber,
    predicate: impl Fn(Arc<MessageRoot<DynProvider>>, u64) -> Fut,
) -> anyhow::Result<BlockNumber> {
    let latest = message_root.provider().get_block_number().await?;

    let guarded_predicate =
        async |message_root: Arc<MessageRoot<DynProvider>>, block: u64| -> anyhow::Result<bool> {
            if !message_root.code_exists_at_block(block.into()).await? {
                // return early if contract is not deployed yet - otherwise `predicate` might fail
                return Ok(false);
            }
            predicate(message_root, block).await
        };

    // Ensure the predicate is true by the upper bound, or bail early.
    if !guarded_predicate(message_root.clone(), latest).await? {
        anyhow::bail!(
            "Condition not satisfied up to latest block: contract not deployed yet \
             or target not reached.",
        );
    }

    // Binary search on [0, latest] for the first block where predicate is true.
    let (mut lo, mut hi) = (start_block_number, latest);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if guarded_predicate(message_root.clone(), mid).await? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }

    Ok(lo)
}

#[async_trait::async_trait]
impl ProcessL1Event for InteropWatcher {
    const NAME: &'static str = "interop_root";

    type SolEvent = NewInteropRoot;
    type WatchedEvent = NewInteropRoot;

    fn contract_address(&self) -> Address {
        self.contract_address
    }

    async fn process_event(&mut self, tx: NewInteropRoot, _log: Log) -> Result<(), L1WatcherError> {
        if tx.logId < self.starting_interop_root_id {
            tracing::debug!(
                log_id = ?tx.logId,
                starting_interop_root_id = self.starting_interop_root_id,
                "skipping interop root event before starting index",
            );
            return Ok(());
        }
        let interop_root = InteropRoot {
            chainId: tx.chainId,
            blockOrBatchNumber: tx.blockNumber,
            sides: tx.sides.clone(),
        };

        self.output
            .send(IndexedInteropRoot {
                log_id: tx.logId.try_into().unwrap(),
                root: interop_root,
            })
            .await
            .map_err(|_| L1WatcherError::OutputClosed)?;

        Ok(())
    }
}
