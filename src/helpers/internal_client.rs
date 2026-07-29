//! Client implementation for WebSocket connections in atomic_websocket.
//!
//! This module provides the core client functionality for establishing and maintaining
//! WebSocket connections to both internal and external servers, including automatic
//! reconnection, connection status monitoring, and ping/pong handling.

use std::error::Error;
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "bebop")]
use crate::generated::schema::ServerConnectInfo;
use crate::helpers::get_internal_websocket::{handle_websocket, TryConnectGuard};
use crate::helpers::get_outer_websocket::wrap_get_outer_websocket;
use crate::helpers::scan_manager::ScanManager;
use crate::helpers::{
    common::make_ping_message,
    connection_store::ConnectionStore,
    get_internal_websocket::wrap_get_internal_websocket,
    server_sender::{to_ws_url, SenderStatus, ServerSenderTrait},
    traits::date_time::now,
};
use crate::{helpers::metrics::Metrics, log_debug, log_error, AtomicWebsocketType};

use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;

use super::types::RwServerSender;

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

    /// Buffer size for the incoming message handler channel (default: 256)
    pub handler_buffer_size: usize,

    /// Buffer size for the connection status channel (default: 8)
    pub status_buffer_size: usize,

    /// Buffer size for the per-connection outgoing message channel (default: 8)
    pub per_connection_buffer_size: usize,

    /// Maximum size of the spillover buffer for handler messages (default: 1024).
    /// When the handler channel is full, messages are buffered here instead of
    /// blocking. Messages are dropped only when this buffer also reaches its cap.
    pub spillover_buffer_size: usize,

    /// Whether `get_internal_connect` may auto-scan the local subnet when no
    /// server IP is known (default: `false`).
    ///
    /// In fixed-IP deployments the server address is typed in (or saved) ahead
    /// of time, so auto-discovery is off by default — when the IP is unknown the
    /// client simply reports `Disconnected` and the application keeps running.
    /// Discovery can still be triggered explicitly via
    /// [`AtomicClient::scan_and_connect`] (the "search" button).
    pub use_scan_discovery: bool,

    /// Maximum time in seconds an explicit scan ([`AtomicClient::scan_and_connect`])
    /// runs before giving up (default: 60). Prevents an endless scan when no
    /// server is present.
    pub scan_timeout_seconds: u64,
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
            handler_buffer_size: 256,
            status_buffer_size: 8,
            per_connection_buffer_size: 8,
            spillover_buffer_size: 1024,
            use_scan_discovery: false,
            scan_timeout_seconds: 60,
        }
    }
}

/// Core client implementation for WebSocket connections.
///
/// Manages connection establishment, message handling, and reconnection logic
/// for both internal (local network) and external server connections.
/// Supports graceful disconnect via `disconnect()`.
pub struct AtomicClient {
    /// Server sender for message handling
    pub server_sender: RwServerSender,

    /// Client configuration options
    pub options: ClientOptions,

    /// Cancellation token for graceful shutdown
    pub(crate) cancel_token: CancellationToken,
}

impl AtomicClient {
    /// Initializes an internal network client.
    ///
    /// Sets up client ID registration and starts the ping checking loop
    /// for maintaining connection health.
    ///
    /// # Arguments
    ///
    /// * `connection_store` - Persistence for connection-identity state
    pub async fn internal_initialize(&self, connection_store: Arc<dyn ConnectionStore>) {
        self.regist_id(connection_store).await;
        tokio::spawn(internal_ping_loop_cheker(
            self.server_sender.clone(),
            self.options.clone(),
            self.cancel_token.clone(),
        ));
    }

    /// Initializes an external network client.
    ///
    /// Sets up client ID registration, initializes TLS if enabled,
    /// and starts the ping checking loop for external connections.
    ///
    /// # Arguments
    ///
    /// * `connection_store` - Persistence for connection-identity state
    pub async fn outer_initialize(&self, connection_store: Arc<dyn ConnectionStore>) {
        #[cfg(feature = "rustls")]
        self.initial_rustls();
        self.regist_id(connection_store).await;
        tokio::spawn(outer_ping_loop_cheker(
            self.server_sender.clone(),
            self.options.clone(),
            self.cancel_token.clone(),
        ));
    }

    /// Gracefully disconnects the client.
    ///
    /// Cancels all background tasks (ping loop checker) and cleans up
    /// the connection state.
    pub async fn disconnect(&self) {
        self.cancel_token.cancel();
        self.server_sender.remove_ip().await;
    }

