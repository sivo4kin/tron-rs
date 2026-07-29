//! Live channel service: persistent peer connections + block/tx gossip.
//!
//! Turns the request/response primitives in [`crate::channel`] into a running
//! network participant (java-tron `org.tron.core.net`: `PeerConnection` /
//! `ChannelManager` / `AdvService`). The service:
//! - accepts inbound TCP and dials outbound peers (from the discovered peer set),
//! - completes the [`MessageType::P2pHello`] handshake,
//! - runs a per-peer loop dispatching frames, with a keepalive ping tick,
//! - exposes [`ChannelHandle::advertise_block`] / [`ChannelHandle::advertise_tx`]
//!   to gossip new inventory to every live peer,
//! - serves `FetchInvData` from, and applies inbound `Block`s through, an injected
//!   [`ChannelHandler`] (the node supplies the H05 intake gate + mempool there,
//!   keeping this crate free of consensus/state dependencies).
//!
//! **Frames handled:** `P2pHello` (handshake), `P2pPing`/`P2pPong` (keepalive),
//! `BlockInventory` (advertise → peers fetch), `FetchInvData` (serve a block),
//! `Block` (apply via the gate, then re-advertise), `Trx` (push to mempool).
//! **Deferred:** `SyncBlockChain` bulk catch-up (the node's periodic
//! `sync_from_best_peer` still covers gap recovery), transaction *inventory*
//! hash-then-fetch (tx is pushed whole), and peer scoring/timeout eviction.

use crate::channel::{accept_handshake, read_frame, write_frame, ChannelError};
use crate::message::{Frame, FrameError, MessageType};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// Node-supplied block/tx sink and block source. All methods are called from the
/// per-peer tasks, so implementors must be cheap and non-blocking (lock, mutate,
/// return). The node's impl runs the intake gate and mempool here.
pub trait ChannelHandler: Send + Sync + 'static {
    /// Our current head block number (`-1` if none).
    fn head(&self) -> i64;
    /// Apply an inbound encoded block. Returns the new head number if it was
    /// accepted (stored), or `None` if it was rejected / out of order.
    fn on_block(&self, block_bytes: &[u8]) -> Option<i64>;
    /// Admit an inbound encoded transaction (e.g. into the mempool).
    fn on_transaction(&self, tx_bytes: &[u8]);
    /// Encoded block for `number`, if we have it (to serve `FetchInvData`).
    fn block_bytes(&self, number: i64) -> Option<Vec<u8>>;
}

/// One gossip event fanned out to every live peer task.
#[derive(Clone)]
enum Advertise {
    Block(i64),
    Tx(Arc<Vec<u8>>),
}

/// A cloneable handle to advertise inventory to all connected peers.
#[derive(Clone)]
pub struct ChannelHandle {
    adv_tx: broadcast::Sender<Advertise>,
}

impl ChannelHandle {
    /// Gossip a new block's number as inventory; peers missing it will fetch it.
    pub fn advertise_block(&self, number: i64) {
        let _ = self.adv_tx.send(Advertise::Block(number));
    }

    /// Gossip a new transaction (pushed whole) to all peers.
    pub fn advertise_tx(&self, tx_bytes: Vec<u8>) {
        let _ = self.adv_tx.send(Advertise::Tx(Arc::new(tx_bytes)));
    }

    /// Number of live peer tasks currently subscribed (for tests / metrics).
    pub fn live_peers(&self) -> usize {
        self.adv_tx.receiver_count()
    }
}

/// Channel-service tuning.
pub struct ChannelConfig {
    /// Interval between keepalive pings on each connection.
    pub keepalive: Duration,
    /// Our `P2pHello` payload.
    pub hello: Vec<u8>,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self { keepalive: Duration::from_secs(10), hello: b"tron-rs/p2p".to_vec() }
    }
}

