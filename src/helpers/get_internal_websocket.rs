//! Internal WebSocket connection handling for atomic_websocket clients.
//!
//! This module provides functionality for establishing and maintaining WebSocket
//! connections to servers, including connection setup, message handling, and
//! automatic reconnection logic.

use std::sync::{atomic::AtomicBool, Arc};

use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{sleep, timeout},
};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

#[cfg(feature = "bebop")]
use crate::generated::schema::Category;
#[cfg(feature = "bebop")]
use crate::helpers::common::get_data_schema;
use crate::{
    helpers::{
        common::{make_disconnect_message, make_ping_message},
        connection_store::ConnectionStore,
        server_sender::{SenderStatus, ServerSenderTrait},
        traits::atomic::FlagAtomic,
    },
    log_debug, log_error, Settings,
};

use super::{
    internal_client::ClientOptions,
    types::{save_key, RwServerSender, DB},
};

/// Guard that owns the `is_try_connect` single-flight lock for the lifetime of
/// one connection attempt, and resets it to `false` on drop.
///
/// Must be acquired via [`TryConnectGuard::try_acquire`] *before* the
/// handshake (`connect_async`) starts, not after it succeeds — otherwise a
/// concurrent reconnect trigger (the periodic ping-loop checker, an app-level
/// `get_internal_connect`/`get_outer_connect` call, or `ServerSender::send`'s
/// exhausted-retry reconnect) can slip through and start its own connection
/// attempt during the handshake window.
///
/// Also ensures the flag is always reset even if the connection handler
/// panics or exits early, preventing permanent connection lockout.
pub(crate) struct TryConnectGuard {
    server_sender: RwServerSender,
    disarmed: bool,
}

impl TryConnectGuard {
    /// Atomically claims the single-flight guard for a new connection
    /// attempt: `false` -> `true` under one write lock. Returns `None` if a
    /// connection attempt is already in progress.
    pub(crate) async fn try_acquire(server_sender: RwServerSender) -> Option<Self> {
        let mut guard = server_sender.write().await;
        if guard.is_try_connect {
            return None;
        }
        guard.is_try_connect = true;
        drop(guard);
        Some(Self {
            server_sender,
            disarmed: false,
        })
    }

    /// Disarm the guard to prevent redundant reset on normal exit.
    pub(crate) fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for TryConnectGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            let server_sender = self.server_sender.clone();
            tokio::spawn(async move {
                server_sender.write().await.is_try_connect = false;
            });
        }
    }
}

/// Wrapper function for establishing an internal WebSocket connection.
///
/// Handles errors from the connection attempt and logs them appropriately.
///
/// # Arguments
///
/// * `connection_store` - Persistence for connection-identity state
/// * `server_sender` - Server sender for message handling
/// * `server_ip` - Server address to connect to
/// * `options` - Client connection options
///
/// # Returns
///
/// `true` if the connection was successfully established, `false` otherwise
pub async fn wrap_get_internal_websocket(
    connection_store: Arc<dyn ConnectionStore>,
    server_sender: RwServerSender,
    server_ip: String,
    options: ClientOptions,
) -> bool {
    match get_internal_websocket(connection_store, server_sender, server_ip, options).await {
        Ok(_) => true,
        Err(e) => {
            log_error!("Error getting websocket: {:?}", e);
            false
        }
    }
}

/// Establishes a WebSocket connection to an internal server.
///
/// Attempts to connect to the specified server with a timeout, then hands off
/// the connection to the WebSocket handler if successful.
///
/// # Arguments
///
/// * `connection_store` - Persistence for connection-identity state
/// * `server_sender` - Server sender for message handling
/// * `server_ip` - Server address to connect to
/// * `options` - Client connection options
///
/// # Returns
///
/// A Result indicating whether the connection process completed successfully
pub async fn get_internal_websocket(
    connection_store: Arc<dyn ConnectionStore>,
    server_sender: RwServerSender,
    server_ip: String,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
    // Claim the single-flight guard *before* dialing, not after the handshake
    // succeeds — otherwise a concurrent reconnect trigger can start its own
    // connect_async during this window. If another attempt already owns it,
    // there's nothing for this call to do.
    let Some(connect_guard) = TryConnectGuard::try_acquire(server_sender.clone()).await else {
        return Ok(());
    };

    log_debug!("Connecting to {}", server_ip);
    match timeout(
        Duration::from_secs(options.connect_timeout_seconds),
        connect_async(&server_ip),
    )
    .await
    {
        Ok(Ok((ws_stream, _))) => {
            if let Err(err) = handle_websocket(
                connection_store,
                server_sender.clone(),
                options,
                server_ip.clone(),
                ws_stream,
                connect_guard,
            )
            .await
            {
                log_error!("Error handling websocket: {:?}", err);
            }
        }
        Err(e) => {
            // connect_guard drops here, resetting is_try_connect automatically.
            server_sender.remove_ip_if_valid_server_ip(&server_ip).await;
            log_error!("Error connecting to {}: {:?}", server_ip, e);
        }
        Ok(Err(e)) => {
            server_sender.remove_ip_if_valid_server_ip(&server_ip).await;
            log_error!("Error connecting to {}: {:?}", server_ip, e);
        }
    }
    log_debug!("Connection session ended for {}", server_ip);
    Ok(())
}