    /// Explicitly scans the local network for a server — the "search" button.
    ///
    /// Auto-discovery is off by default (`use_scan_discovery == false`) so
    /// fixed-IP deployments stay deterministic. Call this to run a one-off scan
    /// when the server IP is unknown (e.g. first-time setup). On success the
    /// connection is handed off and the discovered IP is persisted, so later
    /// launches connect directly without scanning.
    ///
    /// The scan is bounded by `ClientOptions::scan_timeout_seconds` and runs the
    /// network probing on the Tokio runtime without blocking the caller's thread.
    ///
    /// # Arguments
    ///
    /// * `port` - The port to scan for (e.g. "9000")
    /// * `connection_store` - Persistence used to persist the discovered connection info
    ///
    /// # Returns
    ///
    /// `true` if a server was found and a connection was started, `false`
    /// otherwise (timeout, already connecting, or no local network).
    pub async fn scan_and_connect(&self, port: &str, connection_store: Arc<dyn ConnectionStore>) -> bool {
        // Respect the same duplicate-connection guard as the normal path.
        if !self.server_sender.is_need_connect().await {
            return false;
        }
        // Already connected — nothing to do.
        if self.server_sender.is_valid_server_ip().await {
            self.server_sender.send_status(SenderStatus::Connected).await;
            return true;
        }
        // Single-flight: don't stack a second scan on top of an in-progress one.
        {
            let mut guard = self.server_sender.write().await;
            if guard.is_scanning {
                return false;
            }
            guard.is_scanning = true;
        }
        // Scanning needs the local subnet to build the candidate list.
        if get_ip_address().is_empty() {
            self.server_sender.write().await.is_scanning = false;
            self.server_sender.send_status(SenderStatus::Disconnected).await;
            return false;
        }

        self.server_sender.send_status(SenderStatus::Connecting).await;

        let mut manager = ScanManager::new(port);
        let scan_timeout = Duration::from_secs(self.options.scan_timeout_seconds.max(1));
        let found = manager.run_with_timeout(scan_timeout).await;

        self.server_sender.write().await.is_scanning = false;

        match found {
            Some((server_ip, ws_stream)) => {
                // Claim the single-flight guard now, before handing off to
                // handle_websocket — closes the gap between "scan succeeded"
                // and "connection registered" that a concurrent reconnect
                // trigger could otherwise slip through.
                let Some(connect_guard) =
                    TryConnectGuard::try_acquire(self.server_sender.clone()).await
                else {
                    log_debug!(
                        "Scan found a server but a connection attempt is already in progress"
                    );
                    return false;
                };
                let server_sender = self.server_sender.clone();
                let options = self.options.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_websocket(
                        connection_store,
                        server_sender.clone(),
                        options,
                        server_ip,
                        ws_stream,
                        connect_guard,
                    )
                    .await
                    {
                        log_error!("Error handling websocket: {:?}", error);
                    }
                });
                true
            }
            None => {
                self.server_sender.send_status(SenderStatus::Disconnected).await;
                false
            }
        }
    }

    /// Initiates a connection to an external server.
    ///
    /// # Arguments
    ///
    /// * `connection_store` - Persistence for connection-identity state
    ///
    /// # Returns
    ///
    /// A Result indicating whether the connection process was initiated successfully
    pub async fn get_outer_connect(
        &self,
        connection_store: Arc<dyn ConnectionStore>,
    ) -> Result<(), Box<dyn Error>> {
        get_outer_connect(connection_store, self.server_sender.clone(), self.options.clone()).await
    }

    /// Initiates a connection to an internal server.
    ///
    /// # Arguments
    ///
    /// * `input` - Optional server connection information
    /// * `connection_store` - Persistence for connection-identity state
    ///
    /// # Returns
    ///
    /// A Result indicating whether the connection process was initiated successfully
    #[cfg(all(feature = "native-db", feature = "bebop"))]
    pub async fn get_internal_connect(
        &self,
        input: Option<ServerConnectInfo<'_>>,
        connection_store: Arc<dyn ConnectionStore>,
    ) -> Result<(), Box<dyn Error>> {
        get_internal_connect(
            input,
            connection_store,
            self.server_sender.clone(),
            self.options.clone(),
        )
        .await
    }

    /// Initiates a connection to an internal server (without native-db but with bebop).
    #[cfg(all(not(feature = "native-db"), feature = "bebop"))]
    pub async fn get_internal_connect(
        &self,
        _input: Option<ServerConnectInfo<'_>>,
        connection_store: Arc<dyn ConnectionStore>,
    ) -> Result<(), Box<dyn Error>> {
        get_internal_connect(
            None,
            connection_store,
            self.server_sender.clone(),
            self.options.clone(),
        )
        .await
    }

    /// Initiates a connection to an internal server (without bebop).
    #[cfg(not(feature = "bebop"))]
    pub async fn get_internal_connect(
        &self,
        _input: Option<()>,
        connection_store: Arc<dyn ConnectionStore>,
    ) -> Result<(), Box<dyn Error>> {
        get_internal_connect(
            None,
            connection_store,
            self.server_sender.clone(),
            self.options.clone(),
        )
        .await
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

    /// Registers a unique client ID if one doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `connection_store` - Persistence for connection-identity state
    pub async fn regist_id(&self, connection_store: Arc<dyn ConnectionStore>) {
        connection_store.ensure_client_id().await;
    }

    /// Gets a receiver for connection status updates.
    ///
    /// Returns `None` if the receiver has already been taken by a previous call.
    ///
    /// # Returns
    ///
    /// `Some(Receiver)` on the first call, `None` on subsequent calls
    pub async fn get_status_receiver(&self) -> Option<Receiver<SenderStatus>> {
        self.server_sender.get_status_receiver().await
    }

    /// Gets a receiver for incoming messages.
    ///
    /// Returns `None` if the receiver has already been taken by a previous call.
    ///
    /// # Returns
    ///
    /// `Some(Receiver)` on the first call, `None` on subsequent calls
    pub async fn get_handle_message_receiver(&self) -> Option<Receiver<Vec<u8>>> {
        self.server_sender.get_handle_message_receiver().await
    }

    /// Returns a reference to the client's metrics counters.
    pub async fn metrics(&self) -> std::sync::Arc<Metrics> {
        self.server_sender.read().await.metrics.clone()
    }
}

