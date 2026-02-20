//! Client implementation for WebSocket connections in atomic_websocket.
//!
//! This module provides the core client functionality for establishing and maintaining
//! WebSocket connections to both internal and external servers, including automatic
//! reconnection, connection status monitoring, and ping/pong handling.

use std::error::Error;
use std::net::UdpSocket;
use std::time::Duration;

#[cfg(feature = "bebop")]
use crate::generated::schema::ServerConnectInfo;
use crate::helpers::get_internal_websocket::handle_websocket;
use crate::helpers::get_outer_websocket::wrap_get_outer_websocket;
use crate::helpers::scan_manager::ScanManager;
use crate::helpers::{
    common::{get_setting_by_key, make_ping_message},
    get_internal_websocket::{get_id, wrap_get_internal_websocket},
    server_sender::{SenderStatus, ServerSenderTrait},
    traits::date_time::now,
};
use crate::{log_debug, log_error, AtomicWebsocketType, Settings};
#[cfg(feature = "bebop")]
use bebop::Record;

use tokio::sync::mpsc::Receiver;
use tokio::time::{Instant, MissedTickBehavior};

use super::types::{save_key, RwServerSender, DB};

/// Configuration options for WebSocket client connections.
///
/// Controls various aspects of client behavior including reconnection strategy,
/// ping intervals, and connection timeouts.
#[derive(Clone)]
pub struct ClientOptions {
    /// Whether to enable automatic ping/pong for connection health monitoring
    pub use_ping: bool,

    /// Server URL for external connections
    pub url: String,

    /// Time in seconds between reconnection attempts
    pub retry_seconds: u64,

    /// Whether to remember the last working server IP address
    pub use_keep_ip: bool,

    /// Connection timeout in seconds
    pub connect_timeout_seconds: u64,

    /// AtomicWebsocketType for managing connection types
    /// (internal or external)
    pub atomic_websocket_type: AtomicWebsocketType,

    /// Whether to use TLS for secure connections (only available with rustls feature)
    #[cfg(feature = "rustls")]
    pub use_tls: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            use_ping: true,
            url: "".into(),
            retry_seconds: 30,
            use_keep_ip: false,
            connect_timeout_seconds: 3,
            atomic_websocket_type: AtomicWebsocketType::Internal,
            #[cfg(feature = "rustls")]
            use_tls: true,
        }
    }
}

/// Core client implementation for WebSocket connections.
///
/// Manages connection establishment, message handling, and reconnection logic
/// for both internal (local network) and external server connections.
pub struct AtomicClient {
    /// Server sender for message handling
    pub server_sender: RwServerSender,

    /// Client configuration options
    pub options: ClientOptions,
}

impl AtomicClient {
    /// Initializes an internal network client.
    ///
    /// Sets up client ID registration and starts the ping checking loop
    /// for maintaining connection health.
    ///
    /// # Arguments
    ///
    /// * `db` - Database instance for storing client state
    pub async fn internal_initialize(&self, db: DB) {
        self.regist_id(db.clone()).await;
        tokio::spawn(internal_ping_loop_cheker(
            self.server_sender.clone(),
            self.options.clone(),
        ));
    }

    /// Initializes an external network client.
    ///
    /// Sets up client ID registration, initializes TLS if enabled,
    /// and starts the ping checking loop for external connections.
    ///
    /// # Arguments
    ///
    /// * `db` - Database instance for storing client state
    pub async fn outer_initialize(&self, db: DB) {
        #[cfg(feature = "rustls")]
        self.initial_rustls();
        self.regist_id(db.clone()).await;
        tokio::spawn(outer_ping_loop_cheker(
            self.server_sender.clone(),
            self.options.clone(),
        ));
    }

    /// Initiates a connection to an external server.
    ///
    /// # Arguments
    ///
    /// * `db` - Database instance for storing connection state
    ///
    /// # Returns
    ///
    /// A Result indicating whether the connection process was initiated successfully
    pub async fn get_outer_connect(&self, db: DB) -> Result<(), Box<dyn Error>> {
        get_outer_connect(db, self.server_sender.clone(), self.options.clone()).await
    }

