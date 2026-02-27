//! Client-side server connection management for atomic_websocket.
//!
//! This module provides functionality for managing connections to WebSocket servers,
//! including message sending, connection status tracking, and automatic reconnection.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
#[cfg(feature = "bebop")]
use bebop::Record;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio_tungstenite::tungstenite::Message;

#[cfg(feature = "bebop")]
use crate::generated::schema::{Data, ServerConnectInfo};
#[cfg(feature = "bebop")]
use crate::helpers::common::get_setting_by_key;
#[cfg(not(feature = "bebop"))]
use crate::helpers::common::remove_setting;
use crate::{
    helpers::{
        common::set_setting, get_internal_websocket::wrap_get_internal_websocket,
        get_outer_websocket::wrap_get_outer_websocket, metrics::Metrics,
    },
    log_debug, log_error, AtomicWebsocketType, Settings,
};

use crate::helpers::traits::date_time::now;

use super::{
    common::make_disconnect_message,
    internal_client::ClientOptions,
    retry::ExponentialBackoff,
    types::{save_key, RwServerSender, DB},
};

/// Persists server connection info to the database.
///
/// Uses `set_setting` which handles both native-db and in-memory storage,
/// eliminating the need for separate cfg-gated implementations.
async fn persist_connection_info(db: DB, server_ip: &str) {
    #[cfg(feature = "bebop")]
    let value = {
        let port = server_ip.split(':').nth(1).unwrap_or("");
        let data = ServerConnectInfo { server_ip, port };
        let mut buf = Vec::new();
        if let Err(e) = data.serialize(&mut buf) {
            log_error!("Failed to serialize ServerConnectInfo: {:?}", e);
            return;
        }
        buf
    };
    #[cfg(not(feature = "bebop"))]
    let value = server_ip.as_bytes().to_vec();

    if let Err(e) = set_setting(
        db,
        Settings {
            key: save_key::SERVER_CONNECT_INFO.to_owned(),
            value,
        },
    )
    .await
    {
        log_error!("Failed to persist connection info: {:?}", e);
    }
}

/// Represents the current status of a server connection.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SenderStatus {
    /// Connection initialization started
    Start,
    /// Actively attempting to connect for the first time
    Connecting,
    /// Successfully connected to the server
    Connected,
    /// Disconnected from the server
    Disconnected,
    /// Lost connection, automatically attempting to reconnect
    Reconnecting,
}

/// Manages a connection to a WebSocket server from the client side.
///
/// Handles message sending, connection status updates, and reconnection logic.
pub struct ServerSender {
    /// Channel for sending messages to the server
    sx: Option<mpsc::Sender<Message>>,
    /// Database for storing connection state
    pub db: DB,
    /// Reference to self for recursive operations
    pub server_sender: Option<RwServerSender>,
    /// Server IP address or WebSocket URL
    pub server_ip: String,
    /// Timestamp of the last received message
    pub server_received_times: i64,
    /// Channel for sending connection status updates
    status_tx: Sender<SenderStatus>,
    /// Channel for receiving connection status updates (consumed once)
    status_rx: Option<Receiver<SenderStatus>>,
    /// Channel for sending received message data
    handle_message_tx: Sender<Vec<u8>>,
    /// Channel for receiving message data (consumed once)
    handle_message_rx: Option<Receiver<Vec<u8>>>,
    /// Connection configuration options
    pub options: ClientOptions,
    /// Whether a connection attempt is currently in progress
    pub is_try_connect: bool,
    /// Metrics counters for observability
    pub metrics: Arc<Metrics>,
    /// Spillover buffer: stores messages when handler channel is full (non-blocking)
    spillover: std::sync::Mutex<VecDeque<Vec<u8>>>,
}

impl ServerSender {
    /// Creates a new ServerSender instance.
    ///
    /// # Arguments
    ///
    /// * `db` - Database for storing connection state
    /// * `server_ip` - Server IP address or WebSocket URL
    /// * `options` - Connection configuration options
    ///
    /// # Returns
    ///
    /// A new ServerSender instance
    pub fn new(db: DB, server_ip: String, options: ClientOptions) -> Self {
        let (status_tx, status_rx) = mpsc::channel(options.status_buffer_size);
        let (handle_message_tx, handle_message_rx) = mpsc::channel(options.handler_buffer_size);

        Self {
            sx: None,
            db,
            server_sender: None,
            server_ip,
            server_received_times: 0,
            status_tx,
            status_rx: Some(status_rx),
            handle_message_tx,
            handle_message_rx: Some(handle_message_rx),
            options,
            is_try_connect: false,
            metrics: Arc::new(Metrics::new()),
            spillover: std::sync::Mutex::new(VecDeque::new()),
        }
    }

    /// Gets the receiver for connection status updates.
    ///
    /// # Returns
    ///
    /// A channel receiver for connection status events
    ///
    /// # Panics
    ///
    /// Panics if the receiver has already been taken
    pub fn get_status_receiver(&mut self) -> Receiver<SenderStatus> {
        self.status_rx.take().expect("Receiver already taken")
    }

    /// Gets the receiver for incoming messages.
    ///
    /// # Returns
    ///
    /// A channel receiver for incoming message data
    ///
    /// # Panics
    ///
    /// Panics if the receiver has already been taken
    pub fn get_handle_message_receiver(&mut self) -> Receiver<Vec<u8>> {
        self.handle_message_rx
            .take()
            .expect("Receiver already taken")
    }

