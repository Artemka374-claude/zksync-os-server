use async_trait::async_trait;
use openraft::Raft;
use tokio::sync::mpsc;
use zksync_os_consensus_types::RaftTypeConfig;
use zksync_os_sequencer::execution::{BlockCanonization, NoopCanonization};
use zksync_os_storage_api::ReplayRecord;

pub struct OpenRaftCanonizationEngine {
    // Reference to the openraft implementation
    pub(crate) raft: Raft<RaftTypeConfig>,
    // Newly canonized blocks are sent to this channel and exposed via `BlockCanonization::next_canonized()`
    pub(crate) canonized_blocks_rx: mpsc::Receiver<ReplayRecord>,
}

#[async_trait]
impl BlockCanonization for OpenRaftCanonizationEngine {
    async fn propose(&self, record: ReplayRecord) -> anyhow::Result<()> {
        self.raft.client_write(record).await?;
        Ok(())
    }

    async fn next_canonized(&mut self) -> anyhow::Result<ReplayRecord> {
        self.canonized_blocks_rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("raft applied channel closed"))
    }
}

pub enum BlockCanonizationEngine {
    Noop(NoopCanonization),
    OpenRaft(OpenRaftCanonizationEngine),
}

#[async_trait]
impl BlockCanonization for BlockCanonizationEngine {
    async fn propose(&self, record: ReplayRecord) -> anyhow::Result<()> {
        match self {
            BlockCanonizationEngine::Noop(canonization) => canonization.propose(record).await,
            BlockCanonizationEngine::OpenRaft(canonization) => canonization.propose(record).await,
        }
    }

    async fn next_canonized(&mut self) -> anyhow::Result<ReplayRecord> {
        match self {
            BlockCanonizationEngine::Noop(canonization) => canonization.next_canonized().await,
            BlockCanonizationEngine::OpenRaft(canonization) => canonization.next_canonized().await,
        }
    }
}
