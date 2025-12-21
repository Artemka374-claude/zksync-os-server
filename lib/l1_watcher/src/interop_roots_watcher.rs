use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use alloy::{
    primitives::Address,
    providers::{DynProvider, Provider},
};
use zksync_os_contract_interface::{InteropRoot, NewInteropRoot};
pub const INTEROP_ROOTS_PER_IMPORT: u64 = 100;

pub struct L1InteropRootsWatcher {
    contract_address: Address,

    provider: DynProvider,
    // first number is block number, second is log index
    next_log_to_scan_from: (u64, u64),
}

impl L1InteropRootsWatcher {
    pub async fn new(
        provider: DynProvider,
        contract_address: Address,
        next_log_to_scan_from: (u64, u64),
    ) -> Self {
        Self {
            provider,
            contract_address,
            next_log_to_scan_from,
        }
    }

    pub async fn fetch_events(
        &mut self,
        from_block: u64,
        to_block: u64,
        start_log_index: u64,
    ) -> anyhow::Result<Vec<InteropRoot>> {
        let filter = Filter::new()
            .from_block(from_block)
            .to_block(to_block)
            .address(self.contract_address)
            .event_signature(NewInteropRoot::SIGNATURE_HASH);
        let logs = self.provider.get_logs(&filter).await?;

        // comment: a bit more rust-idiomatic way, but it's slower and doesn't handle updating the next_log_to_scan_from
        // let interop_roots = logs
        //     .into_iter()
        //     .filter(|log| {
        //         !(log.block_number.unwrap() == from_block
        //             && log.log_index.unwrap() <= start_log_index)
        //     })
        //     .map(|log| {
        //         NewInteropRoot::decode_log(&log.inner)
        //             .expect("Failed to decode log")
        //             .data
        //     })
        //     .take(INTEROP_ROOTS_PER_IMPORT as usize)
        //     .collect::<Vec<NewInteropRoot>>();

        let mut interop_roots = Vec::new();
        for log in logs {
            let log_block_number = log.block_number.unwrap();
            let log_log_index = log.log_index.unwrap();

            if log_block_number == from_block && log_log_index <= start_log_index {
                continue;
            }
            let interop_root_event = NewInteropRoot::decode_log(&log.inner)?.data;

            if interop_root_event.sides.len() != 1 {
                anyhow::bail!("Expected 1 side, got {}", interop_root_event.sides.len());
            }

            let interop_root = InteropRoot {
                chainId: interop_root_event.chainId,
                blockOrBatchNumber: interop_root_event.blockNumber,
                sides: interop_root_event.sides,
            };
            interop_roots.push(interop_root);

            self.next_log_to_scan_from = (log_block_number, log_log_index + 1);

            if interop_roots.len() >= INTEROP_ROOTS_PER_IMPORT as usize {
                break;
            }
        }

        if interop_roots.len() < INTEROP_ROOTS_PER_IMPORT as usize {
            self.next_log_to_scan_from = (to_block, 0);
        }

        Ok(interop_roots)
    }
}