    /// Initiates a connection to an internal server.
    ///
    /// # Arguments
    ///
    /// * `input` - Optional server connection information
    /// * `db` - Database instance for storing connection state
    ///
    /// # Returns
    ///
    /// A Result indicating whether the connection process was initiated successfully
    #[cfg(all(feature = "native-db", feature = "bebop"))]
    pub async fn get_internal_connect(
        &self,
        input: Option<ServerConnectInfo<'_>>,
        db: DB,
    ) -> Result<(), Box<dyn Error>> {
        get_internal_connect(input, db, self.server_sender.clone(), self.options.clone()).await
    }

    /// Initiates a connection to an internal server (without native-db but with bebop).
    #[cfg(all(not(feature = "native-db"), feature = "bebop"))]
    pub async fn get_internal_connect(
        &self,
        _input: Option<ServerConnectInfo<'_>>,
        db: DB,
    ) -> Result<(), Box<dyn Error>> {
        get_internal_connect(None, db, self.server_sender.clone(), self.options.clone()).await
    }

    /// Initiates a connection to an internal server (without bebop).
    #[cfg(not(feature = "bebop"))]
    pub async fn get_internal_connect(
        &self,
        _input: Option<()>,
        db: DB,
    ) -> Result<(), Box<dyn Error>> {
        get_internal_connect(None, db, self.server_sender.clone(), self.options.clone()).await
    }

    /// Initializes the rustls cryptography provider for secure connections.
    ///
    /// Only available when the `rustls` feature is enabled.
    #[cfg(feature = "rustls")]
    pub fn initial_rustls(&self) {
        use rustls::crypto::{ring, CryptoProvider};
        if CryptoProvider::get_default().is_none() {
            let provider = ring::default_provider();
            if let Err(e) = provider.install_default() {
                log_error!("Failed to install rustls crypto provider: {:?}", e);
            }
        }
    }

    /// Registers a unique client ID in the database if one doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `db` - Database instance
    #[cfg(feature = "native-db")]
    pub async fn regist_id(&self, db: DB) {
        let db = db.lock().await;
        let Ok(reader) = db.r_transaction() else {
            log_error!("Failed to create r_transaction for regist_id");
            return;
        };
        let data = match reader.get().primary::<Settings>(save_key::CLIENT_ID) {
            Ok(data) => data,
            Err(e) => {
                log_error!("Failed to get ClientId: {:?}", e);
                return;
            }
        };
        drop(reader);

        if data.is_none() {
            use nanoid::nanoid;
            let Ok(writer) = db.rw_transaction() else {
                log_error!("Failed to create rw_transaction for regist_id");
                return;
            };
            if let Err(e) = writer.insert::<Settings>(Settings {
                key: save_key::CLIENT_ID.to_owned(),
                value: nanoid!().as_bytes().to_vec(),
            }) {
                log_error!("Failed to insert ClientId: {:?}", e);
                return;
            }
            if let Err(e) = writer.commit() {
                log_error!("Failed to commit ClientId: {:?}", e);
            }
        }
    }

    /// Registers a unique client ID in memory if one doesn't exist.
    #[cfg(not(feature = "native-db"))]
    pub async fn regist_id(&self, db: DB) {
        let mut db = db.lock().await;
        if db.get(save_key::CLIENT_ID).is_none() {
            use nanoid::nanoid;
            db.insert(
                save_key::CLIENT_ID.to_owned(),
                nanoid!().as_bytes().to_vec(),
            );
        }
    }

    /// Gets a receiver for connection status updates.
    ///
    /// # Returns
    ///
    /// A channel receiver for connection status events
    pub async fn get_status_receiver(&self) -> Receiver<SenderStatus> {
        self.server_sender.get_status_receiver().await
    }

    /// Gets a receiver for incoming messages.
    ///
    /// # Returns
    ///
    /// A channel receiver for incoming message data
    pub async fn get_handle_message_receiver(&self) -> Receiver<Vec<u8>> {
        self.server_sender.get_handle_message_receiver().await
    }
}

