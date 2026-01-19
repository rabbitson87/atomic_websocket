//! Client-side server connection management for atomic_websocket.
//!
//! This module provides functionality for managing connections to WebSocket servers,
//! including message sending, connection status tracking, and automatic reconnection.

use std::time::Duration;

use async_trait::async_trait;
#[cfg(feature = "bebop")]
use bebop::Record;
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    time::sleep,
};
use tokio_tungstenite::tungstenite::Message;

#[cfg(feature = "bebop")]
use crate::generated::schema::{Data, ServerConnectInfo};
use crate::{
    helpers::{
        common::get_setting_by_key, get_internal_websocket::wrap_get_internal_websocket,
        get_outer_websocket::wrap_get_outer_websocket,
    },
    log_debug, log_error, AtomicWebsocketType, Settings,
};

use crate::helpers::traits::date_time::now;

use super::{
    common::make_disconnect_message,
    internal_client::ClientOptions,
    types::{save_key, RwServerSender, DB},
};

/// Persists server connection info to the database.
///
/// This is a helper function extracted from the add() method to improve
/// code organization and testability.
#[cfg(all(feature = "native-db", feature = "bebop"))]
async fn persist_connection_info(db: DB, server_ip: &str, existing_info: Option<Settings>) {
    let db_guard = db.lock().await;
    let Ok(writer) = db_guard.rw_transaction() else {
        log_error!("Failed to create rw_transaction");
        return;
    };

    match existing_info {
        Some(before_data) => {
            update_existing_connection_info(&writer, before_data, server_ip);
        }
        None => {
            create_new_connection_info(&writer, server_ip);
        }
    }

    if let Err(e) = writer.commit() {
        log_error!("Failed to commit transaction: {:?}", e);
    }
}

#[cfg(all(not(feature = "native-db"), feature = "bebop"))]
async fn persist_connection_info(db: DB, server_ip: &str, _existing_info: Option<Settings>) {
    let mut db_guard = db.lock().await;
    let port = server_ip.split(':').nth(1).unwrap_or("");
    let data = ServerConnectInfo { server_ip, port };
    let mut value = Vec::new();
    if let Err(e) = data.serialize(&mut value) {
        log_error!("Failed to serialize ServerConnectInfo: {:?}", e);
        return;
    }
    db_guard.insert(save_key::SERVER_CONNECT_INFO.to_owned(), value);
}

/// For native-db only (no bebop) - store raw server_ip as bytes
#[cfg(all(feature = "native-db", not(feature = "bebop")))]
async fn persist_connection_info(db: DB, server_ip: &str, existing_info: Option<Settings>) {
    let db_guard = db.lock().await;
    let Ok(writer) = db_guard.rw_transaction() else {
        log_error!("Failed to create rw_transaction");
        return;
    };

    let new_settings = Settings {
        key: save_key::SERVER_CONNECT_INFO.to_owned(),
        value: server_ip.as_bytes().to_vec(),
    };

    match existing_info {
        Some(before_data) => {
            if let Err(e) = writer.remove::<Settings>(before_data) {
                log_error!("Failed to remove old Settings: {:?}", e);
            }
            if let Err(e) = writer.insert::<Settings>(new_settings) {
                log_error!("Failed to insert new Settings: {:?}", e);
            }
        }
        None => {
            if let Err(e) = writer.insert::<Settings>(new_settings) {
                log_error!("Failed to insert new Settings: {:?}", e);
            }
        }
    }

    if let Err(e) = writer.commit() {
        log_error!("Failed to commit transaction: {:?}", e);
    }
}

/// For no native-db and no bebop - use in-memory storage
#[cfg(all(not(feature = "native-db"), not(feature = "bebop")))]
async fn persist_connection_info(db: DB, server_ip: &str, _existing_info: Option<Settings>) {
    let mut db_guard = db.lock().await;
    db_guard.insert(
        save_key::SERVER_CONNECT_INFO.to_owned(),
        server_ip.as_bytes().to_vec(),
    );
}

/// Updates existing connection info in the database.
#[cfg(all(feature = "native-db", feature = "bebop"))]
fn update_existing_connection_info(
    writer: &native_db::transaction::RwTransaction,
    before_data: Settings,
    server_ip: &str,
) {
    let before_value = before_data.value.clone();
    let Ok(mut data) = ServerConnectInfo::deserialize(&before_value) else {
        log_error!("Failed to deserialize ServerConnectInfo");
        return;
    };

    data.server_ip = server_ip;
    let mut value = Vec::new();
    if let Err(e) = data.serialize(&mut value) {
        log_error!("Failed to serialize ServerConnectInfo: {:?}", e);
        return;
    }

    if let Err(e) = writer.remove::<Settings>(before_data) {
        log_error!("Failed to remove old Settings: {:?}", e);
        return;
    }
    if let Err(e) = writer.insert::<Settings>(Settings {
        key: save_key::SERVER_CONNECT_INFO.to_owned(),
        value,
    }) {
        log_error!("Failed to insert Settings: {:?}", e);
    }
}