/// The channel service. Construct with [`ChannelService::new`], then [`run`](Self::run).
pub struct ChannelService<H: ChannelHandler> {
    handler: Arc<H>,
    adv_tx: broadcast::Sender<Advertise>,
    config: Arc<ChannelConfig>,
}

impl<H: ChannelHandler> ChannelService<H> {
    /// Build the service and a [`ChannelHandle`] for advertising into it.
    pub fn new(handler: Arc<H>, config: ChannelConfig) -> (Self, ChannelHandle) {
        let (adv_tx, _) = broadcast::channel(256);
        let handle = ChannelHandle { adv_tx: adv_tx.clone() };
        (Self { handler, adv_tx, config: Arc::new(config) }, handle)
    }

    fn spawn_peer(&self, stream: TcpStream, initiator: bool, token: CancellationToken) {
        let handler = self.handler.clone();
        let adv_rx = self.adv_tx.subscribe();
        let config = self.config.clone();
        let handle = ChannelHandle { adv_tx: self.adv_tx.clone() };
        tokio::spawn(async move {
            let _ = handle_peer(stream, handler, adv_rx, config, token, initiator, handle).await;
        });
    }

    /// Accept inbound connections on `listener` and dial each address in `dial`,
    /// running every peer until `token` is cancelled.
    pub async fn run(self, listener: TcpListener, dial: Vec<SocketAddr>, token: CancellationToken) {
        for addr in dial {
            match TcpStream::connect(addr).await {
                Ok(stream) => self.spawn_peer(stream, true, token.clone()),
                Err(e) => tracing::debug!(%addr, error = %e, "channel dial failed"),
            }
        }
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                res = listener.accept() => match res {
                    Ok((stream, _peer)) => self.spawn_peer(stream, false, token.clone()),
                    Err(e) => tracing::debug!(error = %e, "channel accept failed"),
                }
            }
        }
    }
}