    /// Registers a reference to self for recursive operations.
    ///
    /// # Arguments
    ///
    /// * `server_sender` - Reference to this server sender wrapped in RwLock
    pub fn regist(&mut self, server_sender: RwServerSender) {
        self.server_sender = Some(server_sender);
    }

    /// Closes and drops the current message sender.
    ///
    /// Sends a disconnect message before closing to ensure clean shutdown.
    fn sx_drop(&mut self) {
        if let Some(sender) = self.sx.take() {
            let prev_server_ip = self.server_ip.clone();
            tokio::spawn(async move {
                let _ = sender.send(make_disconnect_message(&prev_server_ip)).await;
                sender.closed().await;
            });
        }
    }

    /// Sets a new message sender and server IP.
    ///
    /// # Arguments
    ///
    /// * `sx` - New message sender channel
    /// * `server_ip` - Server IP address or WebSocket URL
    pub fn add(&mut self, sx: mpsc::Sender<Message>, server_ip: &str) {
        self.sx_drop();
        self.sx = Some(sx);
        self.server_ip = server_ip.into();
    }

    /// Removes the current server IP and drops the connection.
    pub fn remove_ip(&mut self) {
        if !self.server_ip.is_empty() {
            self.sx_drop();
            self.server_ip = "".into();
        }
    }

