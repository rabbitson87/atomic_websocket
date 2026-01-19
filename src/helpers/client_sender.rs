//! Client connection management module for the atomic_websocket server.
//!
//! This module provides functionality for managing multiple WebSocket client connections
//! on the server side, including message handling and connection timeouts.
use async_trait::async_trait;
#[cfg(feature = "bebop")]
use bebop::Record;
use dashmap::DashMap;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio_tungstenite::tungstenite::Message;

#[cfg(feature = "bebop")]
use crate::schema::Data;
use crate::{
    client_sender::ServerOptions,
    helpers::{common::make_disconnect_message, retry::ExponentialBackoff, traits::date_time::now},
    log_debug, log_error,
};

use super::{common::make_expired_output_message, types::RwClientSenders};

/// Manages a collection of connected WebSocket clients on the server side.
///
/// This struct maintains a DashMap of client connections for O(1) lookup and provides
/// methods for sending messages to specific clients, handling client timeouts, and
/// processing incoming messages.
pub struct ClientSenders {
    /// DashMap of connected clients (peer -> ClientSender) for O(1) lookup with fine-grained locking
    clients: DashMap<String, ClientSender>,
    /// Channel sender for passing received messages to the application
    handle_message_sx: Sender<(Vec<u8>, String)>,
    /// Channel receiver for obtaining received messages (consumed once)
    handle_message_rx: Option<Receiver<(Vec<u8>, String)>>,
    /// Server options for connection management
    pub options: ServerOptions,
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
        let (handle_message_sx, handle_message_rx) = mpsc::channel(1024);
        Self {
            clients: DashMap::new(),
            handle_message_sx,
            handle_message_rx: Some(handle_message_rx),
            options: ServerOptions::default(),
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
        if let Some(existing) = self.clients.get(peer) {
            let _ = existing.sx.send(make_disconnect_message(peer)).await;
        }
        self.clients
            .insert(peer.to_owned(), ClientSender { sx, send_time: 0 });
    }

    /// Retrieves the message receiver channel.
    ///
    /// This method can only be called once per instance as it consumes the receiver.
    ///
    /// # Returns
    ///
    /// The message receiver channel
    ///
    /// # Panics
    ///
    /// Panics if the receiver has already been taken
    pub fn get_handle_message_receiver(&mut self) -> Receiver<(Vec<u8>, String)> {
        self.handle_message_rx
            .take()
            .expect("Receiver already taken")
    }

