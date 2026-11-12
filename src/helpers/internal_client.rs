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
use crate::helpers::get_internal_websocket::{
    handle_websocket, TryConnectGuard, FINAL_FLUSH, STOP_NOTICE,
};
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

    /// Server URL for external connections.
    ///
    /// **The scheme here is what selects TLS.** Give `ws://` for plaintext or
    /// `wss://` for TLS and it is used as written. A URL with no scheme is
    /// filled in from the build: `wss://` when the `rustls` feature is on,
    /// `ws://` when it is not.
    ///
    /// Be explicit. A scheme-less address that works in one build silently
    /// changes protocol in the other, and the failure that follows is a TLS
    /// handshake error against a plaintext server, which reads like a
    /// certificate problem rather than a URL problem.
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
    pub async fn scan_and_connect(
        &self,
        port: &str,
        connection_store: Arc<dyn ConnectionStore>,
    ) -> bool {
        // Respect the same duplicate-connection guard as the normal path.
        if !self.server_sender.is_need_connect().await {
            return false;
        }
        // Already connected — nothing to do.
        if self.server_sender.is_valid_server_ip().await {
            self.server_sender
                .send_status(SenderStatus::Connected)
                .await;
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
            self.server_sender
                .send_status(SenderStatus::Disconnected)
                .await;
            return false;
        }

        self.server_sender
            .send_status(SenderStatus::Connecting)
            .await;

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
                self.server_sender
                    .send_status(SenderStatus::Disconnected)
                    .await;
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
        get_outer_connect(
            connection_store,
            self.server_sender.clone(),
            self.options.clone(),
        )
        .await
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
        // Judged and taken down under the one lock: a connection registered
        // between a verdict and the drop would otherwise be the one dropped.
        //
        // The connection comes down either way. Left standing it goes on
        // holding the single-flight flag, and every reconnect below would be
        // turned away by it. What `use_keep_ip` decides is whether the
        // address goes with it — and, for a client that cannot tell a quiet
        // link from a dead one, whether silence is acted on at all (see
        // `silence_is_evidence`).
        let (silence, connection_store) = {
            let mut guard = server_sender.write().await;
            let silence = judge_silence(
                guard.server_received_times,
                guard.connected_at,
                retry_seconds as i64,
                now().timestamp(),
            );
            if silence == Silence::Dead {
                if !use_keep_ip {
                    guard.remove_ip();
                } else if silence_is_evidence(&options) {
                    guard.drop_connection();
                }
            }
            (silence, guard.connection_store.clone())
        };

        match silence {
            Silence::Dead => {
                server_sender.send_status(SenderStatus::Disconnected).await;

                // The stored half of forgetting the address.
                if !use_keep_ip {
                    connection_store.clear_server_ip("").await;
                }

                // Attempt reconnection with exponential backoff
                server_sender.send_status(SenderStatus::Reconnecting).await;
                let metrics = server_sender.read().await.metrics.clone();
                metrics.inc_reconnections();
                let server_sender = server_sender.clone();
                let options = options.clone();
                tokio::spawn(async move {
                    wait_for_teardown(&server_sender).await;
                    if let Err(e) =
                        get_internal_connect(None, connection_store, server_sender, options).await
                    {
                        log_error!("Internal reconnection failed: {:?}", e);
                    }
                });

                // Exponential backoff: double the retry interval (capped at 8x base)
                current_retry_seconds = (current_retry_seconds * 2).min(max_retry_seconds);
            }
            Silence::Quiet => {
                if options.use_ping {
                    log_debug!("Try ping from loop checker");
                    let id: String = connection_store.get_client_id().await;
                    // With a deadline. A connection whose writer is parked on
                    // a socket that has stopped taking writes has a full
                    // queue, and a ping that waited for room in it would
                    // keep this loop from ever reaching "dead".
                    let _ = tokio::time::timeout(
                        Duration::from_secs(retry_seconds),
                        server_sender.send(make_ping_message(&id)),
                    )
                    .await;
                }
            }
            // A connection that has been made and not heard from yet: too
            // young to call dead, and no reason to forget how often the ones
            // before it failed.
            Silence::Unproven => {}
            Silence::Alive => {
                // Connection is alive — reset reconnection backoff
                current_retry_seconds = retry_seconds;
            }
        }
        log_debug!("loop server checker finish");
    }
}