/// Handles an established WebSocket connection.
///
/// Sets up bidirectional message handling between the client and server,
/// including automatic ping/pong for connection health monitoring.
///
/// # Arguments
///
/// * `connection_store` - Persistence for connection-identity state
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options
/// * `server_ip` - Server address connected to
/// * `ws_stream` - Established WebSocket stream
///
/// # Returns
///
/// A Result indicating whether the connection handling completed successfully
///
/// `connect_guard` must already hold the `is_try_connect` single-flight lock
/// (acquired by the caller before the handshake started); this function owns
/// it for the life of the connection and disarms it on normal exit.
#[cfg(feature = "bebop")]
pub async fn handle_websocket(
    connection_store: Arc<dyn ConnectionStore>,
    server_sender: RwServerSender,
    options: ClientOptions,
    server_ip: String,
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut connect_guard: TryConnectGuard,
) -> tokio_tungstenite::tungstenite::Result<()> {
    let (mut ostream, mut istream) = ws_stream.split();
    log_debug!("Connected to {} for web socket", server_ip);

    let (sx, mut rx) = mpsc::channel(options.per_connection_buffer_size);
    let id = connection_store.get_client_id().await;
    server_sender.add(sx.clone(), &server_ip).await;

    let mut is_first = true;
    let use_ping = options.use_ping;
    if use_ping {
        log_debug!("Client send message: {:?}", make_ping_message(&id));
        server_sender.send(make_ping_message(&id)).await;
    } else {
        is_first = false;
        server_sender.write_received_times().await;
        server_sender.send_status(SenderStatus::Connected).await;
    }

    let retry_seconds = options.retry_seconds;
    let server_sender_clone = server_sender.clone();
    let server_ip_clone = server_ip.clone();
    let (stream_end_tx, mut stream_end_rx) = tokio::sync::oneshot::channel::<()>();

    // Spawn a task to handle incoming messages
    tokio::spawn(async move {
        let server_ip = server_ip_clone;
        let server_sender = server_sender_clone;
        let is_wait_ping = Arc::new(AtomicBool::new(false));

        while let Some(Ok(message)) = istream.next().await {
            server_sender.write_received_times().await;
            let value = message.into_data();
            let data = match get_data_schema(&value) {
                Ok(data) => data,
                Err(e) => {
                    log_error!("Error getting data schema: {:?}", e);
                    continue;
                }
            };

            if is_first {
                is_first = false;
                server_sender.send_status(SenderStatus::Connected).await;
            }
            let id = id.clone();
            log_debug!("Client receive message: {:?}", data);
            if data.category == Category::Pong as u16 {
                if !is_wait_ping.is_true() {
                    is_wait_ping.set_bool(true);
                    let server_sender_clone = server_sender.clone();
                    let is_wait_ping_clone = is_wait_ping.clone();
                    // Schedule the next ping after receiving a pong
                    tokio::spawn(async move {
                        sleep(Duration::from_secs(retry_seconds)).await;
                        is_wait_ping_clone.set_bool(false);
                        server_sender_clone.send(make_ping_message(&id)).await;
                    });
                }
                continue;
            } else if data.category == Category::Disconnect as u16 {
                // Parse server IP from URL format (e.g., "ws://192.168.1.1:8080")
                let peer = server_ip
                    .split("://")
                    .nth(1)
                    .and_then(|s| s.split(':').next())
                    .unwrap_or(&server_ip);
                let _ = sx.send(make_disconnect_message(peer)).await;
                break;
            }
            server_sender.send_handle_message(data).await;
        }
        // Notify writer that the read stream has ended (server disconnected)
        let _ = stream_end_tx.send(());
    });

    // Handle outgoing messages, also watching for reader stream end
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(message) => {
                        match ostream.send(message.clone()).await {
                            Ok(_) => {
                                let data = message.into_data();
                                let data = match get_data_schema(&data) {
                                    Ok(data) => data,
                                    Err(e) => {
                                        log_error!("Error getting data schema: {:?}", e);
                                        rx.close();
                                        break;
                                    }
                                };
                                log_debug!("Send message: {:?}", data);
                                if data.category == Category::Disconnect as u16 {
                                    rx.close();
                                    break;
                                }
                            }
                            Err(e) => {
                                log_error!("Error sending message: {:?}", e);
                                break;
                            }
                        }
                    }
                    None => break,
                }
            }
            _ = &mut stream_end_rx => {
                // Reader stream ended — connection is dead
                break;
            }
        }
    }
    log_debug!("WebSocket closed");
    let _ = timeout(Duration::from_secs(1), ostream.flush()).await;
    // Normal exit: reset flag directly and disarm guard
    server_sender.write().await.is_try_connect = false;
    connect_guard.disarm();
    Ok(())
}