/// Creates new connection info in the database.
#[cfg(all(feature = "native-db", feature = "bebop"))]
fn create_new_connection_info(writer: &native_db::transaction::RwTransaction, server_ip: &str) {
    let mut value = Vec::new();
    let port = server_ip.split(':').nth(1).unwrap_or("");
    let data = ServerConnectInfo { server_ip, port };

    if let Err(e) = data.serialize(&mut value) {
        log_error!("Failed to serialize ServerConnectInfo: {:?}", e);
        return;
    }
    if let Err(e) = writer.insert::<Settings>(Settings {
        key: save_key::SERVER_CONNECT_INFO.to_owned(),
        value,
    }) {
        log_error!("Failed to insert Settings: {:?}", e);
    }
}

/// Represents the current status of a server connection.
#[derive(Clone, Debug, PartialEq)]
pub enum SenderStatus {
    /// Connection initialization started
    Start,
    /// Successfully connected to the server
    Connected,
    /// Disconnected from the server
    Disconnected,
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
        let (status_tx, status_rx) = mpsc::channel(8);
        let (handle_message_tx, handle_message_rx) = mpsc::channel(8);

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
    /// # Arguments
    ///
    /// * `status` - The connection status to send
    pub fn send_status(&self, status: SenderStatus) {
        let _ = self.status_tx.try_send(status);
    }

    /// Forwards received message data to the application.
    ///
    /// # Arguments
    ///
    /// * `data` - Binary message data
    pub fn send_handle_message(&self, data: Vec<u8>) {
        let _ = self.handle_message_tx.try_send(data);
    }