    /// Sends a connection status update.
    ///
    /// Uses `try_send` since status updates are idempotent —
    /// dropping a redundant status update is acceptable.
    ///
    /// # Arguments
    ///
    /// * `status` - The connection status to send
    ///
    /// # Returns
    ///
    /// `true` if the status was sent, `false` if the channel was full
    pub fn send_status(&self, status: SenderStatus) -> bool {
        if self.status_tx.try_send(status).is_err() {
            log_debug!("Status channel full, dropping status update");
            self.metrics.inc_messages_dropped();
            return false;
        }
        true
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
    pub fn send_handle_message(&self, data: Vec<u8>) {
        // Step 1: drain any previously buffered messages first (ordering)
        self.drain_spillover();

        // Step 2: attempt direct send or buffer
        let mut spillover = self.spillover.lock().unwrap_or_else(|e| e.into_inner());
        if spillover.is_empty() {
            match self.handle_message_tx.try_send(data) {
                Ok(()) => return,
                Err(tokio::sync::mpsc::error::TrySendError::Full(data)) => {
                    if spillover.len() < self.options.spillover_buffer_size {
                        spillover.push_back(data);
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
            if spillover.len() < self.options.spillover_buffer_size {
                spillover.push_back(data);
            } else {
                self.metrics.inc_messages_dropped();
            }
        }
    }

    /// Drains buffered spillover messages into the handler channel.
    fn drain_spillover(&self) {
        let mut spillover = self.spillover.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(data) = spillover.front().cloned() {
            match self.handle_message_tx.try_send(data) {
                Ok(()) => {
                    spillover.pop_front();
                }
                Err(_) => break,
            }
        }
    }
}

/// Trait defining operations for server connection management.
///
/// This trait defines the interface for managing WebSocket server connections,
/// allowing for different implementations and thread-safe access.
#[async_trait]
pub trait ServerSenderTrait {
    /// Sets a new message sender and server IP.
    async fn add(&self, sx: mpsc::Sender<Message>, server_ip: &str);

    /// Sends a connection status update.
    async fn send_status(&self, status: SenderStatus);

    /// Forwards received message data to the application.
    #[cfg(feature = "bebop")]
    async fn send_handle_message(&self, data: Data<'_>);

    /// Forwards received message data to the application (raw bytes version).
    #[cfg(not(feature = "bebop"))]
    async fn send_handle_message(&self, data: Vec<u8>);

    /// Gets the receiver for connection status updates.
    async fn get_status_receiver(&self) -> Receiver<SenderStatus>;

    /// Gets the receiver for incoming messages.
    async fn get_handle_message_receiver(&self) -> Receiver<Vec<u8>>;

    /// Sends a message to the connected server.
    async fn send(&self, message: Message);

    /// Registers a reference to self for recursive operations.
    async fn regist(&mut self, server_sender: RwServerSender);

    /// Checks if the current server IP is valid and recently active.
    async fn is_valid_server_ip(&self) -> bool;

    /// Removes the current server IP and drops the connection.
    async fn remove_ip(&self);

    /// Removes the server IP if it matches the specified one.
    async fn remove_ip_if_valid_server_ip(&self, server_ip: &str);

    /// Updates the timestamp of the last received message.
    async fn write_received_times(&self);

    /// Checks if a connection to the specified server IP is needed.
    async fn is_need_connect(&self) -> bool;
}

/// Implementation of ServerSenderTrait for thread-safe server sender.
///
/// This implementation wraps a ServerSender instance with read-write locks
/// to provide thread-safe access.
#[async_trait]
impl ServerSenderTrait for RwServerSender {
    /// Sets a new message sender and server IP, also updating the database.
    async fn add(&self, sx: mpsc::Sender<Message>, server_ip: &str) {
        // Update in-memory state
        let db = {
            let mut guard = self.write().await;
            guard.add(sx, server_ip);
            guard.db.clone()
        };

        log_debug!("set start server_ip: {:?}", server_ip);

        // Persist to database (set_setting handles upsert internally)
        persist_connection_info(db, server_ip).await;
    }

    /// Gets the receiver for connection status updates.
    async fn get_status_receiver(&self) -> Receiver<SenderStatus> {
        self.write().await.get_status_receiver()
    }

    /// Gets the receiver for incoming messages.
    async fn get_handle_message_receiver(&self) -> Receiver<Vec<u8>> {
        self.write().await.get_handle_message_receiver()
    }

    /// Sends a connection status update.
    ///
    /// The underlying `ServerSender::send_status` returns a `bool` indicating
    /// whether the status was sent, but the trait discards it since status
    /// updates are idempotent and dropping a redundant one is acceptable.
    async fn send_status(&self, status: SenderStatus) {
        self.read().await.send_status(status);
    }

    /// Forwards received message data to the application (non-blocking with spillover).
    #[cfg(feature = "bebop")]
    async fn send_handle_message(&self, data: Data<'_>) {
        let mut buf = Vec::new();
        if let Err(e) = data.serialize(&mut buf) {
            log_error!("Failed to serialize Data: {:?}", e);
            return;
        }
        let guard = self.read().await;
        guard.metrics.inc_messages_received();
        guard.send_handle_message(buf);
    }

    /// Forwards received message data to the application (non-blocking with spillover).
    #[cfg(not(feature = "bebop"))]
    async fn send_handle_message(&self, data: Vec<u8>) {
        let guard = self.read().await;
        guard.metrics.inc_messages_received();
        guard.send_handle_message(data);
    }

    /// Sends a message to the connected server.
    ///
    /// Extracts needed data under a brief read lock, then retries with
    /// exponential backoff outside the lock to avoid blocking other operations.
    async fn send(&self, message: Message) {
        // Phase 1: Brief read lock to clone needed data
        let (sender, status_tx, options, server_sender_ref, db, server_ip, metrics) = {
            let guard = self.read().await;
            let Some(sx) = guard.sx.as_ref() else {
                return;
            };
            (
                sx.clone(),
                guard.status_tx.clone(),
                guard.options.clone(),
                guard.server_sender.clone(),
                guard.db.clone(),
                guard.server_ip.clone(),
                guard.metrics.clone(),
            )
        }; // Read lock released

        // Phase 2: Compute retry limit
        let limit_count = match options.retry_seconds > 5 {
            true => 5,
            false => match options.retry_seconds {
                0 | 1 => 1,
                _ => (options.retry_seconds - 1) as u32,
            },
        };

        // Phase 3: Retry loop — NO LOCK HELD
        let send_timeout = Duration::from_secs(options.retry_seconds.max(1));
        let mut backoff = ExponentialBackoff::new(50, 1, limit_count);
        loop {
            match tokio::time::timeout(send_timeout, sender.send(message.clone())).await {
                Ok(Ok(_)) => {
                    metrics.inc_messages_sent();
                    return;
                }
                Ok(Err(e)) => {
                    log_error!(
                        "Send error (channel closed, attempt {}): {:?}",
                        backoff.count() + 1,
                        e
                    );
                    if !backoff.wait().await {
                        metrics.inc_send_errors();
                        // Retries exhausted: notify disconnection + spawn reconnection
                        let _ = status_tx.try_send(SenderStatus::Disconnected);
                        if let Some(ref ss) = server_sender_ref {
                            match options.atomic_websocket_type {
                                AtomicWebsocketType::Internal => {
                                    tokio::spawn(wrap_get_internal_websocket(
                                        db,
                                        ss.clone(),
                                        server_ip,
                                        options,
                                    ));
                                }
                                AtomicWebsocketType::External => {
                                    tokio::spawn(wrap_get_outer_websocket(db, ss.clone(), options));
                                }
                            }
                        }
                        return;
                    }
                }
                Err(_) => {
                    // Timeout: per-connection channel full but open
                    log_error!("Send timeout (attempt {})", backoff.count() + 1);
                    if !backoff.wait().await {
                        metrics.inc_send_errors();
                        let _ = status_tx.try_send(SenderStatus::Disconnected);
                        if let Some(ref ss) = server_sender_ref {
                            match options.atomic_websocket_type {
                                AtomicWebsocketType::Internal => {
                                    tokio::spawn(wrap_get_internal_websocket(
                                        db,
                                        ss.clone(),
                                        server_ip,
                                        options,
                                    ));
                                }
                                AtomicWebsocketType::External => {
                                    tokio::spawn(wrap_get_outer_websocket(db, ss.clone(), options));
                                }
                            }
                        }
                        return;
                    }
                }
            }
        }
    }

    /// Registers a reference to self for recursive operations.
    async fn regist(&mut self, server_sender: RwServerSender) {
        self.write().await.regist(server_sender);
    }

    /// Checks if the current server IP is valid and recently active.
    async fn is_valid_server_ip(&self) -> bool {
        let clone = self.read().await;
        let result = !clone.server_ip.is_empty()
            && clone.server_received_times
                + (match clone.options.retry_seconds {
                    0 => 1,
                    _ => clone.options.retry_seconds as i64,
                } * 2)
                > now().timestamp();
        drop(clone);
        result
    }

    /// Removes the current server IP and drops the connection.
    async fn remove_ip(&self) {
        self.write().await.remove_ip();
    }

    /// Removes the server IP and clears persisted connection info.
    ///
    /// With bebop: clears the `server_ip` field in `ServerConnectInfo` and updates via `set_setting`.
    /// Without bebop: removes the setting entirely via `remove_setting`.
    /// Storage backend (native-db vs in-memory) is abstracted by the common helpers.
    async fn remove_ip_if_valid_server_ip(&self, server_ip: &str) {
        let db = self.read().await.db.clone();
        self.remove_ip().await;

        #[cfg(feature = "bebop")]
        {
            let server_connect_info = match get_setting_by_key(
                db.clone(),
                save_key::SERVER_CONNECT_INFO.to_owned(),
            )
            .await
            {
                Ok(info) => info,
                Err(error) => {
                    log_error!("Failed to get server_connect_info {error:?}");
                    return;
                }
            };

            let Some(server_connect_info) = server_connect_info else {
                return;
            };

            let Ok(mut info) = ServerConnectInfo::deserialize(&server_connect_info.value) else {
                log_error!("Failed to deserialize ServerConnectInfo");
                return;
            };

            // Extract IP from URL format (e.g., "ws://192.168.1.100:9000" -> "192.168.1.100")
            let stored_ip_normalized = info
                .server_ip
                .trim_start_matches("ws://")
                .trim_start_matches("wss://")
                .split(':')
                .next()
                .unwrap_or(info.server_ip);

            let server_ip_normalized = server_ip
                .trim_start_matches("ws://")
                .trim_start_matches("wss://")
                .split(':')
                .next()
                .unwrap_or(server_ip);

            if !stored_ip_normalized.is_empty()
                && !server_ip_normalized.is_empty()
                && stored_ip_normalized != server_ip_normalized
            {
                log_debug!(
                    "IP mismatch: stored={}, attempted={}. Force resetting.",
                    stored_ip_normalized,
                    server_ip_normalized
                );
            }

            info.server_ip = "";
            let mut value = Vec::new();
            if let Err(e) = info.serialize(&mut value) {
                log_error!("Failed to serialize ServerConnectInfo: {:?}", e);
                return;
            }

            if let Err(e) = set_setting(
                db,
                Settings {
                    key: save_key::SERVER_CONNECT_INFO.to_owned(),
                    value,
                },
            )
            .await
            {
                log_error!("Failed to update connection info: {:?}", e);
            }
        }

        #[cfg(not(feature = "bebop"))]
        {
            let _ = server_ip;
            if let Err(e) = remove_setting(db, save_key::SERVER_CONNECT_INFO.to_owned()).await {
                log_error!("Failed to remove connection info: {:?}", e);
            }
        }
    }

    /// Updates the timestamp of the last received message.
    async fn write_received_times(&self) {
        self.write().await.server_received_times = now().timestamp();
    }

    /// Checks if a new connection attempt is needed.
    ///
    /// Returns true when no connection task is currently running (`is_try_connect == false`).
    /// The `is_try_connect` flag is set to true when `handle_websocket` begins and
    /// reset to false when it exits, providing reliable duplicate-connection prevention
    /// regardless of whether the server IP has changed.
    async fn is_need_connect(&self) -> bool {
        !self.read().await.is_try_connect
    }
}

/// Tests deserialization of server connection information.
#[cfg(feature = "bebop")]
#[test]
fn get_sercer_connect_info() {
    let binary: Vec<u8> = vec![0, 0, 0, 0, 5, 0, 0, 0, 49, 54, 50, 53, 48];
    let data = ServerConnectInfo::deserialize(&binary).unwrap();

    println!("{:?}", data);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "native-db"))]
    use std::sync::Arc;

    #[cfg(not(feature = "native-db"))]
    use tokio::sync::{Mutex, RwLock};

    #[cfg(not(feature = "native-db"))]
    use crate::helpers::types::InMemoryStorage;

    #[cfg(not(feature = "native-db"))]
    fn create_test_db() -> DB {
        Arc::new(Mutex::new(InMemoryStorage::new()))
    }

    #[cfg(not(feature = "native-db"))]
    fn create_test_options() -> ClientOptions {
        ClientOptions::default()
    }

    #[cfg(feature = "native-db")]
    use std::sync::Arc;

    #[cfg(feature = "native-db")]
    use tokio::sync::Mutex;

    #[cfg(feature = "native-db")]
    fn create_test_db_native() -> DB {
        use native_db::{Builder, Models};
        let mut models = Models::new();
        models.define::<Settings>().unwrap();
        let models: &'static Models = Box::leak(Box::new(models));
        let temp = tempfile::NamedTempFile::new().unwrap();
        Arc::new(Mutex::new(
            Builder::new().create(models, temp.path()).unwrap(),
        ))
    }

    #[cfg(feature = "native-db")]
    fn create_test_options_native() -> ClientOptions {
        ClientOptions::default()
    }

    // ========================================================================
    // ServerSender 기본 동작 테스트
    // ========================================================================

    #[cfg(not(feature = "native-db"))]
    #[test]
    fn test_server_sender_new() {
        let db = create_test_db();
        let sender = ServerSender::new(db, "127.0.0.1:9000".to_string(), create_test_options());

        assert!(sender.sx.is_none());
        assert_eq!(sender.server_ip, "127.0.0.1:9000");
        assert_eq!(sender.server_received_times, 0);
        assert!(!sender.is_try_connect);
        assert!(sender.server_sender.is_none());
        assert!(sender.status_rx.is_some());
        assert!(sender.handle_message_rx.is_some());
    }

    #[cfg(not(feature = "native-db"))]
    #[test]
    fn test_server_sender_get_status_receiver() {
        let db = create_test_db();
        let mut sender = ServerSender::new(db, "".to_string(), create_test_options());

        // 첫 번째 호출: 성공
        let _rx = sender.get_status_receiver();
        assert!(sender.status_rx.is_none());
    }

    #[cfg(not(feature = "native-db"))]
    #[test]
    #[should_panic(expected = "Receiver already taken")]
    fn test_server_sender_get_status_receiver_double_call_panics() {
        let db = create_test_db();
        let mut sender = ServerSender::new(db, "".to_string(), create_test_options());

        let _rx1 = sender.get_status_receiver();
        let _rx2 = sender.get_status_receiver(); // 패닉 발생
    }

    #[cfg(not(feature = "native-db"))]
    #[test]
    fn test_server_sender_get_handle_message_receiver() {
        let db = create_test_db();
        let mut sender = ServerSender::new(db, "".to_string(), create_test_options());

        let _rx = sender.get_handle_message_receiver();
        assert!(sender.handle_message_rx.is_none());
    }

    #[cfg(not(feature = "native-db"))]
    #[test]
    #[should_panic(expected = "Receiver already taken")]
    fn test_server_sender_get_handle_message_receiver_double_call_panics() {
        let db = create_test_db();
        let mut sender = ServerSender::new(db, "".to_string(), create_test_options());

        let _rx1 = sender.get_handle_message_receiver();
        let _rx2 = sender.get_handle_message_receiver(); // 패닉 발생
    }

    #[cfg(not(feature = "native-db"))]
    #[test]
    fn test_server_sender_regist() {
        let db = create_test_db();
        let mut sender = ServerSender::new(db.clone(), "".to_string(), create_test_options());

        assert!(sender.server_sender.is_none());

        let rw_sender = Arc::new(RwLock::new(ServerSender::new(
            db,
            "".to_string(),
            create_test_options(),
        )));
        sender.regist(rw_sender);

        assert!(sender.server_sender.is_some());
    }

    #[cfg(not(feature = "native-db"))]
    #[test]
    fn test_server_sender_add_updates_state() {
        let db = create_test_db();
        let mut sender = ServerSender::new(db, "".to_string(), create_test_options());

        assert!(sender.sx.is_none());
        assert_eq!(sender.server_ip, "");

        let (tx, _rx) = mpsc::channel(8);
        sender.add(tx, "192.168.1.100:9000");

        assert!(sender.sx.is_some());
        assert_eq!(sender.server_ip, "192.168.1.100:9000");
    }

    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_server_sender_remove_ip_clears_state() {
        let db = create_test_db();
        let mut sender =
            ServerSender::new(db, "192.168.1.100:9000".to_string(), create_test_options());

        let (tx, _rx) = mpsc::channel(8);
        sender.add(tx, "192.168.1.100:9000");

        assert!(!sender.server_ip.is_empty());

        sender.remove_ip();

        assert!(sender.server_ip.is_empty());
        assert!(sender.sx.is_none());
    }

    #[cfg(not(feature = "native-db"))]
    #[test]
    fn test_server_sender_remove_ip_empty_no_panic() {
        let db = create_test_db();
        let mut sender = ServerSender::new(db, "".to_string(), create_test_options());

        // 빈 상태에서 remove_ip 호출해도 패닉 없음
        sender.remove_ip();
        assert!(sender.server_ip.is_empty());
    }

    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_server_sender_send_status() {
        let db = create_test_db();
        let mut sender = ServerSender::new(db, "".to_string(), create_test_options());

        let mut rx = sender.get_status_receiver();

        // 상태 전송
        sender.send_status(SenderStatus::Connected);

        // 수신 확인
        let status = rx.recv().await;
        assert!(status.is_some());
        assert_eq!(status.unwrap(), SenderStatus::Connected);
    }

    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_server_sender_send_handle_message() {
        let db = create_test_db();
        let mut sender = ServerSender::new(db, "".to_string(), create_test_options());

        let mut rx = sender.get_handle_message_receiver();

        // 메시지 전송 (now synchronous)
        sender.send_handle_message(vec![1, 2, 3, 4, 5]);

        // 수신 확인
        let data = rx.recv().await;
        assert!(data.is_some());
        assert_eq!(data.unwrap(), vec![1, 2, 3, 4, 5]);
    }

    // ========================================================================
    // Spillover buffer 테스트
    // ========================================================================

    /// 채널이 가득 찰 때 spillover buffer에 저장되는지 확인
    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_spillover_stores_when_channel_full() {
        let db = create_test_db();
        let mut options = create_test_options();
        options.handler_buffer_size = 2; // 작은 채널로 빠르게 가득 차게 함
        options.spillover_buffer_size = 10;
        let mut sender = ServerSender::new(db, "".to_string(), options);

        let mut rx = sender.get_handle_message_receiver();

        // 채널 용량(2)을 채움
        sender.send_handle_message(vec![1]);
        sender.send_handle_message(vec![2]);

        // 3번째부터는 spillover에 저장되어야 함
        sender.send_handle_message(vec![3]);
        sender.send_handle_message(vec![4]);
        sender.send_handle_message(vec![5]);

        // spillover에 3개 저장되었는지 확인
        let spillover_len = sender.spillover.lock().unwrap().len();
        assert_eq!(spillover_len, 3, "spillover에 3개 메시지가 저장되어야 함");

        // 채널에서 1개 읽으면 spillover drain이 가능해짐
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg, vec![1]);

        // 다음 send_handle_message 호출 시 drain 시도
        sender.send_handle_message(vec![6]);

        // drain 후 spillover 확인 (채널 1칸 비었으므로 1개 drain + 새 메시지 1개 추가)
        let spillover_len = sender.spillover.lock().unwrap().len();
        assert!(
            spillover_len <= 3,
            "drain 후 spillover는 3 이하여야 함, actual: {}",
            spillover_len
        );

        // 전체 메시지 순서 보장: 2, 3, 4, 5, 6 순으로 수신
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg, vec![2]);
    }

