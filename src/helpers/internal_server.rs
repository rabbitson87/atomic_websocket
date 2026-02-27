//! WebSocket server implementation for atomic_websocket.
//!
//! This module provides the server-side functionality for accepting and managing
//! WebSocket connections, including client tracking, message routing, and automatic
//! ping/pong handling.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::{
    self,
    net::TcpListener,
    sync::mpsc::Receiver,
    time::{timeout, Instant, MissedTickBehavior},
};
use tokio_tungstenite::{tungstenite::Error, WebSocketStream};

#[cfg(feature = "bebop")]
use crate::helpers::common::get_data_schema;
#[cfg(feature = "bebop")]
use crate::schema::{Category, Ping};
use crate::{
    helpers::{
        client_sender::ClientSendersTrait,
        common::{make_disconnect_message, make_pong_message},
    },
    log_debug, log_error,
};
#[cfg(feature = "bebop")]
use bebop::Record;
use futures_util::{stream::SplitStream, SinkExt, StreamExt};
use tokio::sync::mpsc::{self, Sender};
use tokio_tungstenite::{
    accept_async,
    tungstenite::{self, Message},
};

use tokio_util::sync::CancellationToken;

use super::{
    client_sender::ClientSenders,
    metrics::Metrics,
    middleware::{MessageMiddleware, MiddlewareResult},
    types::RwClientSenders,
};

/// WebSocket server implementation for accepting and managing client connections.
///
/// Manages WebSocket client connections and routes messages between clients.
/// Supports graceful shutdown via `shutdown()`.
pub struct AtomicServer {
    /// Collection of connected clients
    pub client_senders: RwClientSenders,

    /// Cancellation token for graceful shutdown
    cancel_token: CancellationToken,
}

/// Configuration options for the WebSocket server.
///
/// Controls aspects of server behavior such as ping handling, buffer sizes,
/// and client timeout intervals.
#[derive(Clone)]
pub struct ServerOptions {
    /// Whether to automatically respond to ping messages with pongs
    pub use_ping: bool,

    /// Category ID to use when proxying ping messages instead of responding directly
    /// A value of -1 disables ping proxying
    pub proxy_ping: i16,

    /// Seconds of client inactivity before the server considers the client disconnected
    pub client_timeout_seconds: u64,

    /// Interval in seconds for checking inactive clients (default: 15)
    pub client_check_interval_secs: u64,

    /// Buffer size for per-connection outgoing message channels (default: 8)
    pub per_connection_buffer_size: usize,

    /// Buffer size for the application message handler channel (default: 1024)
    pub handler_buffer_size: usize,

    /// Maximum size of the spillover buffer for handler messages (default: 1024).
    /// When the handler channel is full, messages are buffered here instead of
    /// blocking. Messages are dropped only when this buffer also reaches its cap.
    pub spillover_buffer_size: usize,

    /// Middleware chain for intercepting WebSocket events.
    /// Middlewares are called in order; if any returns `Stop` on a message, it is dropped.
    pub middlewares: Vec<Arc<dyn MessageMiddleware>>,

    /// Optional TLS configuration for secure WebSocket connections (wss://).
    /// When set, incoming TCP connections are wrapped with TLS before
    /// the WebSocket handshake. Only available with the `rustls` feature.
    #[cfg(feature = "rustls")]
    pub tls_config: Option<Arc<rustls::ServerConfig>>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            use_ping: true,
            proxy_ping: -1,
            client_timeout_seconds: 30,
            client_check_interval_secs: 15,
            per_connection_buffer_size: 8,
            handler_buffer_size: 1024,
            spillover_buffer_size: 1024,
            middlewares: Vec::new(),
            #[cfg(feature = "rustls")]
            tls_config: None,
        }
    }
}