/// What the checker makes of how long a connection has been quiet.
#[derive(Debug, PartialEq, Eq)]
enum Silence {
    /// Heard from recently, or never connected at all.
    Alive,
    /// Made since the last message arrived, and not heard from yet.
    Unproven,
    /// Two retry periods without a message: worth a ping.
    Quiet,
    /// Four: take it down and dial again.
    Dead,
}

/// Judges a connection's silence at `now`.
///
/// `last_received` is the last message from the server on *any* connection;
/// `connected_at` is when the current one was registered. The silence is
/// counted from the later of the two. Without that, a connection made a
/// moment ago would be judged by the previous one's last message — dead on
/// arrival, and taken down before its first pong.
///
/// A client that has never heard from a server has no connection to call
/// dead, however long it has been.
///
/// A `connected_at` later than `now` is not believed. It means the clock was
/// set back after the connection was registered, and unlike the receive
/// stamp nothing ever refreshes it: taken at its word it would keep a dead
/// connection "just made" until the clock caught up with it.
fn judge_silence(last_received: i64, connected_at: i64, retry_seconds: i64, now: i64) -> Silence {
    if last_received <= 0 {
        return Silence::Alive;
    }
    let connected_at = if connected_at > now { 0 } else { connected_at };
    let quiet_since = last_received.max(connected_at);
    if quiet_since + retry_seconds * 4 < now {
        Silence::Dead
    } else if quiet_since + retry_seconds * 2 < now {
        Silence::Quiet
    } else if connected_at > last_received {
        Silence::Unproven
    } else {
        Silence::Alive
    }
}

/// Whether a silent link says anything about the link.
///
/// It does for a client that pings and is answered: a link that returns no
/// pong is not carrying traffic. A client that does not ping — `use_ping`
/// off, or a build without `bebop`, which has no pong to wait for — hears
/// nothing on a healthy link that simply has nothing to say. Taking such a
/// connection down on silence alone would recycle a working link every few
/// retry periods, so for a client that keeps its address it is left as it
/// was before 0.9.8: reported, not touched.
fn silence_is_evidence(options: &ClientOptions) -> bool {
    cfg!(feature = "bebop") && options.use_ping
}

/// How long a reconnect waits for the connection it replaces to let go.
///
/// The checker drops a dead connection and spawns the dial in the same
/// breath, but the connection needs a moment to wind down — a write that was
/// in flight, what is left in its queue with the goodbye, its final flush,
/// and the moment it gives the server to hang up first, each with its own
/// deadline in `handle_websocket` — before it releases the single-flight
/// flag. A dial that arrives first is turned
/// away, and the next one is a whole backoff step later. Not an option: it
/// is those deadlines and a little room, not anything a deployment chooses.
const TEARDOWN_WAIT: Duration = STOP_NOTICE
    .saturating_add(STOP_NOTICE)
    .saturating_add(FINAL_FLUSH)
    .saturating_add(STOP_NOTICE)
    .saturating_add(Duration::from_millis(250));