    /// Sends a message to the application message handler.
    ///
    /// # Arguments
    ///
    /// * `data` - Binary message data
    /// * `peer` - Client identifier
    pub async fn send_handle_message(&self, data: Vec<u8>, peer: &str) {
        let _ = self.handle_message_sx.send((data, peer.to_owned())).await;
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
        let keys_to_remove: Vec<String> = self
            .clients
            .iter()
            .filter(|entry| entry.send_time + 30 < now)
            .map(|entry| entry.key().clone())
            .collect();

        for key in keys_to_remove {
            self.clients.remove(&key);
        }
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
        self.clients.remove(peer);
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
    pub async fn send(&self, peer: &str, message: Message) -> bool {
        let sender = {
            let Some(client) = self.clients.get(peer) else {
                return false;
            };
            client.sx.clone()
        };
        let mut backoff = ExponentialBackoff::default();

        loop {
            match sender.send(message.clone()).await {
                Ok(_) => return true,
                Err(e) => {
                    log_error!(
                        "Error sending message (attempt {}): {:?}",
                        backoff.count() + 1,
                        e
                    );
                    if !backoff.wait().await {
                        log_error!("Failed to send after {} retries", backoff.count());
                        return false;
                    }
                }
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
    async fn get_handle_message_receiver(&self) -> Receiver<(Vec<u8>, String)>;

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
/// This implementation wraps a ClientSenders instance with read-write locks
/// to provide thread-safe access.
#[async_trait]
impl ClientSendersTrait for RwClientSenders {
    /// Adds or updates a client connection.
    async fn add(&self, peer: &str, sx: Sender<Message>) {
        self.write().await.add(peer, sx).await;
    }

    /// Gets the message receiver channel.
    async fn get_handle_message_receiver(&self) -> Receiver<(Vec<u8>, String)> {
        self.write().await.get_handle_message_receiver()
    }

    /// Sends a message to the application message handler.
    #[cfg(feature = "bebop")]
    async fn send_handle_message(&self, data: Data<'_>, peer: &str) {
        let mut buf = Vec::new();
        if let Err(e) = data.serialize(&mut buf) {
            log_error!("Failed to serialize data: {:?}", e);
            return;
        }
        self.write().await.send_handle_message(buf, peer).await;
    }

    /// Sends a message to the application message handler (raw bytes version).
    #[cfg(not(feature = "bebop"))]
    async fn send_handle_message(&self, data: Vec<u8>, peer: &str) {
        self.write().await.send_handle_message(data, peer).await;
    }

    /// Sends a message to a specific client.
    ///
    /// Updates the client's last message time on success or removes the client on failure.
    async fn send(&self, peer: &str, message: Message) -> bool {
        let result = self.read().await.send(peer, message).await;

        match result {
            true => self.write().await.write_time(peer),
            false => self.write().await.remove(peer),
        }
        result
    }

    /// Sends expiration messages to clients not in the provided list.
    ///
    /// Uses HashSet for O(1) lookups instead of O(n) contains checks.
    async fn expire_send(&self, peer_list: &[String]) {
        use std::collections::HashSet;
        let valid_peers: HashSet<&String> = peer_list.iter().collect();

        // Collect peers to expire first to avoid holding lock during sends
        let peers_to_expire: Vec<String> = self
            .read()
            .await
            .peers()
            .into_iter()
            .filter(|peer| !valid_peers.contains(peer))
            .collect();

        for peer in peers_to_expire {
            self.send(&peer, make_expired_output_message()).await;
        }
    }

    /// Checks if a client is active.
    async fn is_active(&self, peer: &str) -> bool {
        self.read().await.is_active(peer)
    }

    /// Sends a message to clients in the provided list.
    ///
    /// Uses HashSet for O(1) lookups instead of O(n) contains checks.
    async fn send_message_in_list(&self, peer_list: &[String], message: Message) {
        use std::collections::HashSet;
        let target_peers: HashSet<&String> = peer_list.iter().collect();

        // Collect matching peers first to avoid holding lock during sends
        let peers_to_send: Vec<String> = self
            .read()
            .await
            .peers()
            .into_iter()
            .filter(|peer| target_peers.contains(peer))
            .collect();

        for peer in peers_to_send {
            self.send(&peer, message.clone()).await;
        }
    }

    /// Sends a message to all connected clients.
    async fn send_all(&self, message: Message) {
        let all_peers: Vec<String> = self.read().await.peers();

        for peer in all_peers {
            self.send(&peer, message.clone()).await;
        }
    }

    /// Sends a message to all connected clients in the provided list.
    async fn send_all_in_list(&self, peer_list: &[String], message: Message) {
        use std::collections::HashSet;
        let target_peers: HashSet<&String> = peer_list.iter().collect();

        let peers_to_send: Vec<String> = self
            .read()
            .await
            .peers()
            .into_iter()
            .filter(|peer| target_peers.contains(peer))
            .collect();

        for peer in peers_to_send {
            self.send(&peer, message.clone()).await;
        }
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
        let mut senders = create_test_client_senders();
        let _rx = senders.get_handle_message_receiver();
        // Receiver should be taken successfully
    }

    #[tokio::test]
    async fn test_client_senders_send_handle_message() {
        let mut senders = create_test_client_senders();
        let mut rx = senders.get_handle_message_receiver();

        senders.send_handle_message(vec![1, 2, 3], "peer1").await;

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

        // 클라이언트 추가 (send_time = 0, 1970년)
        senders.add("peer1", tx).await;

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

        // 초기 send_time은 0
        let initial_time = senders.clients.get("peer1").unwrap().send_time;
        assert_eq!(initial_time, 0);

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

    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn create_rw_client_senders() -> RwClientSenders {
        Arc::new(RwLock::new(ClientSenders::new()))
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

        // 초기 send_time은 0
        {
            let guard = senders.read().await;
            let time = guard.clients.get("peer1").unwrap().send_time;
            assert_eq!(time, 0);
        }

        // 메시지 전송
        let msg = Message::Binary(Bytes::from_static(b"test"));
        let result = senders.send("peer1", msg).await;
        assert!(result);

        // 수신 확인
        let _ = rx.recv().await;

        // send_time이 업데이트됨
        {
            let guard = senders.read().await;
            let time = guard.clients.get("peer1").unwrap().send_time;
            assert!(time > 0);
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