impl AtomicServer {
    /// Creates a new WebSocket server instance.
    ///
    /// Binds to the specified address and starts accepting connections.
    /// Also spawns background tasks for connection handling and client tracking.
    ///
    /// # Arguments
    ///
    /// * `addr` - Address to bind the server to (e.g., "0.0.0.0:9000")
    /// * `option` - Server configuration options
    /// * `client_senders` - Optional existing ClientSenders instance
    ///
    /// # Returns
    ///
    /// A `Result` containing the new AtomicServer instance, or an IO error if binding fails
    pub async fn new(
        addr: &str,
        option: ServerOptions,
        client_senders: Option<RwClientSenders>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(&addr).await?;
        let check_interval = option.client_check_interval_secs;
        let cancel_token = CancellationToken::new();
        let client_senders = match client_senders {
            Some(client_senders) => client_senders,
            None => Arc::new(ClientSenders::new_with_buffer_size(
                option.handler_buffer_size,
                option.spillover_buffer_size,
            )),
        };

        #[cfg(feature = "rustls")]
        let use_tls = option.tls_config.is_some();
        #[cfg(not(feature = "rustls"))]
        let use_tls = false;

        client_senders.set_options(option.clone());

        if use_tls {
            #[cfg(feature = "rustls")]
            if let Some(tls_config) = option.tls_config {
                let acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
                tokio::spawn(handle_accept_tls(
                    listener,
                    client_senders.clone(),
                    cancel_token.clone(),
                    acceptor,
                ));
            }
        } else {
            tokio::spawn(handle_accept(
                listener,
                client_senders.clone(),
                cancel_token.clone(),
            ));
        }

        tokio::spawn(loop_client_checker(
            client_senders.clone(),
            check_interval,
            cancel_token.clone(),
        ));
        Ok(Self {
            client_senders,
            cancel_token,
        })
    }

    /// Gets a receiver for incoming messages from clients.
    ///
    /// # Returns
    ///
    /// A channel receiver for message data along with client identifiers
    pub async fn get_handle_message_receiver(&self) -> Receiver<(Vec<u8>, String)> {
        self.client_senders.get_handle_message_receiver().await
    }

    /// Gracefully shuts down the server.
    ///
    /// Cancels all background tasks (accept loop, client checker).
    /// Existing client connections will be closed when their tasks detect
    /// the cancellation or when the TCP listener is dropped.
    pub async fn shutdown(&self) {
        self.cancel_token.cancel();
    }

    /// Returns a reference to the server's metrics counters.
    pub fn metrics(&self) -> Arc<Metrics> {
        self.client_senders.metrics.clone()
    }
}

/// Periodically checks for and removes inactive clients.
///
/// # Arguments
///
/// * `server_sender` - Shared client senders collection
/// * `check_interval_secs` - Interval in seconds between checks
pub async fn loop_client_checker(
    server_sender: RwClientSenders,
    check_interval_secs: u64,
    cancel_token: CancellationToken,
) {
    let secs = check_interval_secs.max(1);
    let mut interval = tokio::time::interval_at(
        Instant::now() + Duration::from_secs(secs),
        Duration::from_secs(secs),
    );
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                log_debug!("Client checker shutting down");
                break;
            }
            _ = interval.tick() => {
                server_sender.check_client_send_time();
                log_debug!("loop client cheker finish");
            }
        }
    }
}

/// Handles accepting new WebSocket connections.
///
/// Listens for incoming TCP connections and spawns a new task for each one.
///
/// # Arguments
///
/// * `listener` - TCP listener for accepting connections
/// * `client_senders` - Shared client senders collection
pub async fn handle_accept(
    listener: TcpListener,
    client_senders: RwClientSenders,
    cancel_token: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                log_debug!("Accept loop shutting down");
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        let peer = match stream.peer_addr() {
                            Ok(addr) => addr,
                            Err(e) => {
                                log_error!("Failed to get peer address: {:?}", e);
                                continue;
                            }
                        };
                        log_debug!("Peer address: {}", peer);
                        tokio::spawn(accept_connection(client_senders.clone(), peer, stream));
                    }
                    Err(e) => {
                        log_error!("Error accepting connection: {:?}", e);
                    }
                }
            }
        }
    }
}