/// Periodic health check for internal network connections.
///
/// Monitors connection health by tracking message timestamps and sends ping
/// messages when needed. Handles reconnection attempts when connection is lost.
///
/// # Arguments
///
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options
#[cfg(all(feature = "native-db", feature = "bebop"))]
async fn internal_ping_loop_cheker(server_sender: RwServerSender, options: ClientOptions) {
    let retry_seconds = options.retry_seconds.max(1);
    let use_keep_ip = options.use_keep_ip;
    let mut interval = tokio::time::interval_at(
        Instant::now() + Duration::from_secs(retry_seconds),
        Duration::from_secs(retry_seconds),
    );
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        let server_sender_read = server_sender.read().await;

        // Check if connection is dead (no messages received for 4x retry period)
        if server_sender_read.server_received_times > 0
            && server_sender_read.server_received_times + (retry_seconds as i64 * 4)
                < now().timestamp()
        {
            drop(server_sender_read);
            server_sender.send_status(SenderStatus::Disconnected).await;

            // Clear server IP if not keeping it
            if !use_keep_ip {
                server_sender.remove_ip().await;
                let db = server_sender.read().await.db.clone();
                let server_connect_info =
                    match get_setting_by_key(db.clone(), save_key::SERVER_CONNECT_INFO.to_owned())
                        .await
                    {
                        Ok(server_connect_info) => server_connect_info,
                        Err(error) => {
                            log_error!("Failed to get server_connect_info {error:?}");
                            None
                        }
                    };

                // Reset stored server IP in database
                if let Some(server_connect_info) = server_connect_info {
                    if let Ok(mut info) = ServerConnectInfo::deserialize(&server_connect_info.value)
                    {
                        info.server_ip = "";
                        let mut value = Vec::new();
                        if info.serialize(&mut value).is_ok() {
                            let db = db.lock().await;
                            if let Ok(writer) = db.rw_transaction() {
                                if writer
                                    .update::<Settings>(
                                        server_connect_info,
                                        Settings {
                                            key: save_key::SERVER_CONNECT_INFO.to_owned(),
                                            value,
                                        },
                                    )
                                    .is_ok()
                                {
                                    let _ = writer.commit();
                                }
                            }
                        }
                    }
                }
            }

            // Attempt reconnection
            let db = server_sender.read().await.db.clone();
            let server_sender = server_sender.clone();
            let options = options.clone();
            tokio::spawn(async move {
                let _ = get_internal_connect(None, db, server_sender, options).await;
                true
            });
        }
        // Send a ping if no messages for 2x retry period
        else if server_sender_read.server_received_times > 0
            && server_sender_read.server_received_times + (retry_seconds as i64 * 2)
                < now().timestamp()
        {
            log_debug!(
                "send: {:?}, current: {:?}",
                server_sender_read.server_received_times,
                now().timestamp()
            );
            if options.use_ping {
                log_debug!("Try ping from loop checker");
                let db = server_sender_read.db.clone();
                drop(server_sender_read);
                let id: String = get_id(db).await;
                server_sender.send(make_ping_message(&id)).await;
            }
        }
        log_debug!("loop server checker finish");
    }
}

/// Periodic health check for internal network connections (simplified version).
#[cfg(not(all(feature = "native-db", feature = "bebop")))]
async fn internal_ping_loop_cheker(server_sender: RwServerSender, options: ClientOptions) {
    let retry_seconds = options.retry_seconds.max(1);
    let use_keep_ip = options.use_keep_ip;
    let mut interval = tokio::time::interval_at(
        Instant::now() + Duration::from_secs(retry_seconds),
        Duration::from_secs(retry_seconds),
    );
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        let server_sender_read = server_sender.read().await;

        // Check if connection is dead
        if server_sender_read.server_received_times > 0
            && server_sender_read.server_received_times + (retry_seconds as i64 * 4)
                < now().timestamp()
        {
            drop(server_sender_read);
            server_sender.send_status(SenderStatus::Disconnected).await;

            if !use_keep_ip {
                server_sender.remove_ip().await;
            }

            // Attempt reconnection
            let db = server_sender.read().await.db.clone();
            let server_sender = server_sender.clone();
            let options = options.clone();
            tokio::spawn(async move {
                let _ = get_internal_connect(None, db, server_sender, options).await;
                true
            });
        }
        // Send a ping if no messages for 2x retry period
        else if server_sender_read.server_received_times > 0
            && server_sender_read.server_received_times + (retry_seconds as i64 * 2)
                < now().timestamp()
        {
            if options.use_ping {
                log_debug!("Try ping from loop checker");
                let db = server_sender_read.db.clone();
                drop(server_sender_read);
                let id: String = get_id(db).await;
                server_sender.send(make_ping_message(&id)).await;
            }
        }
        log_debug!("loop server checker finish");
    }
}