    /// Sends a message to the connected server.
    ///
    /// Implements exponential backoff retry logic with configurable limits.
    ///
    /// # Arguments
    ///
    /// * `message` - WebSocket message to send
    pub async fn send(&mut self, message: Message) {
        if let Some(sx) = &self.sx {
            let sender = sx.clone();
            let mut backoff = Duration::from_millis(50); // Start with 50ms
            let max_backoff = Duration::from_secs(1); // Maximum 1 second
            let mut count = 0;

            // Determine retry count limit based on configured retry seconds
            let limit_count = match self.options.retry_seconds > 5 {
                true => 5,
                false => match self.options.retry_seconds {
                    0 | 1 => 1,
                    _ => self.options.retry_seconds - 1,
                },
            };

            // Initial send attempt
            match sender.send(message.clone()).await {
                Ok(_) => return,
                Err(e) => {
                    log_error!("Initial send error: {:?}", e);
                }
            }

            // Retry loop with exponential backoff
            loop {
                if count >= limit_count {
                    self.send_status(SenderStatus::Disconnected);

                    // Attempt reconnection after max retries
                    if let Some(server_sender) = &self.server_sender {
                        match self.options.atomic_websocket_type {
                            AtomicWebsocketType::Internal => {
                                tokio::spawn(wrap_get_internal_websocket(
                                    self.db.clone(),
                                    server_sender.clone(),
                                    self.server_ip.clone(),
                                    self.options.clone(),
                                ));
                            }
                            AtomicWebsocketType::External => {
                                tokio::spawn(wrap_get_outer_websocket(
                                    self.db.clone(),
                                    server_sender.clone(),
                                    self.options.clone(),
                                ));
                            }
                        }
                    }
                    break;
                }

                backoff = std::cmp::min(backoff * 2, max_backoff);
                sleep(backoff).await;

                count += 1;
                log_debug!("Retrying send (attempt {})", count + 1);

                match sx.clone().send(message.clone()).await {
                    Ok(_) => {
                        log_debug!("Send succeeded on retry {}", count);
                        return;
                    }
                    Err(e) => {
                        log_error!("Retry {} failed: {:?}", count, e);
                        continue;
                    }
                }
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
    async fn is_need_connect(&self, server_ip: &str) -> bool;
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

        // Fetch existing connection info
        let server_connect_info =
            match get_setting_by_key(db.clone(), save_key::SERVER_CONNECT_INFO.to_owned()).await {
                Ok(info) => info,
                Err(error) => {
                    log_debug!("Failed to get server_connect_info {error:?}");
                    None
                }
            };

        // Persist to database
        persist_connection_info(db, server_ip, server_connect_info).await;
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
    async fn send_status(&self, status: SenderStatus) {
        self.read().await.send_status(status);
    }

    /// Forwards received message data to the application.
    #[cfg(feature = "bebop")]
    async fn send_handle_message(&self, data: Data<'_>) {
        let mut buf = Vec::new();
        if let Err(e) = data.serialize(&mut buf) {
            log_error!("Failed to serialize Data: {:?}", e);
            return;
        }
        self.write().await.send_handle_message(buf);
    }

    /// Forwards received message data to the application (raw bytes version).
    #[cfg(not(feature = "bebop"))]
    async fn send_handle_message(&self, data: Vec<u8>) {
        self.write().await.send_handle_message(data);
    }

    /// Sends a message to the connected server.
    async fn send(&self, message: Message) {
        self.write().await.send(message).await;
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

    /// Removes the server IP if it matches the specified one and updates the database.
    /// If the stored IP doesn't match, force reset to allow re-scanning.
    #[cfg(all(feature = "native-db", feature = "bebop"))]
    async fn remove_ip_if_valid_server_ip(&self, server_ip: &str) {
        let db = self.read().await.db.clone();
        let server_connect_info =
            match get_setting_by_key(db.clone(), save_key::SERVER_CONNECT_INFO.to_owned()).await {
                Ok(server_connect_info) => server_connect_info,
                Err(error) => {
                    log_error!("Failed to get server_connect_info {error:?}");
                    None
                }
            };

        let Some(server_connect_info) = server_connect_info else {
            self.remove_ip().await;
            return;
        };

        let Ok(mut info) = ServerConnectInfo::deserialize(&server_connect_info.value) else {
            log_error!("Failed to deserialize ServerConnectInfo");
            self.remove_ip().await;
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

        // If IPs don't match at all, force reset for re-scan
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

        self.remove_ip().await;
        info.server_ip = "";
        let mut value = Vec::new();
        if let Err(e) = info.serialize(&mut value) {
            log_error!("Failed to serialize ServerConnectInfo: {:?}", e);
            return;
        }

        let db = db.lock().await;
        let Ok(writer) = db.rw_transaction() else {
            log_error!("Failed to create rw_transaction");
            return;
        };

        if let Err(e) = writer.update::<Settings>(
            server_connect_info,
            Settings {
                key: save_key::SERVER_CONNECT_INFO.to_owned(),
                value,
            },
        ) {
            log_error!("Failed to update Settings: {:?}", e);
            return;
        }

        if let Err(e) = writer.commit() {
            log_error!("Failed to commit transaction: {:?}", e);
        }
    }

    /// Removes the server IP if it matches the specified one (native-db only, no bebop).
    /// Force reset on connection failure to allow re-scanning.
    #[cfg(all(feature = "native-db", not(feature = "bebop")))]
    async fn remove_ip_if_valid_server_ip(&self, _server_ip: &str) {
        let db = self.read().await.db.clone();
        let server_connect_info =
            match get_setting_by_key(db.clone(), save_key::SERVER_CONNECT_INFO.to_owned()).await {
                Ok(info) => info,
                Err(error) => {
                    log_error!("Failed to get server_connect_info {error:?}");
                    None
                }
            };

        self.remove_ip().await;

        let Some(info) = server_connect_info else {
            return;
        };

        // Clear the stored info using native-db transaction
        let db_guard = db.lock().await;
        let Ok(writer) = db_guard.rw_transaction() else {
            log_error!("Failed to create rw_transaction");
            return;
        };
        if let Err(e) = writer.remove::<Settings>(info) {
            log_error!("Failed to remove Settings: {:?}", e);
        }
        if let Err(e) = writer.commit() {
            log_error!("Failed to commit transaction: {:?}", e);
        }
    }

    /// Removes the server IP if it matches the specified one (in-memory version, no native-db).
    /// Force reset on connection failure to allow re-scanning.
    #[cfg(not(feature = "native-db"))]
    async fn remove_ip_if_valid_server_ip(&self, _server_ip: &str) {
        let db = self.read().await.db.clone();

        self.remove_ip().await;
        let mut db_guard = db.lock().await;
        db_guard.remove(save_key::SERVER_CONNECT_INFO);
    }

    /// Updates the timestamp of the last received message.
    async fn write_received_times(&self) {
        self.write().await.server_received_times = now().timestamp();
    }

    /// Checks if a connection to the specified server IP is needed.
    async fn is_need_connect(&self, server_ip: &str) -> bool {
        let clone = self.read().await;
        server_ip != clone.server_ip && !clone.is_try_connect
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
        let mut sender = ServerSender::new(db, "192.168.1.100:9000".to_string(), create_test_options());

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

        // 메시지 전송
        sender.send_handle_message(vec![1, 2, 3, 4, 5]);

        // 수신 확인
        let data = rx.recv().await;
        assert!(data.is_some());
        assert_eq!(data.unwrap(), vec![1, 2, 3, 4, 5]);
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
    async fn test_trait_is_need_connect_different_ip() {
        let sender = create_rw_server_sender();

        {
            let mut guard = sender.write().await;
            guard.server_ip = "192.168.1.100:9000".to_string();
            guard.is_try_connect = false;
        }

        // 다른 IP로 연결 필요
        assert!(sender.is_need_connect("192.168.1.200:9000").await);
    }

    #[cfg(not(feature = "native-db"))]
    #[tokio::test]
    async fn test_trait_is_need_connect_same_ip() {
        let sender = create_rw_server_sender();

        {
            let mut guard = sender.write().await;
            guard.server_ip = "192.168.1.100:9000".to_string();
            guard.is_try_connect = false;
        }

        // 같은 IP는 연결 불필요
        assert!(!sender.is_need_connect("192.168.1.100:9000").await);
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

        // 이미 연결 시도 중이면 연결 불필요
        assert!(!sender.is_need_connect("192.168.1.200:9000").await);
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
