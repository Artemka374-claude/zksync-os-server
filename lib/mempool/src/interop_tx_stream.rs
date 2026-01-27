use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use tokio::sync::mpsc;
use zksync_os_types::{
    IndexedInteropRoot, IndexedInteropRootsEnvelope, InteropRootsEnvelope, InteropRootsLogIndex,
};

const INTEROP_ROOTS_PER_IMPORT: usize = 100;

pub struct InteropTxStream {
    receiver: mpsc::Receiver<IndexedInteropRoot>,
    pending_roots: VecDeque<IndexedInteropRoot>,
    used_roots: VecDeque<IndexedInteropRoot>,
}

impl Stream for InteropTxStream {
    type Item = IndexedInteropRootsEnvelope;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            match this.receiver.poll_recv(cx) {
                Poll::Ready(Some(root)) => {
                    if let Some(envelope) = this.add_root_and_try_take_tx(root) {
                        return Poll::Ready(Some(envelope));
                    }
                    continue;
                }
                Poll::Pending => {
                    if let Some(envelope) = this.take_tx() {
                        return Poll::Ready(Some(envelope));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(None) => {
                    return Poll::Ready(None);
                }
            }
        }
    }
}

impl InteropTxStream {
    pub fn new(receiver: mpsc::Receiver<IndexedInteropRoot>) -> Self {
        Self {
            receiver,
            pending_roots: VecDeque::new(),
            used_roots: VecDeque::new(),
        }
    }

    fn add_root_and_try_take_tx(
        &mut self,
        root: IndexedInteropRoot,
    ) -> Option<IndexedInteropRootsEnvelope> {
        self.pending_roots.push_back(root);

        if self.pending_roots.len() == INTEROP_ROOTS_PER_IMPORT {
            self.take_tx()
        } else {
            None
        }
    }

    fn take_tx(&mut self) -> Option<IndexedInteropRootsEnvelope> {
        if self.pending_roots.is_empty() {
            None
        } else {
            let tx = IndexedInteropRootsEnvelope {
                log_index: self.pending_roots.back().unwrap().log_index.clone(),
                envelope: InteropRootsEnvelope::from_interop_roots(
                    self.pending_roots.iter().map(|r| r.root.clone()).collect(),
                ),
            };

            self.used_roots.extend(self.pending_roots.drain(..));

            Some(tx)
        }
    }

    async fn take_root(&mut self) -> Option<IndexedInteropRoot> {
        if let Some(root) = self.used_roots.pop_front() {
            Some(root)
        } else if let Some(root) = self.pending_roots.pop_front() {
            Some(root)
        } else {
            self.receiver.recv().await
        }
    }

    pub async fn on_canonical_state_change(
        &mut self,
        txs: Vec<InteropRootsEnvelope>,
    ) -> Option<InteropRootsLogIndex> {
        let mut log_index = None;
        for tx in txs {
            let mut roots = Vec::new();
            for _ in 0..tx.interop_roots_count() {
                roots.push(self.take_root().await.unwrap());

                let envelope = InteropRootsEnvelope::from_interop_roots(
                    roots.iter().map(|r| r.root.clone()).collect(),
                );

                assert_eq!(&envelope, &tx);

                log_index = Some(roots.last().unwrap().log_index.clone());
            }
        }

        assert!(self.pending_roots.is_empty());

        self.pending_roots.extend(self.used_roots.drain(..));

        log_index
    }
}