/// Periodic health check for external network connections.
///
/// Similar to internal_ping_loop_cheker but with different timing parameters
/// for external connections, which may have different latency characteristics.
///
/// # Arguments
///
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options
async fn outer_ping_loop_cheker(server_sender: RwServerSender, options: ClientOptions) {
    let retry_seconds = options.retry_seconds.max(1);
    let use_keep_ip = options.use_keep_ip;
    let mut interval = tokio::time::interval_at(
        Instant::now() + Duration::from_secs(retry_seconds),
        Duration::from_secs(retry_seconds),
    );
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        let server_sender_read = server_sender.read().await;

        // Check if connection is dead (no messages for 4x retry period)
        if server_sender_read.server_received_times > 0
            && server_sender_read.server_received_times + (retry_seconds as i64 * 4)
                < now().timestamp()
        {
            drop(server_sender_read);
            server_sender.send_status(SenderStatus::Disconnected).await;

            if !use_keep_ip {
                server_sender.remove_ip().await;
            }

            // Attempt reconnection
            let server_sender = server_sender.clone();
            let options = options.clone();
            let db = server_sender.read().await.db.clone();
            tokio::spawn(async move {
                let _ = get_outer_connect(db, server_sender, options).await;
                true
            });
        }
        // Send a ping if no messages for 2x retry period
        else if server_sender_read.server_received_times > 0
            && server_sender_read.server_received_times + (retry_seconds as i64 * 2)
                < now().timestamp()
        {
            log_debug!(
                "send: {:?}, current: {:?}",
                server_sender_read.server_received_times,
                now().timestamp()
            );

            if options.use_ping {
                log_debug!("Try ping from loop checker");
                let db = server_sender_read.db.clone();
                drop(server_sender_read);
                let id: String = get_id(db).await;
                server_sender.send(make_ping_message(&id)).await;
            }
        }
        log_debug!("loop server checker finish");
    }
}

/// Initiates a connection to an external server.
///
/// # Arguments
///
/// * `db` - Database instance for storing connection state
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options
///
/// # Returns
///
/// A Result indicating whether the connection process was initiated successfully
pub async fn get_outer_connect(
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
) -> Result<(), Box<dyn Error>> {
    // Skip if already attempting to connect
    if server_sender.read().await.is_try_connect {
        return Ok(());
    }

    // If already connected, just update status
    if server_sender.is_valid_server_ip().await {
        server_sender.send_status(SenderStatus::Connected).await;
        return Ok(());
    }

    let server_connect_info =
        get_setting_by_key(db.clone(), save_key::SERVER_CONNECT_INFO.to_owned()).await?;
    log_debug!("server_connect_info: {:?}", server_connect_info);

    // Cannot connect if no URL is provided and no stored server IP
    if options.url.is_empty() && !server_sender.is_valid_server_ip().await {
        server_sender.send_status(SenderStatus::Disconnected).await;
        return Ok(());
    }

    // Spawn connection task
    tokio::spawn(wrap_get_outer_websocket(db, server_sender, options));
    Ok(())
}

