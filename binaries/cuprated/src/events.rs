//! Node event streaming.
//!
//! [`Node::events`](crate::Node::events) returns a [`NodeEventListener`] -- a forward-looking
//! subscription to [`NodeEvent`]s published by node subsystems.

use tokio::sync::broadcast;

/// Capacity of the node-event broadcast channel.
///
/// Slow consumers that fall this far behind receive
/// [`RecvError::Lagged`](broadcast::error::RecvError::Lagged).
pub const NODE_EVENT_CHANNEL_CAPACITY: usize = 256;

/// An event emitted by a running node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeEvent {
    /// A new block was accepted at the live main-chain tip via the incoming-block path —
    /// whether relayed by a peer or submitted locally (e.g. the `submit_block` RPC).
    ///
    /// Not emitted for blocks applied during initial batch-sync or for blocks re-applied
    /// during a reorg.
    NewBlock {
        /// Height of the new block.
        height: usize,
        /// Hash of the new block.
        hash: [u8; 32],
    },
    /// The main chain was reorganized onto a heavier alternative chain.
    Reorg {
        /// Height of the first block that differs between the old and new chains.
        split_height: usize,
        /// Hash of the new main-chain tip after the reorg.
        new_top_hash: [u8; 32],
        /// Main-chain height after the reorg.
        new_chain_height: usize,
    },
}

/// The publishing half of the node-event channel. Cheaply cloneable.
#[derive(Clone)]
pub(crate) struct NodeEventSender {
    tx: broadcast::Sender<NodeEvent>,
}

impl NodeEventSender {
    /// Create a new sender backed by a fresh broadcast channel.
    pub(crate) fn new() -> Self {
        let (tx, _rx) = broadcast::channel(NODE_EVENT_CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Publish an event. A send with no active listeners is a no-op (not an error).
    pub(crate) fn send(&self, event: NodeEvent) {
        let _ = self.tx.send(event);
    }

    /// Subscribe a new, forward-looking listener.
    pub fn subscribe(&self) -> NodeEventListener {
        NodeEventListener {
            rx: self.tx.subscribe(),
        }
    }
}

/// A subscription to [`NodeEvent`]s. Obtain one from [`Node::events`](crate::Node::events).
#[must_use]
pub struct NodeEventListener {
    rx: broadcast::Receiver<NodeEvent>,
}

impl NodeEventListener {
    /// Await the next event. See [`broadcast::Receiver::recv`].
    pub async fn recv(&mut self) -> Result<NodeEvent, broadcast::error::RecvError> {
        self.rx.recv().await
    }

    /// Try to receive the next event without waiting.
    pub fn try_recv(&mut self) -> Result<NodeEvent, broadcast::error::TryRecvError> {
        self.rx.try_recv()
    }

    /// Create a new listener that receives events from now on, sharing the same channel.
    pub fn resubscribe(&self) -> Self {
        Self {
            rx: self.rx.resubscribe(),
        }
    }
}