    /// spillover buffer cap 초과 시 메시지 drop 확인
    #[cfg(not(feature = "native-db"))]
    #[test]
    fn test_spillover_drops_when_cap_exceeded() {
        let db = create_test_db();
        let mut options = create_test_options();
        options.handler_buffer_size = 1;
        options.spillover_buffer_size = 3; // spillover cap = 3
        let mut sender = ServerSender::new(db, "".to_string(), options);

        let _rx = sender.get_handle_message_receiver();

        // 채널 1칸 채움
        sender.send_handle_message(vec![1]);

        // spillover 3칸 채움
        sender.send_handle_message(vec![2]);
        sender.send_handle_message(vec![3]);
        sender.send_handle_message(vec![4]);

        assert_eq!(sender.spillover.lock().unwrap().len(), 3);

        // cap 초과 → drop되어야 함
        sender.send_handle_message(vec![5]);
        sender.send_handle_message(vec![6]);

        // spillover 크기는 여전히 3 (cap)
        assert_eq!(sender.spillover.lock().unwrap().len(), 3);

        // drop된 메시지 수 확인 (metrics)
        let snapshot = sender.metrics.snapshot();
        assert_eq!(
            snapshot.messages_dropped, 2,
            "cap 초과로 2개 메시지가 drop되어야 함"
        );
    }