/// Initiates a connection to an internal server.
///
/// Handles automatic server discovery when server IP is not provided.
///
/// # Arguments
///
/// * `input` - Optional server connection information
/// * `db` - Database instance for storing connection state
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options
///
/// # Returns
///
/// A Result indicating whether the connection process was initiated successfully
#[cfg(all(feature = "native-db", feature = "bebop"))]
pub async fn get_internal_connect(
    input: Option<ServerConnectInfo<'_>>,
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
) -> Result<(), Box<dyn Error>> {
    // Skip if already attempting to connect
    if server_sender.read().await.is_try_connect {
        return Ok(());
    }

    // If already connected, just update status
    if server_sender.is_valid_server_ip().await {
        server_sender.send_status(SenderStatus::Connected).await;
        return Ok(());
    }

    let server_connect_info =
        get_setting_by_key(db.clone(), save_key::SERVER_CONNECT_INFO.to_owned()).await?;
    log_debug!("server_connect_info: {:?}", server_connect_info);

    // Store connection info in database if provided and not already present
    if let (Some(input_ref), None) = (input.as_ref(), server_connect_info.as_ref()) {
        let db_clone = db.lock().await;
        let writer = db_clone.rw_transaction()?;
        let mut value = Vec::new();
        ServerConnectInfo {
            server_ip: "",
            port: input_ref.port,
        }
        .serialize(&mut value)?;
        writer.insert::<Settings>(Settings {
            key: save_key::SERVER_CONNECT_INFO.to_owned(),
            value,
        })?;
        writer.commit()?;
        drop(db_clone);
    }

    // Cannot connect if no input or stored connection info
    if input.is_none() && server_connect_info.is_none() {
        server_sender.send_status(SenderStatus::Disconnected).await;
        return Ok(());
    }

    // Determine connection info to use
    let connect_info_data = match input.as_ref() {
        Some(info) => ServerConnectInfo {
            server_ip: match server_connect_info.as_ref() {
                Some(server_connect_info) => {
                    match ServerConnectInfo::deserialize(&server_connect_info.value) {
                        Ok(info) => info.server_ip,
                        Err(_) => "",
                    }
                }
                None => "",
            },
            port: info.port,
        },
        None => {
            let Some(ref stored_info) = server_connect_info else {
                server_sender.send_status(SenderStatus::Disconnected).await;
                return Ok(());
            };
            match ServerConnectInfo::deserialize(&stored_info.value) {
                Ok(info) => info,
                Err(e) => {
                    log_error!("Failed to deserialize ServerConnectInfo: {:?}", e);
                    server_sender.send_status(SenderStatus::Disconnected).await;
                    return Ok(());
                }
            }
        }
    };

    // Get local IP address for connection
    let ip = get_ip_address();

    // Cannot connect without local IP
    if ip.is_empty() {
        server_sender.send_status(SenderStatus::Disconnected).await;
        return Ok(());
    }

    // Connect directly to known server IP or use discovery
    match connect_info_data.server_ip {
        // Empty server IP means use local network discovery
        "" => {
            let (server_ip, ws_stream) = ScanManager::new(connect_info_data.port).run().await;
            tokio::spawn(async move {
                if let Err(error) =
                    handle_websocket(db, server_sender.clone(), options, server_ip, ws_stream).await
                {
                    log_error!("Error handling websocket: {:?}", error);
                    server_sender.write().await.is_try_connect = false;
                }
            });
        }
        // Connect to known server IP
        _server_ip => {
            tokio::spawn(wrap_get_internal_websocket(
                db.clone(),
                server_sender.clone(),
                _server_ip.into(),
                options.clone(),
            ));
        }
    };

    Ok(())
}