/// Handles an established WebSocket connection (raw bytes version).
#[cfg(not(feature = "bebop"))]
pub async fn handle_websocket(
    connection_store: Arc<dyn ConnectionStore>,
    server_sender: RwServerSender,
    options: ClientOptions,
    server_ip: String,
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut connect_guard: TryConnectGuard,
) -> tokio_tungstenite::tungstenite::Result<()> {
    let _ = connection_store;
    let (mut ostream, mut istream) = ws_stream.split();
    log_debug!("Connected to {} for web socket", server_ip);

    let (sx, mut rx) = mpsc::channel(options.per_connection_buffer_size);
    server_sender.add(sx.clone(), &server_ip).await;

    // Without bebop there is no pong handshake, so emit Connected immediately
    // and set the initial received timestamp for the loop checker.
    server_sender.write_received_times().await;
    server_sender.send_status(SenderStatus::Connected).await;

    let server_sender_clone = server_sender.clone();
    let (stream_end_tx, mut stream_end_rx) = tokio::sync::oneshot::channel::<()>();

    // Spawn a task to handle incoming messages - pass raw bytes
    tokio::spawn(async move {
        let server_sender = server_sender_clone;

        while let Some(Ok(message)) = istream.next().await {
            server_sender.write_received_times().await;
            let value = message.into_data();
            server_sender.send_handle_message(value.to_vec()).await;
        }
        // Notify writer that the read stream has ended (server disconnected)
        let _ = stream_end_tx.send(());
    });

    // Handle outgoing messages, also watching for reader stream end
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(message) => {
                        if let Err(e) = ostream.send(message).await {
                            log_error!("Error sending message: {:?}", e);
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = &mut stream_end_rx => {
                // Reader stream ended — connection is dead
                break;
            }
        }
    }
    log_debug!("WebSocket closed");
    let _ = timeout(Duration::from_secs(1), ostream.flush()).await;
    // Normal exit: reset flag directly and disarm guard
    server_sender.write().await.is_try_connect = false;
    connect_guard.disarm();
    Ok(())
}

/// Retrieves the client identifier from the database.
///
/// # Arguments
///
/// * `db` - Database instance
///
/// # Returns
///
/// The client identifier as a string, or empty string if not found
#[cfg(feature = "native-db")]
pub async fn get_id(db: DB) -> String {
    // Run the synchronous redb read on the blocking pool so it never stalls a
    // Tokio worker thread (see `common::flatten_join` for the rationale).
    tokio::task::spawn_blocking(move || {
        let db = db.blocking_lock();
        let Ok(reader) = db.r_transaction() else {
            return String::new();
        };

        let Ok(Some(data)) = reader.get().primary::<Settings>(save_key::CLIENT_ID) else {
            return String::new();
        };

        String::from_utf8(data.value).unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

/// Retrieves the client identifier from in-memory storage.
#[cfg(not(feature = "native-db"))]
pub async fn get_id(db: DB) -> String {
    let db = db.lock().await;
    db.get(save_key::CLIENT_ID)
        .map(|v| String::from_utf8(v.clone()).unwrap_or_default())
        .unwrap_or_default()
}
