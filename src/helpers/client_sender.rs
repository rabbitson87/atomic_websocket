//! Client connection management module for the atomic_websocket server.
//!
//! This module provides functionality for managing multiple WebSocket client connections
//! on the server side, including message handling and connection timeouts.
use async_trait::async_trait;
#[cfg(feature = "bebop")]
use bebop::Record;
use dashmap::DashMap;
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

#[cfg(feature = "bebop")]
use crate::schema::Data;
use std::sync::Arc;

use crate::{
    client_sender::ServerOptions,
    helpers::{
        common::make_disconnect_message, metrics::Metrics, traits::date_time::now,
    },
    log_debug, log_error,
};

use super::{common::make_expired_output_message, types::RwClientSenders};

/// A received message on its way to the application: the raw payload and the
/// peer it came from.
type HandlerMessage = (Vec<u8>, String);

/// How long `add` will wait for a replaced connection to accept its disconnect
/// notice before giving up on it.
///
/// Generous for a healthy peer (the channel is drained as fast as the socket
/// accepts bytes) and short enough that a wedged one cannot hold up the peer
/// that is replacing it.
const DISCONNECT_NOTICE_TIMEOUT: Duration = Duration::from_millis(200);

/// How long a single peer may hold up a message before it is considered gone.
///
/// Long enough that a tablet briefly behind on its buffer is not dropped for
/// it; short enough that `send_all` cannot be held open by one unreachable
/// device. The alternative — no bound — is what this replaced.
const SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Manages a collection of connected WebSocket clients on the server side.
///
/// This struct maintains a DashMap of client connections for O(1) lookup and provides
/// methods for sending messages to specific clients, handling client timeouts, and
/// processing incoming messages.
pub struct ClientSenders {
    /// DashMap of connected clients (peer -> ClientSender) for O(1) lookup with fine-grained locking
    clients: DashMap<String, ClientSender>,
    /// Channel sender for passing received messages to the application
    handle_message_sx: Sender<HandlerMessage>,
    /// Channel receiver for obtaining received messages (consumed once)
    handle_message_rx: std::sync::Mutex<Option<Receiver<HandlerMessage>>>,
    /// Server options for connection management (interior mutability for lock-free Arc sharing)
    options: std::sync::RwLock<ServerOptions>,
    /// Metrics counters for observability
    pub metrics: Arc<Metrics>,
    /// Spillover buffer: stores handler messages when channel is full (non-blocking)
    spillover: std::sync::Mutex<VecDeque<(Vec<u8>, String)>>,
    /// Maximum spillover buffer capacity
    spillover_buffer_size: usize,
}

impl Default for ClientSenders {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientSenders {
    /// Creates a new ClientSenders instance.
    ///
    /// Initializes an empty HashMap of client connections and sets up message channels.
    ///
    /// # Returns
    ///
    /// A new ClientSenders instance
    pub fn new() -> Self {
        Self::new_with_buffer_size(1024, 1024)
    }

    /// Creates a new ClientSenders instance with custom buffer sizes.
    ///
    /// # Arguments
    ///
    /// * `handler_buffer_size` - Buffer size for the application message handler channel
    /// * `spillover_buffer_size` - Maximum spillover buffer capacity for handler messages
    pub fn new_with_buffer_size(handler_buffer_size: usize, spillover_buffer_size: usize) -> Self {
        let (handle_message_sx, handle_message_rx) = mpsc::channel(handler_buffer_size);
        Self {
            clients: DashMap::new(),
            handle_message_sx,
            handle_message_rx: std::sync::Mutex::new(Some(handle_message_rx)),
            options: std::sync::RwLock::new(ServerOptions::default()),
            metrics: Arc::new(Metrics::new()),
            spillover: std::sync::Mutex::new(VecDeque::new()),
            spillover_buffer_size,
        }
    }