/// Handles accepting new WebSocket connections over TLS.
///
/// Performs the TLS handshake before handing off to the WebSocket handler.
#[cfg(feature = "rustls")]
pub async fn handle_accept_tls(
    listener: TcpListener,
    client_senders: RwClientSenders,
    cancel_token: CancellationToken,
    tls_acceptor: tokio_rustls::TlsAcceptor,
) {
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                log_debug!("TLS accept loop shutting down");
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        let peer = match stream.peer_addr() {
                            Ok(addr) => addr,
                            Err(e) => {
                                log_error!("Failed to get peer address: {:?}", e);
                                continue;
                            }
                        };
                        log_debug!("Peer address (TLS): {}", peer);
                        let acceptor = tls_acceptor.clone();
                        let cs = client_senders.clone();
                        tokio::spawn(async move {
                            match acceptor.accept(stream).await {
                                Ok(tls_stream) => {
                                    accept_connection(cs, peer, tls_stream).await;
                                }
                                Err(e) => {
                                    log_error!("TLS handshake failed for {}: {:?}", peer, e);
                                }
                            }
                        });
                    }
                    Err(e) => {
                        log_error!("Error accepting connection: {:?}", e);
                    }
                }
            }
        }
    }
}

/// Handles the WebSocket upgrade process for a new connection.
///
/// # Arguments
///
/// * `client_senders` - Shared client senders collection
/// * `peer` - Socket address of the connecting client
/// * `stream` - TCP stream for the connection
pub async fn accept_connection<S>(client_senders: RwClientSenders, peer: SocketAddr, stream: S)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    if let Err(e) = handle_connection(client_senders, peer, stream).await {
        match e {
            Error::ConnectionClosed | Error::Protocol(_) | Error::Utf8(_) => (),
            err => log_error!("Error processing connection: {}", err),
        }
    }
}