/// Periodic health check for internal network connections.
///
/// Monitors connection health by tracking message timestamps and sends ping
/// messages when needed. Handles reconnection attempts when connection is lost.
///
/// Uses `remove_ip_if_valid_server_ip` which handles clearing stored connection
/// info across all feature flag combinations (native-db/in-memory, bebop/raw).
///
/// # Arguments
///
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options
async fn internal_ping_loop_cheker(
    server_sender: RwServerSender,
    options: ClientOptions,
    cancel_token: CancellationToken,
) {
    let retry_seconds = options.retry_seconds.max(1);
    let use_keep_ip = options.use_keep_ip;
    let max_retry_seconds = retry_seconds * 8;
    let mut current_retry_seconds = retry_seconds;

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                log_debug!("internal_ping_loop_cheker cancelled");
                break;
            }
            _ = tokio::time::sleep(Duration::from_secs(current_retry_seconds)) => {}
        }
        let server_sender_read = server_sender.read().await;

        // Check if connection is dead (no messages received for 4x retry period)
        if server_sender_read.server_received_times > 0
            && server_sender_read.server_received_times + (retry_seconds as i64 * 4)
                < now().timestamp()
        {
            drop(server_sender_read);
            server_sender.send_status(SenderStatus::Disconnected).await;

            // Clear server IP and stored connection info if not keeping it
            if !use_keep_ip {
                server_sender.remove_ip_if_valid_server_ip("").await;
            }

            // Attempt reconnection with exponential backoff
            server_sender.send_status(SenderStatus::Reconnecting).await;
            let (metrics, connection_store) = {
                let guard = server_sender.read().await;
                (guard.metrics.clone(), guard.connection_store.clone())
            };
            metrics.inc_reconnections();
            let server_sender = server_sender.clone();
            let options = options.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    get_internal_connect(None, connection_store, server_sender, options).await
                {
                    log_error!("Internal reconnection failed: {:?}", e);
                }
            });

            // Exponential backoff: double the retry interval (capped at 8x base)
            current_retry_seconds = (current_retry_seconds * 2).min(max_retry_seconds);
        }
        // Send a ping if no messages for 2x retry period
        else if server_sender_read.server_received_times > 0
            && server_sender_read.server_received_times + (retry_seconds as i64 * 2)
                < now().timestamp()
        {
            if options.use_ping {
                log_debug!("Try ping from loop checker");
                let connection_store = server_sender_read.connection_store.clone();
                drop(server_sender_read);
                let id: String = connection_store.get_client_id().await;
                server_sender.send(make_ping_message(&id)).await;
            }
        } else {
            // Connection is alive — reset reconnection backoff
            current_retry_seconds = retry_seconds;
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
async fn outer_ping_loop_cheker(
    server_sender: RwServerSender,
    options: ClientOptions,
    cancel_token: CancellationToken,
) {
    let retry_seconds = options.retry_seconds.max(1);
    let use_keep_ip = options.use_keep_ip;
    let max_retry_seconds = retry_seconds * 8;
    let mut current_retry_seconds = retry_seconds;

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                log_debug!("outer_ping_loop_cheker cancelled");
                break;
            }
            _ = tokio::time::sleep(Duration::from_secs(current_retry_seconds)) => {}
        }
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

            // Attempt reconnection with exponential backoff
            server_sender.send_status(SenderStatus::Reconnecting).await;
            let (metrics, connection_store) = {
                let guard = server_sender.read().await;
                (guard.metrics.clone(), guard.connection_store.clone())
            };
            metrics.inc_reconnections();
            let server_sender = server_sender.clone();
            let options = options.clone();
            tokio::spawn(async move {
                if let Err(e) = get_outer_connect(connection_store, server_sender, options).await {
                    log_error!("External reconnection failed: {:?}", e);
                }
            });

            // Exponential backoff: double the retry interval (capped at 8x base)
            current_retry_seconds = (current_retry_seconds * 2).min(max_retry_seconds);
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
                let connection_store = server_sender_read.connection_store.clone();
                drop(server_sender_read);
                let id: String = connection_store.get_client_id().await;
                server_sender.send(make_ping_message(&id)).await;
            }
        } else {
            // Connection is alive — reset reconnection backoff
            current_retry_seconds = retry_seconds;
        }
        log_debug!("loop server checker finish");
    }
}