    /// spillover drain 후 모든 메시지가 올바른 순서로 수신되는지 확인
    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_spillover_drain_preserves_order() {
        let db = create_test_db();
        let mut options = create_test_options();
        options.handler_buffer_size = 2;
        options.spillover_buffer_size = 100;
        let mut sender = ServerSender::new(db, "".to_string(), options);

        let mut rx = sender.get_handle_message_receiver();

        // 채널(2) + spillover에 메시지 적재
        for i in 1..=6u8 {
            sender.send_handle_message(vec![i]);
        }

        // 채널: [1, 2], spillover: [3, 4, 5, 6]
        assert_eq!(sender.spillover.lock().unwrap().len(), 4);

        // 채널에서 모두 빼면서 drain 유도
        let mut received = Vec::new();
        // 채널에서 2개 읽기
        received.push(rx.recv().await.unwrap());
        received.push(rx.recv().await.unwrap());

        // drain 유도: 새 메시지 전송 시 spillover drain 시도
        sender.send_handle_message(vec![7]);

        // 남은 메시지 모두 수신
        while let Ok(msg) = rx.try_recv() {
            received.push(msg);
        }

        // 순서 확인: 1, 2, 3, ... 순서가 보장되어야 함
        for i in 0..received.len().saturating_sub(1) {
            assert!(
                received[i] <= received[i + 1],
                "순서 위반: {:?} 다음에 {:?}",
                received[i],
                received[i + 1]
            );
        }
    }