    /// Adds or updates a client connection.
    ///
    /// If a client with the same peer identifier already exists, it replaces
    /// the sender channel with the new one after sending a disconnect message to
    /// the previous connection. Otherwise, it adds a new client to the HashMap.
    ///
    /// # Arguments
    ///
    /// * `peer` - Client identifier (typically an address)
    /// * `sx` - Message sender channel for the client
    ///
    /// # Complexity
    ///
    /// O(1) average case for DashMap operations
    pub async fn add(&self, peer: &str, sx: Sender<Message>) {
        log_debug!(
            "Add peer: {:?}, exists: {:?}",
            peer,
            self.clients.contains_key(peer)
        );

        // Clone the previous sender and let the shard guard go *before* awaiting
        // on it.
        //
        // Awaiting under the guard was a whole-store stall. The channel holds
        // `per_connection_buffer_size` messages (8 by default) and is drained by
        // the old connection's writer task, which is itself blocked writing to
        // the TCP socket. A tablet that walks out of WiFi range and comes back —
        // an ordinary event in a 48-tablet dining room, and the exact case this
        // branch exists to handle — leaves that socket half-open until the OS
        // gives up, minutes later. For all of those minutes the shard's lock was
        // held. `insert`, `remove` and `check_client_send_time` need the write
        // side of it, and dashmap's RwLock is task-fair, so every later reader —
        // `peers()`, `len()`, `send()` — queued behind those writers too. One
        // returning tablet stopped broadcasts to all forty-eight.
        //
        // `send` below already did this correctly; only `add` did not.
        let previous = self.clients.get(peer).map(|existing| existing.sx.clone());
        let replacing = previous.is_some();

        if let Some(previous) = previous {
            // Best effort. The point is to tell the old connection to go away,
            // not to guarantee it hears us — if it is wedged, the notice is
            // worthless anyway and the replacement below is what matters.
            if timeout(
                DISCONNECT_NOTICE_TIMEOUT,
                previous.send(make_disconnect_message(peer)),
            )
            .await
            .is_err()
            {
                log_error!(
                    "Previous connection for peer {:?} did not accept the disconnect notice within {:?}; replacing it anyway",
                    peer,
                    DISCONNECT_NOTICE_TIMEOUT
                );
            }
        }

        self.clients.insert(
            peer.to_owned(),
            ClientSender {
                sx,
                send_time: now().timestamp(),
            },
        );
        self.metrics.inc_connections_total();
        // Only a genuinely new peer raises the active count. Replacing one
        // leaves the count where it was — it is still one connection. Counting
        // it twice made `connections_active` climb on every reconnect and never
        // come back down, which over a day of 48 tablets turned the gauge into
        // noise.
        if !replacing {
            self.metrics.inc_connections_active();
        }
    }