/// Drive a single peer connection: handshake, then a select loop over inbound
/// frames, the advertise fan-out, keepalive, and shutdown.
async fn handle_peer<H: ChannelHandler>(
    mut stream: TcpStream,
    handler: Arc<H>,
    mut adv_rx: broadcast::Receiver<Advertise>,
    config: Arc<ChannelConfig>,
    token: CancellationToken,
    initiator: bool,
    handle: ChannelHandle,
) -> Result<(), ChannelError> {
    // Handshake on the whole stream before splitting.
    if initiator {
        write_frame(&mut stream, &Frame::new(MessageType::P2pHello, config.hello.clone())).await?;
        let peer = read_frame(&mut stream).await?;
        if peer.kind != MessageType::P2pHello {
            return Err(ChannelError::Frame(FrameError::UnknownType(peer.kind as u8)));
        }
    } else {
        accept_handshake(&mut stream, config.hello.clone()).await?;
    }

    let (mut reader, mut writer) = stream.into_split();

    // A dedicated read task keeps `read_frame` off the select loop (read_exact is
    // not cancel-safe); parsed frames flow over an mpsc.
    let (in_tx, mut in_rx) = mpsc::channel::<Frame>(64);
    let read_token = token.clone();
    let read_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = read_token.cancelled() => break,
                res = read_frame(&mut reader) => match res {
                    Ok(frame) => {
                        if in_tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    });

    let mut keepalive = tokio::time::interval(config.keepalive);
    keepalive.tick().await; // consume the immediate first tick

    let result = loop {
        tokio::select! {
            _ = token.cancelled() => break Ok(()),
            _ = keepalive.tick() => {
                if write_frame(&mut writer, &Frame::new(MessageType::P2pPing, vec![])).await.is_err() {
                    break Ok(());
                }
            }
            adv = adv_rx.recv() => match adv {
                Ok(Advertise::Block(n)) => {
                    let inv = Frame::new(MessageType::BlockInventory, encode_numbers(&[n]));
                    if write_frame(&mut writer, &inv).await.is_err() {
                        break Ok(());
                    }
                }
                Ok(Advertise::Tx(bytes)) => {
                    let frame = Frame::new(MessageType::Trx, (*bytes).clone());
                    if write_frame(&mut writer, &frame).await.is_err() {
                        break Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break Ok(()),
            },
            inbound = in_rx.recv() => match inbound {
                Some(frame) => {
                    if handle_frame(&frame, &handler, &mut writer, &handle).await.is_err() {
                        break Ok(());
                    }
                }
                None => break Ok(()), // read task ended (EOF / error)
            }
        }
    };

    read_task.abort();
    result
}

/// Handle one inbound frame, writing any reply on `writer`.
async fn handle_frame<H, W>(
    frame: &Frame,
    handler: &Arc<H>,
    writer: &mut W,
    handle: &ChannelHandle,
) -> Result<(), ChannelError>
where
    H: ChannelHandler,
    W: AsyncWriteExt + Unpin,
{
    match frame.kind {
        MessageType::P2pPing => {
            write_frame(writer, &Frame::new(MessageType::P2pPong, vec![])).await?;
        }
        MessageType::P2pPong => {}
        MessageType::BlockInventory => {
            let head = handler.head();
            for n in decode_numbers(&frame.payload) {
                if n > head {
                    let req = Frame::new(MessageType::FetchInvData, encode_numbers(&[n]));
                    write_frame(writer, &req).await?;
                }
            }
        }
        MessageType::FetchInvData => {
            if let Some(n) = decode_numbers(&frame.payload).first().copied() {
                let bytes = handler.block_bytes(n).unwrap_or_default();
                write_frame(writer, &Frame::new(MessageType::Block, bytes)).await?;
            }
        }
        MessageType::Block => {
            if !frame.payload.is_empty() {
                if let Some(new_head) = handler.on_block(&frame.payload) {
                    // Propagate onward to our other peers (gossip).
                    handle.advertise_block(new_head);
                }
            }
        }
        MessageType::Trx => handler.on_transaction(&frame.payload),
        _ => {}
    }
    Ok(())
}

fn encode_numbers(nums: &[i64]) -> Vec<u8> {
    nums.iter().flat_map(|n| n.to_be_bytes()).collect()
}

fn decode_numbers(bytes: &[u8]) -> Vec<i64> {
    bytes
        .chunks_exact(8)
        .map(|c| i64::from_be_bytes(c.try_into().unwrap()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Minimal in-memory handler: a monotonic head, a block store, and counters.
    struct StubHandler {
        head: AtomicI64,
        blocks: Mutex<HashMap<i64, Vec<u8>>>,
        txs: Mutex<Vec<Vec<u8>>>,
        applied: AtomicUsize,
    }

    impl StubHandler {
        fn new(head: i64) -> Arc<Self> {
            Arc::new(Self {
                head: AtomicI64::new(head),
                blocks: Mutex::new(HashMap::new()),
                txs: Mutex::new(Vec::new()),
                applied: AtomicUsize::new(0),
            })
        }

        fn with_block(head: i64, number: i64, bytes: &[u8]) -> Arc<Self> {
            let h = Self::new(head);
            h.blocks.lock().unwrap().insert(number, bytes.to_vec());
            h
        }
    }

    impl ChannelHandler for StubHandler {
        fn head(&self) -> i64 {
            self.head.load(Ordering::SeqCst)
        }
        fn on_block(&self, block_bytes: &[u8]) -> Option<i64> {
            // Accept as the next contiguous block.
            let new_head = self.head.load(Ordering::SeqCst) + 1;
            self.head.store(new_head, Ordering::SeqCst);
            self.blocks.lock().unwrap().insert(new_head, block_bytes.to_vec());
            self.applied.fetch_add(1, Ordering::SeqCst);
            Some(new_head)
        }
        fn on_transaction(&self, tx_bytes: &[u8]) {
            self.txs.lock().unwrap().push(tx_bytes.to_vec());
        }
        fn block_bytes(&self, number: i64) -> Option<Vec<u8>> {
            self.blocks.lock().unwrap().get(&number).cloned()
        }
    }

    async fn wait_until(timeout_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..(timeout_ms / 20) {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cond()
    }

    fn cfg() -> ChannelConfig {
        // Fast keepalive so the ping path is exercised within the test window.
        ChannelConfig { keepalive: Duration::from_millis(50), hello: b"test".to_vec() }
    }

    /// A advertises a block it holds; B (behind) fetches and applies it — heads
    /// converge purely over the advertise→inventory→fetch path.
    #[tokio::test]
    async fn advertised_block_is_fetched_and_applied() {
        let a_handler = StubHandler::with_block(1, 1, b"block-1");
        let b_handler = StubHandler::new(0);

        let (a_svc, a_handle) = ChannelService::new(a_handler.clone(), cfg());
        let (b_svc, _b_handle) = ChannelService::new(b_handler.clone(), cfg());

        let a_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a_addr = a_listener.local_addr().unwrap();
        let b_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

        let token = CancellationToken::new();
        // B dials A, so A gains a live peer to advertise to.
        tokio::spawn(a_svc.run(a_listener, vec![], token.clone()));
        tokio::spawn(b_svc.run(b_listener, vec![a_addr], token.clone()));

        assert!(wait_until(2000, || a_handle.live_peers() >= 1).await, "A never got a peer");
        a_handle.advertise_block(1);

        assert!(wait_until(2000, || b_handler.head() == 1).await, "B did not apply the block");
        assert_eq!(b_handler.applied.load(Ordering::SeqCst), 1);

        token.cancel();
    }

    /// Inventory for a block we already have (n <= head) is not fetched/applied.
    #[tokio::test]
    async fn inventory_at_or_below_head_is_ignored() {
        let a_handler = StubHandler::with_block(5, 3, b"block-3");
        let b_handler = StubHandler::new(5); // already at 5

        let (a_svc, a_handle) = ChannelService::new(a_handler.clone(), cfg());
        let (b_svc, _b) = ChannelService::new(b_handler.clone(), cfg());

        let a_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a_addr = a_listener.local_addr().unwrap();
        let b_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

        let token = CancellationToken::new();
        tokio::spawn(a_svc.run(a_listener, vec![], token.clone()));
        tokio::spawn(b_svc.run(b_listener, vec![a_addr], token.clone()));
        assert!(wait_until(2000, || a_handle.live_peers() >= 1).await);

        a_handle.advertise_block(3); // 3 <= B.head(5)
        // Give it time to (not) act.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(b_handler.head(), 5);
        assert_eq!(b_handler.applied.load(Ordering::SeqCst), 0);

        token.cancel();
    }

    /// A pushed transaction reaches the peer's `on_transaction`.
    #[tokio::test]
    async fn advertised_tx_reaches_peer_mempool_hook() {
        let a_handler = StubHandler::new(0);
        let b_handler = StubHandler::new(0);

        let (a_svc, a_handle) = ChannelService::new(a_handler.clone(), cfg());
        let (b_svc, _b) = ChannelService::new(b_handler.clone(), cfg());

        let a_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a_addr = a_listener.local_addr().unwrap();
        let b_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

        let token = CancellationToken::new();
        tokio::spawn(a_svc.run(a_listener, vec![], token.clone()));
        tokio::spawn(b_svc.run(b_listener, vec![a_addr], token.clone()));
        assert!(wait_until(2000, || a_handle.live_peers() >= 1).await);

        a_handle.advertise_tx(b"tx-bytes".to_vec());
        assert!(
            wait_until(2000, || b_handler.txs.lock().unwrap().len() == 1).await,
            "B never received the tx"
        );
        assert_eq!(b_handler.txs.lock().unwrap()[0], b"tx-bytes");

        token.cancel();
    }
}