    /// spillover buffer가 비어있을 때 drain이 무해한지 확인
    #[cfg(not(feature = "native-db"))]
    #[test]
    fn test_spillover_drain_empty_is_noop() {
        let db = create_test_db();
        let sender = ServerSender::new(db, "".to_string(), create_test_options());

        // 빈 상태에서 drain 호출 — 패닉 없어야 함
        sender.drain_spillover();
        assert_eq!(sender.spillover.lock().unwrap().len(), 0);
    }

    // ========================================================================
    // Spillover buffer 테스트 (native-db)
    // ========================================================================

    /// [native-db] 채널이 가득 찰 때 spillover buffer에 저장되는지 확인
    #[cfg(feature = "native-db")]
    #[tokio::test]
    async fn test_spillover_stores_when_channel_full_native_db() {
        let db = create_test_db_native();
        let mut options = create_test_options_native();
        options.handler_buffer_size = 2;
        options.spillover_buffer_size = 10;
        let mut sender = ServerSender::new(db, "".to_string(), options);

        let mut rx = sender.get_handle_message_receiver();

        // 채널 용량(2)을 채움
        sender.send_handle_message(vec![1]);
        sender.send_handle_message(vec![2]);

        // 3번째부터는 spillover에 저장되어야 함
        sender.send_handle_message(vec![3]);
        sender.send_handle_message(vec![4]);
        sender.send_handle_message(vec![5]);

        let spillover_len = sender.spillover.lock().unwrap().len();
        assert_eq!(spillover_len, 3, "spillover에 3개 메시지가 저장되어야 함");

        // 채널에서 1개 읽으면 spillover drain이 가능해짐
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg, vec![1]);

        // drain 유도
        sender.send_handle_message(vec![6]);

