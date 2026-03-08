use crate::config::NetworkConfig;
use crate::protocol::{ProtocolEvent, ProtocolState, ZksProtocolHandler};
use crate::raft::protocol::RaftProtocolHandler;
use crate::version::ZksProtocolV1;
use crate::wire::replays::RecordOverride;
use alloy::primitives::BlockNumber;
use reth_chainspec::{ChainSpecProvider, EthChainSpec, Hardforks};
use reth_discv5::discv5;
use reth_eth_wire::HelloMessageWithProtocols;
use reth_net_nat::NatResolver;
use reth_network::error::NetworkError;
use reth_network::types::peers::config::PeerBackoffDurations;
use reth_network::{NetworkConfig as RethNetworkConfig, NetworkManager, PeersConfig};
use reth_provider::BlockNumReader;
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use zksync_os_metadata::NODE_CLIENT_VERSION;
use zksync_os_storage_api::{ReadReplay, ReplayRecord};
use zksync_os_types::NodeRole;

/// Max number of active devp2p connections.
const MAX_ACTIVE_CONNECTIONS: usize = 10;

/// Manages the entire network state including all RLPx subprotocols and discv5 peer discovery.
///
/// This type is supposed to be consumed through [`NetworkService::run`] that registers it as an
/// endless task that consistently drives the state of the entire network forward.
#[derive(Debug)]
pub struct NetworkService {
    network_manager: NetworkManager,
    protocol_rx: mpsc::UnboundedReceiver<ProtocolEvent>,
}

impl NetworkService {
    pub async fn new(
        config: NetworkConfig,
        node_role: NodeRole,
        replay: impl ReadReplay + Clone,
        starting_block: BlockNumber,
        record_overrides: Vec<RecordOverride>,
        client: impl ChainSpecProvider<ChainSpec: Hardforks> + BlockNumReader + 'static,
        replay_sender: mpsc::Sender<ReplayRecord>,
        raft_handler: Option<RaftProtocolHandler>,
    ) -> Result<Self, NetworkError> {
        tracing::info!(
            node_role = %node_role,
            listen_addr = %config.address,
            listen_port = config.port,
            boot_nodes_count = config.boot_nodes.len(),
            "initializing p2p network service"
        );
        tracing::debug!(boot_nodes = ?config.boot_nodes, "configured p2p boot nodes");

        match NatResolver::Any.external_addr().await {
            None => {
                tracing::info!("could not resolve external IP (STUN)");
            }
            Some(ip) => {
                tracing::info!(%ip, "resolved external IP (STUN)");
            }
        };
        let rlpx_address = SocketAddr::V4(SocketAddrV4::new(config.address, config.port));
        tracing::info!(%rlpx_address, "using rlpx/discovery listen address");
        let (protocol_tx, protocol_rx) = mpsc::unbounded_channel();
        let mut net_cfg = RethNetworkConfig::builder(config.secret_key)
            .boot_nodes(config.boot_nodes.clone())
            // Configure node identity
            .apply(|builder| {
                let peer_id = builder.get_peer_id();
                builder.hello_message(
                    HelloMessageWithProtocols::builder(peer_id)
                        .client_version(NODE_CLIENT_VERSION)
                        .build(),
                )
            })
            // Disable Node Discovery Protocol v4 as ZKsync OS only uses v5
            .disable_discv4_discovery()
            // Disable DNS-based discovery (EIP-1459), unused in ZKsync OS
            .disable_dns_discovery()
            // Disable built-in NAT resolver as discv5 does not need it (ENR socket address is
            // updated based on PONG responses from the majority of peers)
            .disable_nat()
            // Setup Node Discovery Protocol v5 on `localhost:<port>:UDP` that points to RLPx socket
            // at `localhost:<port>:TCP`
            .discovery_v5(
                reth_discv5::Config::builder(rlpx_address).discv5_config(
                    discv5::ConfigBuilder::new(discv5::ListenConfig::from_ip(
                        rlpx_address.ip(),
                        config.port,
                    ))
                    // Require only 2 peers to agree on our external IP to update our local ENR
                    .enr_peer_update_min(2)
                    // 2 peers from above must agree on external IP within 1h from each other.
                    // This can make the node less responsive to dynamic IP changes.
                    .vote_duration(Duration::from_secs(3600))
                    // Sets peer ban duration to 1 second, effectively disabling it
                    .ban_duration(Some(Duration::from_secs(1)))
                    .build(),
                ),
            )
            .peer_config(
                PeersConfig::default()
                    // Sets peer ban duration to 1 second, effectively disabling it
                    .with_ban_duration(Duration::from_secs(1))
                    // Tune backoff durations to be low, useful while we are in exploratory phase
                    // and infra issues are expected.
                    .with_backoff_durations(PeerBackoffDurations {
                        low: Duration::from_secs(30),
                        medium: Duration::from_secs(60),
                        high: Duration::from_secs(60 * 2),
                        max: Duration::from_secs(60 * 3),
                    }),
            )
            // Use the same port for RLPx (TCP) and for discv5 (UDP)
            .listener_addr(rlpx_address)
            .discovery_addr(rlpx_address)
            // Disable transaction gossip as it is unsupported by ZKsync OS
            .disable_tx_gossip(true)
            // Do not require any block hashes in `eth` RLPx protocol as it is unused
            .required_block_hashes(vec![])
            // Set network id to ZKsync OS chain's id, otherwise we might connect to unrelated peers
            .network_id(Some(client.chain_spec().chain_id()))
            // Add latest version of `zks` subprotocol. In the future this can be extended so that
            // several versions are registered here.
            .add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV1, _> {
                replay,
                node_role,
                starting_block: Arc::new(RwLock::new(starting_block)),
                record_overrides,
                state: ProtocolState::new(protocol_tx, MAX_ACTIVE_CONNECTIONS),
                replay_sender,
                _phantom: Default::default(),
            });
        if let Some(raft_handler) = raft_handler {
            tracing::info!("registering raft sub-protocol with network service");
            net_cfg = net_cfg.add_rlpx_sub_protocol(raft_handler);
        } else {
            tracing::info!("raft sub-protocol is not registered");
        }
        let net_cfg = net_cfg.build(client);
        tracing::debug!(?net_cfg, "starting p2p network service");
        // Create network manager. We are not interested in `txpool` because transaction gossip is
        // disabled. `request_handler` is also unused as it is specific to `eth` protocol.
        let (network_manager, _txpool, _request_handler) =
            NetworkManager::builder(net_cfg).await?.split();

        Ok(Self {
            network_manager,
            protocol_rx,
        })
    }

    /// Consume the service by registering it as an endless task that drives the network state.
    pub fn run(mut self, tasks: &mut JoinSet<()>, stop_receiver: watch::Receiver<bool>) {
        tracing::info!("starting p2p network manager tasks");
        tasks.spawn(self.network_manager);
        tasks.spawn(async move {
            while !*stop_receiver.borrow() {
                let Some(event) = self.protocol_rx.recv().await else {
                    break;
                };
                // todo: does it need to say "zk"? I thought both protocols are handled here
                match event {
                    ProtocolEvent::Established { direction, peer_id } => {
                        tracing::info!(?direction, %peer_id, "zks protocol connection established");
                    }
                    ProtocolEvent::MaxActiveConnectionsExceeded { max_connections } => {
                        tracing::warn!(
                            max_connections,
                            "zks protocol connection rejected: max active connections reached"
                        );
                    }
                }
            }
            tracing::debug!("p2p protocol event loop exited");
        });
    }
}