/// Handles an established WebSocket connection.
///
/// Sets up bidirectional message handling for the client connection.
///
/// **Note:** The peer is registered in `client_senders` (via the first
/// message handshake) before middleware `on_connect` is evaluated. If a
/// middleware rejects the connection, a disconnect message is sent
/// immediately, which cleans up the entry. There is a brief window
/// where `send_all` could reach a rejected peer.
///
/// # Arguments
///
/// * `client_senders` - Shared client senders collection
/// * `peer` - Socket address of the client
/// * `stream` - TCP stream for the connection
/// * `option` - Server configuration options
///
/// # Returns
///
/// A Result indicating whether the connection handling completed successfully
#[cfg(feature = "bebop")]
pub async fn handle_connection<S>(
    client_senders: RwClientSenders,
    peer: SocketAddr,
    stream: S,
) -> tungstenite::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    match accept_async(stream).await {
        Ok(ws_stream) => {
            log_debug!("New WebSocket connection: {}", peer);
            let (mut ostream, mut istream) = ws_stream.split();

            let options = client_senders.options();
            let buf_size = options.per_connection_buffer_size;
            let (sx, mut rx) = mpsc::channel(buf_size);
            tokio::spawn(async move {
                let use_ping = options.use_ping;
                let middlewares = options.middlewares;
                let id =
                    get_id_from_first_message(&mut istream, client_senders.clone(), sx.clone())
                        .await;

                if let Some(id) = id {
                    // Check on_connect middlewares
                    let mut connected = true;
                    for mw in &middlewares {
                        if !mw.on_connect(&id).await {
                            connected = false;
                            break;
                        }
                    }

                    if connected {
                        // Handle incoming messages
                        while let Some(Ok(message)) = istream.next().await {
                            let value = message.into_data();
                            let data = match get_data_schema(&value) {
                                Ok(data) => data,
                                Err(e) => {
                                    log_error!("Error getting data schema: {:?}", e);
                                    continue;
                                }
                            };

                            // Handle ping messages
                            if data.category == Category::Ping as u16 && use_ping {
                                if let Ok(data) = Ping::deserialize(&data.datas) {
                                    client_senders.send(data.peer, make_pong_message()).await;
                                    continue;
                                }
                            }

                            // Handle disconnect messages
                            if data.category == Category::Disconnect as u16 {
                                break;
                            }

                            // Middleware on_message check
                            let mut should_forward = true;
                            for mw in &middlewares {
                                if mw.on_message(&id, &value).await == MiddlewareResult::Stop {
                                    should_forward = false;
                                    break;
                                }
                            }

                            // Forward to application handler if not stopped by middleware
                            if should_forward {
                                client_senders.send_handle_message(data, &id).await;
                            }
                        }

                        // Notify middlewares of disconnect
                        for mw in &middlewares {
                            mw.on_disconnect(&id).await;
                        }
                    }
                }
                // Always send disconnect when reader exits (stream closed or explicit disconnect)
                let _ = sx.send(make_disconnect_message(&peer.to_string())).await;
            });

            // Handle outgoing messages
            while let Some(message) = rx.recv().await {
                ostream.send(message.clone()).await?;
                let data = message.into_data();
                let data = match get_data_schema(&data) {
                    Ok(data) => data,
                    Err(e) => {
                        log_error!("Error getting data schema: {:?}", e);
                        rx.close();
                        break;
                    }
                };
                log_debug!("Server sending message: {:?}", data);
                if data.category == Category::Disconnect as u16 {
                    rx.close();
                    break;
                }
            }
            log_debug!("client: {} disconnected", peer);
            let _ = timeout(Duration::from_secs(1), ostream.flush()).await;
        }
        Err(e) => {
            log_debug!("Error accepting WebSocket connection: {:?}", e);
        }
    }

    Ok(())
}

/// Handles an established WebSocket connection (raw bytes version).
///
/// **Note:** The peer is registered in `client_senders` before middleware
/// `on_connect` is evaluated. If a middleware rejects the connection, a
/// disconnect message is sent immediately, which cleans up the entry.
/// There is a brief window where `send_all` could reach a rejected peer.
#[cfg(not(feature = "bebop"))]
pub async fn handle_connection<S>(
    client_senders: RwClientSenders,
    peer: SocketAddr,
    stream: S,
) -> tungstenite::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    match accept_async(stream).await {
        Ok(ws_stream) => {
            log_debug!("New WebSocket connection: {}", peer);
            let (mut ostream, mut istream) = ws_stream.split();

            let options = client_senders.options();
            let buf_size = options.per_connection_buffer_size;
            let (sx, mut rx) = mpsc::channel(buf_size);
            let peer_str = peer.to_string();
            client_senders.add(&peer_str, sx.clone()).await;

            tokio::spawn(async move {
                let middlewares = options.middlewares;

                // Check on_connect middlewares
                let mut connected = true;
                for mw in &middlewares {
                    if !mw.on_connect(&peer_str).await {
                        connected = false;
                        break;
                    }
                }

                if connected {
                    // Handle incoming messages - pass raw bytes
                    while let Some(Ok(message)) = istream.next().await {
                        let value = message.into_data();

                        // Middleware on_message check
                        let mut should_forward = true;
                        for mw in &middlewares {
                            if mw.on_message(&peer_str, &value).await == MiddlewareResult::Stop {
                                should_forward = false;
                                break;
                            }
                        }

                        if should_forward {
                            client_senders
                                .send_handle_message(value.to_vec(), &peer_str)
                                .await;
                        }
                    }

                    // Notify middlewares of disconnect
                    for mw in &middlewares {
                        mw.on_disconnect(&peer_str).await;
                    }
                }

                let _ = sx.send(make_disconnect_message(&peer_str)).await;
            });

            // Handle outgoing messages
            while let Some(message) = rx.recv().await {
                ostream.send(message).await?;
            }
            log_debug!("client: {} disconnected", peer);
            let _ = timeout(Duration::from_secs(1), ostream.flush()).await;
        }
        Err(e) => {
            log_debug!("Error accepting WebSocket connection: {:?}", e);
        }
    }

    Ok(())
}