/// Initiates a connection to an external server.
///
/// # Arguments
///
/// * `connection_store` - Persistence for connection-identity state
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options
///
/// # Returns
///
/// A Result indicating whether the connection process was initiated successfully
pub async fn get_outer_connect(
    connection_store: Arc<dyn ConnectionStore>,
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

    let server_connect_info = connection_store.get_server_connect_info().await;
    log_debug!("server_connect_info: {:?}", server_connect_info);

    // Cannot connect if no URL is provided and no stored server IP
    if options.url.is_empty() && !server_sender.is_valid_server_ip().await {
        server_sender.send_status(SenderStatus::Disconnected).await;
        return Ok(());
    }

    // Spawn connection task
    server_sender.send_status(SenderStatus::Connecting).await;
    tokio::spawn(wrap_get_outer_websocket(
        connection_store,
        server_sender,
        options,
    ));
    Ok(())
}

/// Initiates a connection to an internal server.
///
/// Handles automatic server discovery when server IP is not provided.
///
/// # Arguments
///
/// * `input` - Optional server connection information
/// * `connection_store` - Persistence for connection-identity state
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options
///
/// # Returns
///
/// A Result indicating whether the connection process was initiated successfully
#[cfg(all(feature = "native-db", feature = "bebop"))]
pub async fn get_internal_connect(
    input: Option<ServerConnectInfo<'_>>,
    connection_store: Arc<dyn ConnectionStore>,
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

    let server_connect_info = connection_store.get_server_connect_info().await;
    log_debug!("server_connect_info: {:?}", server_connect_info);

    // Reserve the port ahead of the first successful connection, if provided
    // and not already present.
    if let (Some(input_ref), None) = (input.as_ref(), server_connect_info.as_ref()) {
        connection_store
            .set_server_connect_info("", input_ref.port)
            .await;
    }

    // Cannot connect if no input or stored connection info
    if input.is_none() && server_connect_info.is_none() {
        server_sender.send_status(SenderStatus::Disconnected).await;
        return Ok(());
    }

    // Determine connection info to use
    let (connect_server_ip, connect_port): (String, String) = match input.as_ref() {
        Some(info) => {
            let server_ip = server_connect_info
                .as_ref()
                .map(|(ip, _)| ip.clone())
                .unwrap_or_default();
            (server_ip, info.port.to_owned())
        }
        None => {
            let Some((server_ip, port)) = server_connect_info else {
                server_sender.send_status(SenderStatus::Disconnected).await;
                return Ok(());
            };
            (server_ip, port)
        }
    };

    // Connect directly to known server IP or, only if explicitly enabled, scan.
    match connect_server_ip.as_str() {
        // No known server IP. In fixed-IP deployments the address is typed in,
        // so we do NOT auto-scan unless `use_scan_discovery` is enabled. The app
        // keeps running and the user can enter the IP or press "search"
        // (`scan_and_connect`).
        "" => {
            if !options.use_scan_discovery {
                server_sender.send_status(SenderStatus::Disconnected).await;
                return Ok(());
            }

            // Single-flight: never run more than one scan at a time. `is_try_connect`
            // can't guard this — it only becomes true once a connection is
            // established — so repeated calls would each start another unbounded
            // scan and leak a subnet's worth of in-flight sockets (port exhaustion).
            {
                let mut guard = server_sender.write().await;
                if guard.is_scanning {
                    return Ok(());
                }
                guard.is_scanning = true;
            }

            // Auto-scan path: needs the local subnet, so the local IP must be
            // resolvable. This is the only branch that depends on `get_ip_address`.
            if get_ip_address().is_empty() {
                server_sender.write().await.is_scanning = false;
                server_sender.send_status(SenderStatus::Disconnected).await;
                return Ok(());
            }

            server_sender.send_status(SenderStatus::Connecting).await;

            // Bounded scan — must not run forever when no server is present.
            let scan_timeout = Duration::from_secs(options.scan_timeout_seconds.max(1));
            let found = ScanManager::new(&connect_port)
                .run_with_timeout(scan_timeout)
                .await;

            // Scan finished (found or timed out) — release the single-flight guard.
            server_sender.write().await.is_scanning = false;

            match found {
                Some((server_ip, ws_stream)) => {
                    // Claim the single-flight guard now, before handing off
                    // to handle_websocket (see scan_and_connect for why).
                    match TryConnectGuard::try_acquire(server_sender.clone()).await {
                        Some(connect_guard) => {
                            tokio::spawn(async move {
                                if let Err(error) = handle_websocket(
                                    connection_store,
                                    server_sender.clone(),
                                    options,
                                    server_ip,
                                    ws_stream,
                                    connect_guard,
                                )
                                .await
                                {
                                    log_error!("Error handling websocket: {:?}", error);
                                }
                            });
                        }
                        None => {
                            log_debug!(
                                "Scan found a server but a connection attempt is already in progress"
                            );
                        }
                    }
                }
                None => {
                    // Timed out with no server — hand control back to the caller's
                    // retry loop instead of holding sockets open.
                    server_sender.send_status(SenderStatus::Disconnected).await;
                }
            }
        }
        // Direct connect to the known fixed server IP. No local-IP / internet
        // dependency — works on an isolated LAN with no route to the outside.
        _server_ip => {
            // Normalized rather than dialed as-is: a stored address without a
            // scheme (a bare IP) makes `connect_async` fail at URL-parse time,
            // silently and without emitting any status. See `to_ws_url`.
            // Because `handle_websocket` persists whatever address it
            // connected with, the normalized form also replaces the malformed
            // record on the first success.
            let url = to_ws_url(_server_ip, &connect_port);
            server_sender.send_status(SenderStatus::Connecting).await;
            tokio::spawn(wrap_get_internal_websocket(
                connection_store,
                server_sender.clone(),
                url,
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
    connection_store: Arc<dyn ConnectionStore>,
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

    // Local network discovery is opt-in (off by default for fixed-IP setups).
    if !options.use_scan_discovery {
        server_sender.send_status(SenderStatus::Disconnected).await;
        return Ok(());
    }

    // Single-flight guard so repeated calls never stack concurrent scans.
    {
        let mut guard = server_sender.write().await;
        if guard.is_scanning {
            return Ok(());
        }
        guard.is_scanning = true;
    }

    // Cannot scan without knowing the local subnet.
    if get_ip_address().is_empty() {
        server_sender.write().await.is_scanning = false;
        server_sender.send_status(SenderStatus::Disconnected).await;
        return Ok(());
    }

    // Use local network discovery, bounded so it cannot run forever.
    server_sender.send_status(SenderStatus::Connecting).await;
    let scan_timeout = Duration::from_secs(options.scan_timeout_seconds.max(1));
    let found = ScanManager::new("9000").run_with_timeout(scan_timeout).await;

    server_sender.write().await.is_scanning = false;

    match found {
        Some((server_ip, ws_stream)) => {
            // Claim the single-flight guard now, before handing off to
            // handle_websocket (see scan_and_connect for why).
            match TryConnectGuard::try_acquire(server_sender.clone()).await {
                Some(connect_guard) => {
                    tokio::spawn(async move {
                        if let Err(error) = handle_websocket(
                            connection_store,
                            server_sender.clone(),
                            options,
                            server_ip,
                            ws_stream,
                            connect_guard,
                        )
                        .await
                        {
                            log_error!("Error handling websocket: {:?}", error);
                        }
                    });
                }
                None => {
                    log_debug!(
                        "Scan found a server but a connection attempt is already in progress"
                    );
                }
            }
        }
        None => {
            server_sender.send_status(SenderStatus::Disconnected).await;
        }
    }

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
            ..Default::default()
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
            ..Default::default()
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