    /// Retrieves the message receiver channel.
    ///
    /// Returns `None` if the receiver has already been taken by a previous call.
    ///
    /// # Returns
    ///
    /// `Some(Receiver)` on the first call, `None` on subsequent calls
    pub fn get_handle_message_receiver(&self) -> Option<Receiver<(Vec<u8>, String)>> {
        self.handle_message_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Sets the server options. Thread-safe via internal RwLock.
    pub fn set_options(&self, options: ServerOptions) {
        *self.options.write().unwrap_or_else(|e| e.into_inner()) = options;
    }

    /// Returns a clone of the current server options.
    pub fn options(&self) -> ServerOptions {
        self.options
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Forwards received message data to the application (non-blocking).
    ///
    /// Uses `try_send` to avoid blocking the WebSocket read loop.
    /// If the handler channel is full, messages are buffered in a spillover
    /// queue and drained on subsequent calls. Messages are dropped only
    /// when the spillover buffer also reaches its cap.
    ///
    /// # Arguments
    ///
    /// * `data` - Binary message data
    /// * `peer` - Client identifier
    pub fn send_handle_message(&self, data: Vec<u8>, peer: &str) {
        self.metrics.inc_messages_received();

        // Single lock guard for both drain and send (reduces lock contention)
        let mut spillover = self.spillover.lock().unwrap_or_else(|e| e.into_inner());

        // Step 1: drain any previously buffered messages first (ordering)
        while let Some(item) = spillover.front().cloned() {
            match self.handle_message_sx.try_send(item) {
                Ok(()) => {
                    spillover.pop_front();
                }
                Err(_) => break,
            }
        }

        // Step 2: attempt direct send or buffer
        if spillover.is_empty() {
            match self.handle_message_sx.try_send((data, peer.to_owned())) {
                Ok(()) => (),
                Err(tokio::sync::mpsc::error::TrySendError::Full(item)) => {
                    if spillover.len() < self.spillover_buffer_size {
                        spillover.push_back(item);
                    } else {
                        self.metrics.inc_messages_dropped();
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    log_error!("Handle message channel closed");
                }
            }
        } else {
            // Spillover is non-empty — queue to maintain message ordering
            if spillover.len() < self.spillover_buffer_size {
                spillover.push_back((data, peer.to_owned()));
            } else {
                self.metrics.inc_messages_dropped();
            }
        }
    }

    /// Checks for client timeouts and removes inactive clients.
    ///
    /// Clients that haven't sent a message within 30 seconds of the current time
    /// are considered inactive and are removed from the client HashMap.
    ///
    /// # Complexity
    ///
    /// O(n) where n is the number of clients
    pub fn check_client_send_time(&self) {
        let now = now().timestamp();
        let timeout = self
            .options
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .client_timeout_seconds as i64;
        self.clients.retain(|_, client| {
            let keep = client.send_time + timeout >= now;
            if !keep {
                self.metrics.dec_connections_active();
            }
            keep
        });
    }

    /// Removes a client from the DashMap.
    ///
    /// # Arguments
    ///
    /// * `peer` - Client identifier to remove
    ///
    /// # Complexity
    ///
    /// O(1) average case for DashMap removal
    pub fn remove(&self, peer: &str) {
        if self.clients.remove(peer).is_some() {
            self.metrics.dec_connections_active();
        }
        log_debug!("Remove peer: {:?}", peer);
    }

    /// Updates the last message time for a client.
    ///
    /// # Arguments
    ///
    /// * `peer` - Client identifier
    ///
    /// # Complexity
    ///
    /// O(1) average case for DashMap lookup
    pub fn write_time(&self, peer: &str) {
        if let Some(mut client) = self.clients.get_mut(peer) {
            client.write_time();
        }
    }

    /// Sends a message to a specific client.
    ///
    /// Attempts to send a message to the specified client, with exponential backoff
    /// retry logic in case of failures.
    ///
    /// # Arguments
    ///
    /// * `peer` - Client identifier
    /// * `message` - WebSocket message to send
    ///
    /// # Returns
    ///
    /// `true` if the message was sent successfully, `false` otherwise
    ///
    /// # Complexity
    ///
    /// O(1) average case for DashMap lookup
    /// Queues `message` for `peer`, giving up after [`SEND_TIMEOUT`].
    ///
    /// Returning `false` means the caller should treat the peer as gone;
    /// `ClientSendersTrait::send` removes it.
    ///
    /// This used to retry with exponential backoff, which could not help with
    /// either thing that goes wrong here.
    ///
    /// The channel is bounded, so `Sender::send` only fails once the receiver
    /// is dropped — and a dropped receiver does not come back. Every retry was
    /// guaranteed to fail, and the five of them took about two seconds to
    /// arrive at the answer the first attempt already had.
    ///
    /// The case it never covered is the one that matters: a full buffer whose
    /// receiver is still alive. `send().await` then waits for capacity, and
    /// capacity is freed by the writer task, which is blocked writing to the
    /// socket. A tablet that leaves WiFi mid-service holds a half-open socket
    /// until the OS gives up minutes later, so the wait had no bound at all.
    /// `send_all` joins across every peer, so it did not return until that
    /// one tablet's write completed — one tablet out of range stopped every
    /// broadcast to the other forty-seven for as long as it took.
    pub async fn send(&self, peer: &str, message: Message) -> bool {
        let sender = {
            let Some(client) = self.clients.get(peer) else {
                return false;
            };
            client.sx.clone()
        };

        match sender.send_timeout(message, SEND_TIMEOUT).await {
            Ok(_) => {
                self.metrics.inc_messages_sent();
                true
            }
            Err(e) => {
                self.metrics.inc_send_errors();
                log_error!("Dropping peer {:?}: {:?}", peer, e);
                false
            }
        }
    }

    /// Checks if a client is active.
    ///
    /// # Arguments
    ///
    /// * `peer` - Client identifier
    ///
    /// # Returns
    ///
    /// `true` if the client exists in the HashMap, `false` otherwise
    ///
    /// # Complexity
    ///
    /// O(1) average case for DashMap lookup
    pub fn is_active(&self, peer: &str) -> bool {
        self.clients.contains_key(peer)
    }

    /// Returns the number of connected clients.
    ///
    /// # Returns
    ///
    /// Number of clients in the DashMap
    pub fn len(&self) -> usize {
        self.clients.len()
    }

    /// Checks if there are no connected clients.
    ///
    /// # Returns
    ///
    /// `true` if the DashMap is empty, `false` otherwise
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Returns a vector of all peer identifiers.
    ///
    /// # Returns
    ///
    /// Vector of peer strings
    pub fn peers(&self) -> Vec<String> {
        self.clients
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Returns peers NOT in the provided set.
    pub fn peers_except(&self, valid: &std::collections::HashSet<&String>) -> Vec<String> {
        self.clients
            .iter()
            .filter(|entry| !valid.contains(entry.key()))
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Returns peers that ARE in the provided set.
    pub fn peers_in(&self, target: &std::collections::HashSet<&String>) -> Vec<String> {
        self.clients
            .iter()
            .filter(|entry| target.contains(entry.key()))
            .map(|entry| entry.key().clone())
            .collect()
    }
}

/// Trait defining operations for client connection management.
///
/// This trait defines the interface for managing WebSocket client connections,
/// allowing for different implementations.
#[async_trait]
pub trait ClientSendersTrait {
    /// Adds or updates a client connection.
    async fn add(&self, peer: &str, sx: Sender<Message>);

    /// Gets the message receiver channel.
    async fn get_handle_message_receiver(&self) -> Option<Receiver<(Vec<u8>, String)>>;

    /// Sends a message to the application message handler.
    #[cfg(feature = "bebop")]
    async fn send_handle_message(&self, data: Data<'_>, peer: &str);

    /// Sends a message to the application message handler (raw bytes version).
    #[cfg(not(feature = "bebop"))]
    async fn send_handle_message(&self, data: Vec<u8>, peer: &str);

    /// Sends a message to a specific client.
    async fn send(&self, peer: &str, message: Message) -> bool;

    /// Sends expiration messages to clients not in the provided list.
    async fn expire_send(&self, peer_list: &[String]);

    /// Checks if a client is active.
    async fn is_active(&self, peer: &str) -> bool;

    /// Sends a message to clients in the provided list.
    async fn send_message_in_list(&self, peer_list: &[String], message: Message);

    /// Sends a message to all connected clients.
    async fn send_all(&self, message: Message);

    /// Sends a message to all connected clients in the provided list.
    async fn send_all_in_list(&self, peer_list: &[String], message: Message);
}

/// Implementation of ClientSendersTrait for thread-safe client senders.
///
/// Since `RwClientSenders` is now `Arc<ClientSenders>` (no outer RwLock),
/// all thread safety comes from interior mutability: DashMap for clients,
/// std::sync::Mutex for the receiver, and std::sync::RwLock for options.
#[async_trait]
impl ClientSendersTrait for RwClientSenders {
    async fn add(&self, peer: &str, sx: Sender<Message>) {
        (**self).add(peer, sx).await;
    }

    async fn get_handle_message_receiver(&self) -> Option<Receiver<(Vec<u8>, String)>> {
        (**self).get_handle_message_receiver()
    }

    #[cfg(feature = "bebop")]
    async fn send_handle_message(&self, data: Data<'_>, peer: &str) {
        let mut buf = Vec::with_capacity(256);
        if let Err(e) = data.serialize(&mut buf) {
            log_error!("Failed to serialize data: {:?}", e);
            return;
        }
        (**self).send_handle_message(buf, peer);
    }

    #[cfg(not(feature = "bebop"))]
    async fn send_handle_message(&self, data: Vec<u8>, peer: &str) {
        (**self).send_handle_message(data, peer);
    }

    /// Sends a message to a specific client with retry and bookkeeping.
    ///
    /// Delegates to `ClientSenders::send()` for the actual send+retry,
    /// then updates send time on success or removes the peer on failure.
    async fn send(&self, peer: &str, message: Message) -> bool {
        let result = (**self).send(peer, message).await;
        match result {
            true => (**self).write_time(peer),
            false => (**self).remove(peer),
        }
        result
    }

    async fn expire_send(&self, peer_list: &[String]) {
        use std::collections::HashSet;
        let valid_peers: HashSet<&String> = peer_list.iter().collect();
        let peers_to_expire = (**self).peers_except(&valid_peers);
        for peer in peers_to_expire {
            self.send(&peer, make_expired_output_message()).await;
        }
    }

    async fn is_active(&self, peer: &str) -> bool {
        (**self).is_active(peer)
    }

    async fn send_message_in_list(&self, peer_list: &[String], message: Message) {
        use std::collections::HashSet;
        let target_peers: HashSet<&String> = peer_list.iter().collect();
        let peers = (**self).peers_in(&target_peers);
        let futures: Vec<_> = peers
            .iter()
            .map(|peer| self.send(peer, message.clone()))
            .collect();
        futures_util::future::join_all(futures).await;
    }

    async fn send_all(&self, message: Message) {
        let all_peers = (**self).peers();
        let futures: Vec<_> = all_peers
            .iter()
            .map(|peer| self.send(peer, message.clone()))
            .collect();
        futures_util::future::join_all(futures).await;
    }

    async fn send_all_in_list(&self, peer_list: &[String], message: Message) {
        use std::collections::HashSet;
        let target_peers: HashSet<&String> = peer_list.iter().collect();
        let peers = (**self).peers_in(&target_peers);
        let futures: Vec<_> = peers
            .iter()
            .map(|peer| self.send(peer, message.clone()))
            .collect();
        futures_util::future::join_all(futures).await;
    }
}

/// Represents a single WebSocket client connection.
///
/// Stores the message sender channel and the timestamp of the last message.
/// The peer identifier is now the HashMap key, not stored in the struct.
#[derive(Debug, Clone)]
struct ClientSender {
    /// Message sender channel
    sx: Sender<Message>,
    /// Timestamp of the last message sent
    send_time: i64,
}

impl ClientSender {
    /// Updates the last message timestamp to the current time.
    pub fn write_time(&mut self) {
        self.send_time = now().timestamp();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::Bytes;

    fn create_test_client_senders() -> ClientSenders {
        ClientSenders::new()
    }

    #[test]
    fn test_client_senders_new() {
        let senders = create_test_client_senders();
        assert!(senders.is_empty());
        assert_eq!(senders.len(), 0);
    }

    #[test]
    fn test_client_senders_default() {
        let senders = ClientSenders::default();
        assert!(senders.is_empty());
        assert_eq!(senders.len(), 0);
    }

    #[tokio::test]
    async fn test_client_senders_add() {
        let senders = create_test_client_senders();
        let (tx, _rx) = mpsc::channel(8);

        senders.add("peer1", tx).await;
        assert_eq!(senders.len(), 1);
        assert!(senders.is_active("peer1"));
        assert!(!senders.is_empty());
    }

    #[tokio::test]
    async fn test_client_senders_add_multiple() {
        let senders = create_test_client_senders();
        let (tx1, _rx1) = mpsc::channel(8);
        let (tx2, _rx2) = mpsc::channel(8);

        senders.add("peer1", tx1).await;
        senders.add("peer2", tx2).await;

        assert_eq!(senders.len(), 2);
        assert!(senders.is_active("peer1"));
        assert!(senders.is_active("peer2"));
    }

    #[tokio::test]
    async fn test_client_senders_remove() {
        let senders = create_test_client_senders();
        let (tx, _rx) = mpsc::channel(8);

        senders.add("peer1", tx).await;
        assert_eq!(senders.len(), 1);

        senders.remove("peer1");
        assert_eq!(senders.len(), 0);
        assert!(!senders.is_active("peer1"));
    }

    #[tokio::test]
    async fn test_client_senders_peers() {
        let senders = create_test_client_senders();
        let (tx1, _rx1) = mpsc::channel(8);
        let (tx2, _rx2) = mpsc::channel(8);

        senders.add("peer1", tx1).await;
        senders.add("peer2", tx2).await;

        let peers = senders.peers();
        assert_eq!(peers.len(), 2);
        assert!(peers.contains(&"peer1".to_string()));
        assert!(peers.contains(&"peer2".to_string()));
    }

    #[test]
    fn test_client_senders_is_active_nonexistent() {
        let senders = create_test_client_senders();
        assert!(!senders.is_active("nonexistent"));
    }

    #[tokio::test]
    async fn test_client_senders_send_success() {
        let senders = create_test_client_senders();
        let (tx, mut rx) = mpsc::channel(8);

        senders.add("peer1", tx).await;

        let msg = Message::Binary(Bytes::from_static(b"test"));
        let result = senders.send("peer1", msg).await;
        assert!(result);

        let received = rx.recv().await;
        assert!(received.is_some());
    }

    #[tokio::test]
    async fn test_client_senders_send_nonexistent_peer() {
        let senders = create_test_client_senders();
        let msg = Message::Binary(Bytes::from_static(b"test"));
        let result = senders.send("nonexistent", msg).await;
        assert!(!result);
    }

    #[test]
    fn test_client_senders_get_handle_message_receiver() {
        let senders = create_test_client_senders();
        let rx = senders.get_handle_message_receiver();
        assert!(rx.is_some(), "First call should return Some");

        // Second call should return None
        let rx2 = senders.get_handle_message_receiver();
        assert!(rx2.is_none(), "Second call should return None");
    }

    #[tokio::test]
    async fn test_client_senders_send_handle_message() {
        let senders = create_test_client_senders();
        let mut rx = senders.get_handle_message_receiver().expect("receiver");

        senders.send_handle_message(vec![1, 2, 3], "peer1");

        let received = rx.recv().await;
        assert!(received.is_some());
        let (data, peer) = received.unwrap();
        assert_eq!(data, vec![1, 2, 3]);
        assert_eq!(peer, "peer1");
    }

    #[tokio::test]
    async fn test_client_senders_write_time() {
        let senders = create_test_client_senders();
        let (tx, _rx) = mpsc::channel(8);

        senders.add("peer1", tx).await;
        senders.write_time("peer1");
        // Should not panic, time should be updated
    }

    #[tokio::test]
    async fn test_client_senders_replace_existing() {
        let senders = create_test_client_senders();
        let (tx1, mut rx1) = mpsc::channel(8);
        let (tx2, _rx2) = mpsc::channel(8);

        senders.add("peer1", tx1).await;
        senders.add("peer1", tx2).await;

        // Should still have only 1 peer
        assert_eq!(senders.len(), 1);

        // Old connection should receive disconnect message
        let msg = rx1.recv().await;
        assert!(msg.is_some());
    }

    #[test]
    fn test_client_sender_write_time() {
        let (tx, _rx) = mpsc::channel(8);
        let mut sender = ClientSender {
            sx: tx,
            send_time: 0,
        };

        assert_eq!(sender.send_time, 0);
        sender.write_time();
        assert!(sender.send_time > 0);
    }

    // ========================================================================
    // check_client_send_time() 타임아웃 감지 테스트
    // ========================================================================

    #[tokio::test]
    async fn test_check_client_send_time_removes_inactive_clients() {
        let senders = create_test_client_senders();
        let (tx, _rx) = mpsc::channel(8);

        // 클라이언트 추가 후 send_time을 0으로 설정하여 비활성 상태 시뮬레이션
        senders.add("peer1", tx).await;
        senders.clients.get_mut("peer1").unwrap().send_time = 0;

        // send_time이 0이므로 현재 시간보다 30초 이상 오래됨
        // check_client_send_time 호출 시 제거되어야 함
        assert!(senders.is_active("peer1"));

        senders.check_client_send_time();

        // 30초 이상 지난 클라이언트는 제거됨
        assert!(!senders.is_active("peer1"));
        assert_eq!(senders.len(), 0);
    }

    #[tokio::test]
    async fn test_check_client_send_time_keeps_active_clients() {
        let senders = create_test_client_senders();
        let (tx, _rx) = mpsc::channel(8);

        senders.add("peer1", tx).await;

        // write_time을 호출하여 현재 시간으로 업데이트
        senders.write_time("peer1");

        senders.check_client_send_time();

        // 최근에 활동한 클라이언트는 유지됨
        assert!(senders.is_active("peer1"));
        assert_eq!(senders.len(), 1);
    }

    #[tokio::test]
    async fn test_check_client_send_time_mixed_clients() {
        let senders = create_test_client_senders();
        let (tx1, _rx1) = mpsc::channel(8);
        let (tx2, _rx2) = mpsc::channel(8);
        let (tx3, _rx3) = mpsc::channel(8);

        // 3개의 클라이언트 추가
        senders.add("inactive1", tx1).await;
        senders.add("active", tx2).await;
        senders.add("inactive2", tx3).await;

        // inactive 클라이언트들의 send_time을 0으로 설정하여 비활성 상태 시뮬레이션
        senders.clients.get_mut("inactive1").unwrap().send_time = 0;
        senders.clients.get_mut("inactive2").unwrap().send_time = 0;

        // active만 시간 업데이트
        senders.write_time("active");

        assert_eq!(senders.len(), 3);

        senders.check_client_send_time();

        // inactive 클라이언트들은 제거되고, active만 남음
        assert!(!senders.is_active("inactive1"));
        assert!(senders.is_active("active"));
        assert!(!senders.is_active("inactive2"));
        assert_eq!(senders.len(), 1);
    }

    #[test]
    fn test_check_client_send_time_empty_clients() {
        let senders = create_test_client_senders();

        // 빈 상태에서 호출해도 에러 없이 동작
        senders.check_client_send_time();
        assert_eq!(senders.len(), 0);
    }

    // ========================================================================
    // write_time() 동작 검증 테스트
    // ========================================================================

    #[tokio::test]
    async fn test_write_time_updates_timestamp_correctly() {
        let senders = create_test_client_senders();
        let (tx, _rx) = mpsc::channel(8);

        senders.add("peer1", tx).await;

        // 초기 send_time은 now()로 초기화됨
        let initial_time = senders.clients.get("peer1").unwrap().send_time;
        let now_ts = crate::helpers::traits::date_time::now().timestamp();
        assert!((initial_time - now_ts).abs() <= 1);

        // write_time 호출
        senders.write_time("peer1");

        // send_time이 현재 시간으로 업데이트됨
        let updated_time = senders.clients.get("peer1").unwrap().send_time;
        assert!(updated_time > 0);

        // 현재 시간과 비슷해야 함 (1초 오차 허용)
        let now = crate::helpers::traits::date_time::now().timestamp();
        assert!((updated_time - now).abs() <= 1);
    }

    #[tokio::test]
    async fn test_write_time_nonexistent_peer_no_panic() {
        let senders = create_test_client_senders();

        // 존재하지 않는 peer에 대해 호출해도 패닉 없음
        senders.write_time("nonexistent");
        // 아무 일도 일어나지 않음
    }

    // ========================================================================
    // ClientSendersTrait 브로드캐스트 동작 검증 테스트
    // ========================================================================

    fn create_rw_client_senders() -> RwClientSenders {
        Arc::new(ClientSenders::new())
    }

    #[tokio::test]
    async fn test_trait_send_all_broadcasts_to_all_clients() {
        let senders = create_rw_client_senders();
        let (tx1, mut rx1) = mpsc::channel(8);
        let (tx2, mut rx2) = mpsc::channel(8);
        let (tx3, mut rx3) = mpsc::channel(8);

        // 3개의 클라이언트 추가
        senders.add("peer1", tx1).await;
        senders.add("peer2", tx2).await;
        senders.add("peer3", tx3).await;

        // 메시지 브로드캐스트
        let msg = Message::Binary(Bytes::from_static(b"broadcast"));
        senders.send_all(msg).await;

        // 모든 클라이언트가 메시지를 받아야 함
        let recv1 = rx1.recv().await;
        let recv2 = rx2.recv().await;
        let recv3 = rx3.recv().await;

        assert!(recv1.is_some());
        assert!(recv2.is_some());
        assert!(recv3.is_some());
    }

    #[tokio::test]
    async fn test_trait_send_all_empty_clients() {
        let senders = create_rw_client_senders();

        // 클라이언트 없는 상태에서 브로드캐스트해도 에러 없음
        let msg = Message::Binary(Bytes::from_static(b"broadcast"));
        senders.send_all(msg).await;
        // 패닉 없이 완료됨
    }

    #[tokio::test]
    async fn test_trait_send_all_in_list_filters_correctly() {
        let senders = create_rw_client_senders();
        let (tx1, mut rx1) = mpsc::channel(8);
        let (tx2, mut rx2) = mpsc::channel(8);
        let (tx3, mut rx3) = mpsc::channel(8);

        senders.add("peer1", tx1).await;
        senders.add("peer2", tx2).await;
        senders.add("peer3", tx3).await;

        // peer1과 peer3에만 메시지 전송
        let target_list = vec!["peer1".to_string(), "peer3".to_string()];
        let msg = Message::Binary(Bytes::from_static(b"filtered"));
        senders.send_all_in_list(&target_list, msg).await;

        // peer1, peer3는 메시지를 받고, peer2는 받지 않음
        let recv1 = tokio::time::timeout(std::time::Duration::from_millis(100), rx1.recv()).await;
        let recv2 = tokio::time::timeout(std::time::Duration::from_millis(100), rx2.recv()).await;
        let recv3 = tokio::time::timeout(std::time::Duration::from_millis(100), rx3.recv()).await;

        assert!(recv1.is_ok() && recv1.unwrap().is_some());
        assert!(recv2.is_err() || recv2.unwrap().is_none()); // 타임아웃 또는 None
        assert!(recv3.is_ok() && recv3.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_trait_send_message_in_list_filters_by_existing_peers() {
        let senders = create_rw_client_senders();
        let (tx1, mut rx1) = mpsc::channel(8);
        let (tx2, mut rx2) = mpsc::channel(8);

        senders.add("peer1", tx1).await;
        senders.add("peer2", tx2).await;

        // 리스트에 존재하지 않는 peer도 포함
        let target_list = vec![
            "peer1".to_string(),
            "peer3".to_string(), // 존재하지 않음
            "peer4".to_string(), // 존재하지 않음
        ];
        let msg = Message::Binary(Bytes::from_static(b"test"));
        senders.send_message_in_list(&target_list, msg).await;

        // peer1만 메시지를 받음 (리스트에 있고 실제로 존재하는 peer)
        let recv1 = tokio::time::timeout(std::time::Duration::from_millis(100), rx1.recv()).await;
        let recv2 = tokio::time::timeout(std::time::Duration::from_millis(100), rx2.recv()).await;

        assert!(recv1.is_ok() && recv1.unwrap().is_some());
        assert!(recv2.is_err() || recv2.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_trait_expire_send_sends_to_unlisted_peers() {
        let senders = create_rw_client_senders();
        let (tx1, mut rx1) = mpsc::channel(8);
        let (tx2, mut rx2) = mpsc::channel(8);
        let (tx3, mut rx3) = mpsc::channel(8);

        senders.add("peer1", tx1).await;
        senders.add("peer2", tx2).await;
        senders.add("peer3", tx3).await;

        // peer2만 유효한 리스트에 포함
        let valid_list = vec!["peer2".to_string()];
        senders.expire_send(&valid_list).await;

        // peer1, peer3는 만료 메시지를 받음 (리스트에 없으므로)
        let recv1 = tokio::time::timeout(std::time::Duration::from_millis(100), rx1.recv()).await;
        let recv2 = tokio::time::timeout(std::time::Duration::from_millis(100), rx2.recv()).await;
        let recv3 = tokio::time::timeout(std::time::Duration::from_millis(100), rx3.recv()).await;

        assert!(recv1.is_ok() && recv1.unwrap().is_some()); // 만료 메시지 받음
        assert!(recv2.is_err() || recv2.unwrap().is_none()); // 리스트에 있으므로 안 받음
        assert!(recv3.is_ok() && recv3.unwrap().is_some()); // 만료 메시지 받음
    }

    #[tokio::test]
    async fn test_trait_is_active_through_rwlock() {
        let senders = create_rw_client_senders();
        let (tx, _rx) = mpsc::channel(8);

        assert!(!senders.is_active("peer1").await);

        senders.add("peer1", tx).await;

        assert!(senders.is_active("peer1").await);
        assert!(!senders.is_active("peer2").await);
    }

    #[tokio::test]
    async fn test_trait_send_updates_time_on_success() {
        let senders = create_rw_client_senders();
        let (tx, mut rx) = mpsc::channel(8);

        senders.add("peer1", tx).await;

        // 초기 send_time은 now()로 초기화됨
        let initial_time = {
            let time = senders.clients.get("peer1").unwrap().send_time;
            assert!(time > 0);
            time
        };

        // 메시지 전송
        let msg = Message::Binary(Bytes::from_static(b"test"));
        let result = senders.send("peer1", msg).await;
        assert!(result);

        // 수신 확인
        let _ = rx.recv().await;

        // send_time이 업데이트됨 (초기값 이상)
        {
            let time = senders.clients.get("peer1").unwrap().send_time;
            assert!(time >= initial_time);
        }
    }

    #[tokio::test]
    async fn test_trait_send_removes_peer_on_failure() {
        let senders = create_rw_client_senders();
        let (tx, rx) = mpsc::channel(1);

        senders.add("peer1", tx).await;
        assert!(senders.is_active("peer1").await);

        // 수신자를 드롭하여 채널 닫음
        drop(rx);

        // 메시지 전송 시도 - 실패해야 함
        let msg = Message::Binary(Bytes::from_static(b"test"));
        let result = senders.send("peer1", msg).await;
        assert!(!result);

        // 실패 후 peer가 제거됨
        assert!(!senders.is_active("peer1").await);
    }
}