/// Extracts client ID from the first message and sets up the connection.
///
/// WebSocket clients are expected to send a Ping message as their first
/// communication, containing their client identifier.
///
/// # Arguments
///
/// * `istream` - Stream of incoming WebSocket messages
/// * `client_senders` - Shared client senders collection
/// * `sx` - Sender for outgoing messages to this client
/// * `options` - Server configuration options
///
/// # Returns
///
/// Some(client_id) if identification was successful, None otherwise
#[cfg(feature = "bebop")]
async fn get_id_from_first_message<S>(
    istream: &mut SplitStream<WebSocketStream<S>>,
    client_senders: RwClientSenders,
    sx: Sender<Message>,
) -> Option<String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut _id: Option<String> = None;
    if let Some(Ok(message)) = istream.next().await {
        log_debug!("receive first message from client: {:?}", message);
        let value = message.into_data();
        let mut data = match get_data_schema(&value) {
            Ok(data) => data,
            Err(e) => {
                log_error!("Error getting data schema: {:?}", e);
                return None;
            }
        };
        let options = client_senders.options();

        // Check if the first message is a ping
        if data.category == Category::Ping as u16 {
            log_debug!("receive ping from client: {:?}", data);
            if let Ok(ping) = Ping::deserialize(&data.datas) {
                let peer_id: String = ping.peer.into();
                _id = Some(peer_id.clone());
                client_senders.add(&peer_id, sx).await;

                // Either respond with a pong or proxy the ping
                if options.use_ping {
                    client_senders.send(&peer_id, make_pong_message()).await;
                } else {
                    // Optionally change the category when proxying
                    if options.proxy_ping > 0 {
                        data.category = options.proxy_ping as u16;
                    }
                    client_senders.send_handle_message(data, &peer_id).await;
                }
            }
        } else if options.proxy_ping > 0 && data.category == options.proxy_ping as u16 {
            if let Ok(ping) = Ping::deserialize(&data.datas) {
                let peer_id: String = ping.peer.into();
                _id = Some(peer_id.clone());
                client_senders.add(&peer_id, sx).await;

                // Optionally change the category when proxying
                data.category = options.proxy_ping as u16;
                client_senders.send_handle_message(data, &peer_id).await;
            }
        }
    }
    _id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_options_default() {
        let options = ServerOptions::default();
        assert!(options.use_ping);
        assert_eq!(options.proxy_ping, -1);
    }

    #[test]
    fn test_server_options_custom() {
        let options = ServerOptions {
            use_ping: false,
            proxy_ping: 100,
            ..Default::default()
        };
        assert!(!options.use_ping);
        assert_eq!(options.proxy_ping, 100);
    }

    #[test]
    fn test_server_options_clone() {
        let options = ServerOptions {
            use_ping: false,
            proxy_ping: 50,
            ..Default::default()
        };
        let cloned = options.clone();
        assert!(!cloned.use_ping);
        assert_eq!(cloned.proxy_ping, 50);
    }

    #[test]
    fn test_server_options_proxy_ping_disabled() {
        let options = ServerOptions {
            use_ping: true,
            proxy_ping: -1,
            ..Default::default()
        };
        assert!(options.proxy_ping < 0);
    }

    #[test]
    fn test_server_options_proxy_ping_enabled() {
        let options = ServerOptions {
            use_ping: false,
            proxy_ping: 200,
            ..Default::default()
        };
        assert!(options.proxy_ping > 0);
        assert_eq!(options.proxy_ping, 200);
    }
}