/// Initiates a connection to an internal server (simplified version).
#[cfg(not(all(feature = "native-db", feature = "bebop")))]
pub async fn get_internal_connect(
    _input: Option<()>,
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
) -> Result<(), Box<dyn Error>> {
    // Skip if already attempting to connect
    if server_sender.read().await.is_try_connect {
        return Ok(());
    }

    // If already connected, just update status
    if server_sender.is_valid_server_ip().await {
        server_sender.send_status(SenderStatus::Connected).await;
        return Ok(());
    }

    // Get local IP address for connection
    let ip = get_ip_address();

    // Cannot connect without local IP
    if ip.is_empty() {
        server_sender.send_status(SenderStatus::Disconnected).await;
        return Ok(());
    }

    // Use local network discovery
    let (server_ip, ws_stream) = ScanManager::new("9000").run().await;
    tokio::spawn(async move {
        if let Err(error) =
            handle_websocket(db, server_sender.clone(), options, server_ip, ws_stream).await
        {
            log_error!("Error handling websocket: {:?}", error);
            server_sender.write().await.is_try_connect = false;
        }
    });

    Ok(())
}

/// Determines the local IP address by creating a UDP socket.
///
/// This function requires internet connectivity to properly determine the local IP address,
/// as it attempts to connect to Google's DNS server (8.8.8.8) to identify the correct
/// network interface.
///
/// # Returns
///
/// The local IP address as a string, or an empty string if it cannot be determined
pub fn get_ip_address() -> String {
    let socket = UdpSocket::bind("0.0.0.0:0");
    let socket = match socket {
        Ok(socket) => socket,
        Err(_) => return "".into(),
    };
    // Connects to Google's DNS server to determine the correct network interface
    match socket.connect("8.8.8.8:80") {
        Ok(_) => {}
        Err(_) => return "".into(),
    };
    let addr = match socket.local_addr() {
        Ok(addr) => addr,
        Err(_) => return "".into(),
    };
    addr.ip().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_options_default() {
        let options = ClientOptions::default();
        assert!(options.use_ping);
        assert_eq!(options.url, "");
        assert_eq!(options.retry_seconds, 30);
        assert!(!options.use_keep_ip);
        assert_eq!(options.connect_timeout_seconds, 3);
        assert!(matches!(
            options.atomic_websocket_type,
            AtomicWebsocketType::Internal
        ));
    }

    #[cfg(feature = "rustls")]
    #[test]
    fn test_client_options_default_with_tls() {
        let options = ClientOptions::default();
        assert!(options.use_tls);
    }

    #[test]
    fn test_client_options_clone() {
        let options = ClientOptions {
            use_ping: false,
            url: "ws://example.com:9000".to_string(),
            retry_seconds: 60,
            use_keep_ip: true,
            connect_timeout_seconds: 10,
            atomic_websocket_type: AtomicWebsocketType::External,
            #[cfg(feature = "rustls")]
            use_tls: false,
        };

        let cloned = options.clone();
        assert!(!cloned.use_ping);
        assert_eq!(cloned.url, "ws://example.com:9000");
        assert_eq!(cloned.retry_seconds, 60);
        assert!(cloned.use_keep_ip);
        assert_eq!(cloned.connect_timeout_seconds, 10);
        assert!(matches!(
            cloned.atomic_websocket_type,
            AtomicWebsocketType::External
        ));
    }

    #[test]
    fn test_client_options_custom_values() {
        let options = ClientOptions {
            use_ping: false,
            url: "192.168.1.100:9000".to_string(),
            retry_seconds: 5,
            use_keep_ip: true,
            connect_timeout_seconds: 1,
            atomic_websocket_type: AtomicWebsocketType::Internal,
            #[cfg(feature = "rustls")]
            use_tls: true,
        };

        assert!(!options.use_ping);
        assert_eq!(options.url, "192.168.1.100:9000");
        assert_eq!(options.retry_seconds, 5);
        assert!(options.use_keep_ip);
        assert_eq!(options.connect_timeout_seconds, 1);
    }

    #[test]
    fn test_get_ip_address_format() {
        // This test may fail in environments without network connectivity
        let ip = get_ip_address();
        if !ip.is_empty() {
            // Verify it looks like an IPv4 address
            let parts: Vec<&str> = ip.split('.').collect();
            assert_eq!(parts.len(), 4, "IP should have 4 octets");
            for part in parts {
                let num: Result<u8, _> = part.parse();
                assert!(num.is_ok(), "Each octet should be a valid u8");
            }
        }
        // If ip is empty, it means no network connectivity - that's OK for testing
    }
}