        let spillover_len = sender.spillover.lock().unwrap().len();
        assert!(
            spillover_len <= 3,
            "drain 후 spillover는 3 이하여야 함, actual: {}",
            spillover_len
        );

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg, vec![2]);
    }

    /// [native-db] spillover buffer cap 초과 시 메시지 drop 확인
    #[cfg(feature = "native-db")]
    #[test]
    fn test_spillover_drops_when_cap_exceeded_native_db() {
        let db = create_test_db_native();
        let mut options = create_test_options_native();
        options.handler_buffer_size = 1;
        options.spillover_buffer_size = 3;
        let mut sender = ServerSender::new(db, "".to_string(), options);

        let _rx = sender.get_handle_message_receiver();

        // 채널 1칸 + spillover 3칸 채움
        sender.send_handle_message(vec![1]);
        sender.send_handle_message(vec![2]);
        sender.send_handle_message(vec![3]);
        sender.send_handle_message(vec![4]);

        assert_eq!(sender.spillover.lock().unwrap().len(), 3);

        // cap 초과 → drop
        sender.send_handle_message(vec![5]);
        sender.send_handle_message(vec![6]);

        assert_eq!(sender.spillover.lock().unwrap().len(), 3);

        let snapshot = sender.metrics.snapshot();
        assert_eq!(
            snapshot.messages_dropped, 2,
            "cap 초과로 2개 메시지가 drop되어야 함"
        );
    }

    /// [native-db] spillover drain 후 메시지 순서 보장 확인
    #[cfg(feature = "native-db")]
    #[tokio::test]
    async fn test_spillover_drain_preserves_order_native_db() {
        let db = create_test_db_native();
        let mut options = create_test_options_native();
        options.handler_buffer_size = 2;
        options.spillover_buffer_size = 100;
        let mut sender = ServerSender::new(db, "".to_string(), options);

        let mut rx = sender.get_handle_message_receiver();

        for i in 1..=6u8 {
            sender.send_handle_message(vec![i]);
        }

        assert_eq!(sender.spillover.lock().unwrap().len(), 4);

        let mut received = Vec::new();
        received.push(rx.recv().await.unwrap());
        received.push(rx.recv().await.unwrap());

        // drain 유도
        sender.send_handle_message(vec![7]);

        while let Ok(msg) = rx.try_recv() {
            received.push(msg);
        }

        for i in 0..received.len().saturating_sub(1) {
            assert!(
                received[i] <= received[i + 1],
                "순서 위반: {:?} 다음에 {:?}",
                received[i],
                received[i + 1]
            );
        }
    }

    /// [native-db] spillover buffer 빈 상태에서 drain 안전성 확인
    #[cfg(feature = "native-db")]
    #[test]
    fn test_spillover_drain_empty_is_noop_native_db() {
        let db = create_test_db_native();
        let sender = ServerSender::new(db, "".to_string(), create_test_options_native());

        sender.drain_spillover();
        assert_eq!(sender.spillover.lock().unwrap().len(), 0);
    }

    // ========================================================================
    // Spillover 완전 복구 테스트 — 채널 막힘 → spillover 저장 → 소비자 재개 → 전부 수신
    // ========================================================================

    /// 채널 가득 참 → spillover 저장 → 소비자가 읽기 시작 → spillover drain → 모든 메시지 수신 확인
    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_spillover_full_recovery_all_messages_received() {
        let db = create_test_db();
        let mut options = create_test_options();
        options.handler_buffer_size = 3;
        options.spillover_buffer_size = 100;
        let mut sender = ServerSender::new(db, "".to_string(), options);

        let mut rx = sender.get_handle_message_receiver();

        let total_messages = 20u8;

        // Phase 1: 소비자 없이 메시지 대량 전송 → 채널(3) 초과분은 spillover에 저장
        for i in 1..=total_messages {
            sender.send_handle_message(vec![i]);
        }

        // 채널에 3개, spillover에 17개
        assert_eq!(sender.spillover.lock().unwrap().len(), 17);
        assert_eq!(
            sender.metrics.snapshot().messages_dropped,
            0,
            "drop 없어야 함"
        );

        // Phase 2: 소비자가 읽기 시작 — 읽을 때마다 drain 유도를 위해 send_handle_message 호출
        let mut received = Vec::new();

        // 채널에서 먼저 읽기
        while let Ok(msg) = rx.try_recv() {
            received.push(msg[0]);
        }

        // drain 유도: 소비자가 읽어서 채널에 빈 공간 생김 → drain_spillover
        // drain은 send_handle_message 호출 시 발생하므로, 반복적으로 읽고 drain
        loop {
            sender.drain_spillover();

            let mut drained_any = false;
            while let Ok(msg) = rx.try_recv() {
                received.push(msg[0]);
                drained_any = true;
            }

            let remaining = sender.spillover.lock().unwrap().len();
            if remaining == 0 && !drained_any {
                break;
            }
        }

        // Phase 3: 검증 — 모든 메시지가 순서대로 수신됨
        assert_eq!(
            received.len(),
            total_messages as usize,
            "전체 {}개 메시지 모두 수신되어야 함, actual: {}",
            total_messages,
            received.len()
        );

        let expected: Vec<u8> = (1..=total_messages).collect();
        assert_eq!(
            received, expected,
            "메시지 순서가 1..={} 이어야 함",
            total_messages
        );

        // spillover 완전히 비었는지 확인
        assert_eq!(
            sender.spillover.lock().unwrap().len(),
            0,
            "spillover 완전 drain"
        );
        assert_eq!(sender.metrics.snapshot().messages_dropped, 0, "drop 없음");
    }

    /// [native-db] 채널 가득 참 → spillover 저장 → 소비자 재개 → 모든 메시지 수신 확인
    #[cfg(feature = "native-db")]
    #[tokio::test]
    async fn test_spillover_full_recovery_all_messages_received_native_db() {
        let db = create_test_db_native();
        let mut options = create_test_options_native();
        options.handler_buffer_size = 3;
        options.spillover_buffer_size = 100;
        let mut sender = ServerSender::new(db, "".to_string(), options);

        let mut rx = sender.get_handle_message_receiver();

        let total_messages = 20u8;

        // Phase 1: 소비자 없이 메시지 대량 전송
        for i in 1..=total_messages {
            sender.send_handle_message(vec![i]);
        }

        assert_eq!(sender.spillover.lock().unwrap().len(), 17);
        assert_eq!(sender.metrics.snapshot().messages_dropped, 0);

        // Phase 2: 소비자 재개 — 읽고 drain 반복
        let mut received = Vec::new();

        while let Ok(msg) = rx.try_recv() {
            received.push(msg[0]);
        }

        loop {
            sender.drain_spillover();

            let mut drained_any = false;
            while let Ok(msg) = rx.try_recv() {
                received.push(msg[0]);
                drained_any = true;
            }

            let remaining = sender.spillover.lock().unwrap().len();
            if remaining == 0 && !drained_any {
                break;
            }
        }

        // Phase 3: 검증
        assert_eq!(received.len(), total_messages as usize);

        let expected: Vec<u8> = (1..=total_messages).collect();
        assert_eq!(received, expected);

        assert_eq!(sender.spillover.lock().unwrap().len(), 0);
        assert_eq!(sender.metrics.snapshot().messages_dropped, 0);
    }

    // ========================================================================
    // SenderStatus 테스트
    // ========================================================================

    #[test]
    fn test_sender_status_equality() {
        assert_eq!(SenderStatus::Start, SenderStatus::Start);
        assert_eq!(SenderStatus::Connected, SenderStatus::Connected);
        assert_eq!(SenderStatus::Disconnected, SenderStatus::Disconnected);

        assert_ne!(SenderStatus::Start, SenderStatus::Connected);
        assert_ne!(SenderStatus::Connected, SenderStatus::Disconnected);
    }

    #[test]
    fn test_sender_status_clone() {
        let status = SenderStatus::Connected;
        let cloned = status.clone();
        assert_eq!(status, cloned);
    }

    #[test]
    fn test_sender_status_debug() {
        let status = SenderStatus::Disconnected;
        let debug_str = format!("{:?}", status);
        assert!(debug_str.contains("Disconnected"));
    }

    // ========================================================================
    // ServerSenderTrait (RwServerSender) 테스트
    // ========================================================================

    #[cfg(not(feature = "native-db"))]
    fn create_rw_server_sender() -> RwServerSender {
        let db = create_test_db();
        Arc::new(RwLock::new(ServerSender::new(
            db,
            "".to_string(),
            create_test_options(),
        )))
    }

    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_trait_is_valid_server_ip_empty() {
        let sender = create_rw_server_sender();

        // 빈 server_ip는 유효하지 않음
        assert!(!sender.is_valid_server_ip().await);
    }

    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_trait_is_valid_server_ip_with_recent_activity() {
        let sender = create_rw_server_sender();

        // server_ip 설정 및 최근 활동 시간 업데이트
        {
            let mut guard = sender.write().await;
            guard.server_ip = "192.168.1.100:9000".to_string();
        }

        // 활동 시간 업데이트
        sender.write_received_times().await;

        // 유효한 상태
        assert!(sender.is_valid_server_ip().await);
    }

    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_trait_is_valid_server_ip_with_old_activity() {
        let sender = create_rw_server_sender();

        {
            let mut guard = sender.write().await;
            guard.server_ip = "192.168.1.100:9000".to_string();
            // 오래된 시간 설정 (retry_seconds * 2 보다 오래됨)
            guard.server_received_times = 0;
        }

        // 활동이 오래되어 유효하지 않음
        assert!(!sender.is_valid_server_ip().await);
    }

    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_trait_is_need_connect_not_trying() {
        let sender = create_rw_server_sender();

        {
            let mut guard = sender.write().await;
            guard.server_ip = "192.168.1.100:9000".to_string();
            guard.is_try_connect = false;
        }

        // is_try_connect가 false면 연결 필요 (IP가 달라도 같아도)
        assert!(sender.is_need_connect().await);
        assert!(sender.is_need_connect().await);
    }

    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_trait_is_need_connect_already_trying() {
        let sender = create_rw_server_sender();

        {
            let mut guard = sender.write().await;
            guard.server_ip = "192.168.1.100:9000".to_string();
            guard.is_try_connect = true; // 이미 연결 시도 중
        }

        // is_try_connect가 true면 연결 불필요 (IP가 달라도 같아도)
        assert!(!sender.is_need_connect().await);
        assert!(!sender.is_need_connect().await);
    }

    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_trait_write_received_times_updates_timestamp() {
        let sender = create_rw_server_sender();

        // 초기값 확인
        {
            let guard = sender.read().await;
            assert_eq!(guard.server_received_times, 0);
        }

        // 시간 업데이트
        sender.write_received_times().await;

        // 현재 시간으로 업데이트됨
        {
            let guard = sender.read().await;
            assert!(guard.server_received_times > 0);

            let now_ts = now().timestamp();
            assert!((guard.server_received_times - now_ts).abs() <= 1);
        }
    }

    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_trait_remove_ip() {
        let sender = create_rw_server_sender();

        {
            let mut guard = sender.write().await;
            guard.server_ip = "192.168.1.100:9000".to_string();
        }

        sender.remove_ip().await;

        {
            let guard = sender.read().await;
            assert!(guard.server_ip.is_empty());
        }
    }

    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_trait_send_status_through_rwlock() {
        let sender = create_rw_server_sender();

        let mut rx = sender.get_status_receiver().await;

        sender.send_status(SenderStatus::Start).await;

        let status = rx.recv().await;
        assert!(status.is_some());
        assert_eq!(status.unwrap(), SenderStatus::Start);
    }
}