/// Waits until no connection attempt holds the single-flight flag, or
/// [`TEARDOWN_WAIT`] has passed.
async fn wait_for_teardown(server_sender: &RwServerSender) {
    let deadline = tokio::time::Instant::now() + TEARDOWN_WAIT;
    while !server_sender.is_need_connect().await && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
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
        // See `internal_ping_loop_cheker`: judged and taken down under the
        // one lock; `use_keep_ip` decides about the address.
        let (silence, connection_store) = {
            let mut guard = server_sender.write().await;
            let silence = judge_silence(
                guard.server_received_times,
                guard.connected_at,
                retry_seconds as i64,
                now().timestamp(),
            );
            if silence == Silence::Dead {
                if !use_keep_ip {
                    guard.remove_ip();
                } else if silence_is_evidence(&options) {
                    guard.drop_connection();
                }
            }
            (silence, guard.connection_store.clone())
        };

        match silence {
            Silence::Dead => {
                server_sender.send_status(SenderStatus::Disconnected).await;

                // Attempt reconnection with exponential backoff
                server_sender.send_status(SenderStatus::Reconnecting).await;
                let metrics = server_sender.read().await.metrics.clone();
                metrics.inc_reconnections();
                let server_sender = server_sender.clone();
                let options = options.clone();
                tokio::spawn(async move {
                    wait_for_teardown(&server_sender).await;
                    if let Err(e) =
                        get_outer_connect(connection_store, server_sender, options).await
                    {
                        log_error!("External reconnection failed: {:?}", e);
                    }
                });

                // Exponential backoff: double the retry interval (capped at 8x base)
                current_retry_seconds = (current_retry_seconds * 2).min(max_retry_seconds);
            }
            Silence::Quiet => {
                if options.use_ping {
                    log_debug!("Try ping from loop checker");
                    let id: String = connection_store.get_client_id().await;
                    // With a deadline. A connection whose writer is parked on
                    // a socket that has stopped taking writes has a full
                    // queue, and a ping that waited for room in it would
                    // keep this loop from ever reaching "dead".
                    let _ = tokio::time::timeout(
                        Duration::from_secs(retry_seconds),
                        server_sender.send(make_ping_message(&id)),
                    )
                    .await;
                }
            }
            // A connection that has been made and not heard from yet: too
            // young to call dead, and no reason to forget how often the ones
            // before it failed.
            Silence::Unproven => {}
            Silence::Alive => {
                // Connection is alive — reset reconnection backoff
                current_retry_seconds = retry_seconds;
            }
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

            // A kept address that goes on failing may simply be the server
            // taking its time — or the server may have moved. Discovery, if
            // it is on, is started beside the dial rather than in its place:
            // the dial is what brings a restarted server back within one
            // attempt, and a scan takes most of a minute.
            let failed_dials = server_sender.read().await.failed_dials;
            if wants_discovery(&options, failed_dials) {
                tokio::spawn(discover_beside_dials(
                    connect_port.clone(),
                    connection_store.clone(),
                    server_sender.clone(),
                    options.clone(),
                ));
            }

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

/// How many dials of a kept address fail in a row before discovery is
/// brought in beside them.
///
/// One or two is a server restarting. It is a count of dials, not a length
/// of time: how long three take depends on who is asking for connections —
/// seconds for an application that redials every few seconds, minutes for
/// the checker alone at its default pace. Not an option: the cost of being
/// wrong either way is small — a scan a little early, or a little late —
/// and it is paid only by clients with both `use_keep_ip` and
/// `use_scan_discovery`.
#[cfg(all(feature = "native-db", feature = "bebop"))]
const DISCOVERY_AFTER_FAILED_DIALS: u32 = 3;

/// Whether a client that keeps its address should also look for the server
/// elsewhere. Only one that was asked to discover at all.
#[cfg(all(feature = "native-db", feature = "bebop"))]
fn wants_discovery(options: &ClientOptions, failed_dials: u32) -> bool {
    options.use_keep_ip
        && options.use_scan_discovery
        && failed_dials >= DISCOVERY_AFTER_FAILED_DIALS
}

/// Scans the local subnet while the kept address goes on being dialed, and
/// hands over the connection if a server answers.
///
/// Reports no status: the dials are what the application is watching, and a
/// scan that finds nothing has changed nothing. A server found here replaces
/// the kept address the usual way, when its connection is registered.
#[cfg(all(feature = "native-db", feature = "bebop"))]
async fn discover_beside_dials(
    connect_port: String,
    connection_store: Arc<dyn ConnectionStore>,
    server_sender: RwServerSender,
    options: ClientOptions,
) {
    // Cheap exit first: with a scan already running, every request that
    // finds the count at the threshold would otherwise build a scan list
    // (a socket call and 254 strings) just to be turned away below.
    {
        let guard = server_sender.read().await;
        if guard.is_scanning || !wants_discovery(&options, guard.failed_dials) {
            return;
        }
    }

    // A scan builds its list from this machine's own IPv4 address; without
    // one there is nothing to scan (and `ScanManager` would index past the
    // end of what it was given). The manager is built here, before the
    // single-flight flag goes up: it reads the address again, and nothing
    // that can fail may sit between raising that flag and lowering it.
    if get_ip_address().split('.').count() != 4 {
        return;
    }
    let mut manager = ScanManager::new(&connect_port);

    // Same single-flight as every other scan, and the request checked again
    // under the lock: a connection made since it was sent has put the count
    // back to zero, and there is nothing to look for. The count starts over
    // here, at the point a scan really starts.
    let connected_before = {
        let mut guard = server_sender.write().await;
        if guard.is_scanning || !wants_discovery(&options, guard.failed_dials) {
            return;
        }
        guard.is_scanning = true;
        guard.failed_dials = 0;
        guard.connected_at
    };

    // The scan stops as soon as a connection is made by other means: the
    // kept address answered after all. Without that it would run on, open a
    // second connection to the same server, and have to throw it away.
    let scan_timeout = Duration::from_secs(options.scan_timeout_seconds.max(1));
    let found = {
        let scan = manager.run_with_timeout(scan_timeout);
        tokio::pin!(scan);
        tokio::select! {
            found = &mut scan => found,
            _ = connection_made_since(&server_sender, connected_before) => None,
        }
    };
    manager.stop();
    // With everything else it opened: only the stream that was found is
    // wanted, and it lives as long as the connection made from it.
    drop(manager);

    let found = {
        let mut guard = server_sender.write().await;
        guard.is_scanning = false;
        if guard.connected_at != connected_before {
            // Connected meanwhile; whatever the scan found is not needed.
            return;
        }
        if found.is_none() {
            // Nothing found. Three more failed dials before the next scan,
            // rather than one scan straight after another.
            guard.failed_dials = 0;
        }
        found
    };
    let Some((server_ip, ws_stream)) = found else {
        return;
    };

    // A dial of the kept address may be holding the single-flight flag, for
    // as long as its timeout if that address no longer answers. Wait that
    // long, and a second more, rather than throw away the server that was
    // just found. If the flag is still held after that, the dial got through
    // and there is nothing to replace: the found connection is dropped.
    let patience = Duration::from_secs(options.connect_timeout_seconds + 1);
    let deadline = tokio::time::Instant::now() + patience;
    let connect_guard = loop {
        if let Some(guard) = TryConnectGuard::try_acquire(server_sender.clone()).await {
            break guard;
        }
        if tokio::time::Instant::now() >= deadline {
            log_debug!("Scan found a server but the kept address connected first");
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    if let Err(error) = handle_websocket(
        connection_store,
        server_sender,
        options,
        server_ip,
        ws_stream,
        connect_guard,
    )
    .await
    {
        log_error!("Error handling websocket: {:?}", error);
    }
}

/// Resolves once a connection has been registered that was not there at
/// `connected_before`.
#[cfg(all(feature = "native-db", feature = "bebop"))]
async fn connection_made_since(server_sender: &RwServerSender, connected_before: i64) {
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if server_sender.read().await.connected_at != connected_before {
            return;
        }
    }
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
    let found = ScanManager::new("9000")
        .run_with_timeout(scan_timeout)
        .await;

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

    #[test]
    fn test_client_options_clone() {
        let options = ClientOptions {
            use_ping: false,
            url: "ws://example.com:9000".to_string(),
            retry_seconds: 60,
            use_keep_ip: true,
            connect_timeout_seconds: 10,
            atomic_websocket_type: AtomicWebsocketType::External,
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
            ..Default::default()
        };

        assert!(!options.use_ping);
        assert_eq!(options.url, "192.168.1.100:9000");
        assert_eq!(options.retry_seconds, 5);
        assert!(options.use_keep_ip);
        assert_eq!(options.connect_timeout_seconds, 1);
    }

    const RETRY: i64 = 5;
    const NOW: i64 = 1_700_000_000;

    #[test]
    fn a_client_that_never_connected_is_not_called_dead() {
        assert_eq!(judge_silence(0, 0, RETRY, NOW), Silence::Alive);
    }

    #[test]
    fn silence_is_pinged_after_two_periods_and_dead_after_four() {
        // Exactly on the mark still counts as the gentler of the two.
        assert_eq!(judge_silence(NOW - 2 * RETRY, 0, RETRY, NOW), Silence::Alive);
        assert_eq!(judge_silence(NOW - 2 * RETRY - 1, 0, RETRY, NOW), Silence::Quiet);
        assert_eq!(judge_silence(NOW - 4 * RETRY, 0, RETRY, NOW), Silence::Quiet);
        assert_eq!(judge_silence(NOW - 4 * RETRY - 1, 0, RETRY, NOW), Silence::Dead);
    }

    /// The reconnect after an hour of silence: the new connection is up and
    /// its first pong has not arrived yet. The last message on record is the
    /// old connection's, an hour ago.
    #[test]
    fn a_connection_just_made_is_not_judged_by_the_one_before_it() {
        let an_hour_ago = NOW - 3600;
        assert_eq!(judge_silence(an_hour_ago, NOW, RETRY, NOW), Silence::Unproven);
        assert_eq!(
            judge_silence(an_hour_ago, NOW - 2 * RETRY, RETRY, NOW),
            Silence::Unproven
        );
    }

    /// Not dead — but not "alive" either, which is what resets the backoff
    /// between redials. A server that accepts every connection and answers
    /// none must still be dialed less and less often.
    #[test]
    fn a_connection_not_heard_from_yet_does_not_count_as_alive() {
        let an_hour_ago = NOW - 3600;
        assert_ne!(judge_silence(an_hour_ago, NOW - 1, RETRY, NOW), Silence::Alive);
        // Heard from since it was made: alive.
        assert_eq!(judge_silence(NOW, NOW - 1, RETRY, NOW), Silence::Alive);
        assert_eq!(judge_silence(NOW - 1, NOW - 1, RETRY, NOW), Silence::Alive);
    }

    /// The clock was set back after the connection was made. Its stamp is in
    /// the future and nothing refreshes it; believed, it would call the
    /// connection young until the clock caught up.
    #[test]
    fn a_connection_stamped_in_the_future_is_judged_by_its_messages() {
        let tomorrow = NOW + 86_400;
        assert_eq!(
            judge_silence(NOW - 4 * RETRY - 1, tomorrow, RETRY, NOW),
            Silence::Dead
        );
        assert_eq!(
            judge_silence(NOW - 2 * RETRY - 1, tomorrow, RETRY, NOW),
            Silence::Quiet
        );
        assert_eq!(judge_silence(NOW - 1, tomorrow, RETRY, NOW), Silence::Alive);
    }

    /// Silence means something only to a client that pings and is answered.
    #[test]
    fn silence_is_evidence_only_for_a_client_that_pings() {
        let pinging = ClientOptions {
            use_ping: true,
            ..Default::default()
        };
        let quiet = ClientOptions {
            use_ping: false,
            ..Default::default()
        };
        // Without bebop there is no pong to wait for, ping or not.
        assert_eq!(silence_is_evidence(&pinging), cfg!(feature = "bebop"));
        assert!(!silence_is_evidence(&quiet));
    }

    /// It gets the same patience as any other, and no more.
    #[test]
    fn a_new_connection_that_never_answers_is_dead_in_its_turn() {
        let an_hour_ago = NOW - 3600;
        assert_eq!(
            judge_silence(an_hour_ago, NOW - 2 * RETRY - 1, RETRY, NOW),
            Silence::Quiet
        );
        assert_eq!(
            judge_silence(an_hour_ago, NOW - 4 * RETRY - 1, RETRY, NOW),
            Silence::Dead
        );
    }

    /// A message since the connection was made is what counts once there is
    /// one.
    #[test]
    fn the_later_of_the_two_stamps_is_the_one_that_counts() {
        assert_eq!(
            judge_silence(NOW - 1, NOW - 3600, RETRY, NOW),
            Silence::Alive
        );
        assert_eq!(
            judge_silence(NOW - 4 * RETRY - 1, NOW - 3600, RETRY, NOW),
            Silence::Dead
        );
    }

    #[cfg(all(feature = "native-db", feature = "bebop"))]
    fn keeping(use_scan_discovery: bool) -> ClientOptions {
        ClientOptions {
            use_keep_ip: true,
            use_scan_discovery,
            ..Default::default()
        }
    }

    /// One or two failed dials is a server restarting.
    #[cfg(all(feature = "native-db", feature = "bebop"))]
    #[test]
    fn discovery_waits_for_the_kept_address_to_fail_three_times() {
        let options = keeping(true);
        assert!(!wants_discovery(&options, 0));
        assert!(!wants_discovery(&options, 2));
        assert!(wants_discovery(&options, 3));
        assert!(wants_discovery(&options, 30));
    }

    /// A tablet with a typed-in address never scans by itself, however long
    /// the server is away.
    #[cfg(all(feature = "native-db", feature = "bebop"))]
    #[test]
    fn a_client_that_was_not_asked_to_discover_never_does() {
        assert!(!wants_discovery(&keeping(false), 30));
    }

    /// Without `use_keep_ip` there is no kept address to give up on: the
    /// first failed dial already cleared it, and discovery takes its usual
    /// path.
    #[cfg(all(feature = "native-db", feature = "bebop"))]
    #[test]
    fn discovery_beside_dials_is_only_for_a_kept_address() {
        let options = ClientOptions {
            use_keep_ip: false,
            use_scan_discovery: true,
            ..Default::default()
        };
        assert!(!wants_discovery(&options, 30));
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
