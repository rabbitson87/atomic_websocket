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
        server_sender::{SenderStatus, ServerSenderTrait},
        traits::atomic::FlagAtomic,
    },
    log_debug, log_error, Settings,
};

use super::{
    internal_client::ClientOptions,
    types::{save_key, RwServerSender, DB},
};

/// Wrapper function for establishing an internal WebSocket connection.
///
/// Handles errors from the connection attempt and logs them appropriately.
///
/// # Arguments
///
/// * `db` - Database instance for storing connection state
/// * `server_sender` - Server sender for message handling
/// * `server_ip` - Server address to connect to
/// * `options` - Client connection options
///
/// # Returns
///
/// `true` if the connection was successfully established, `false` otherwise
pub async fn wrap_get_internal_websocket(
    db: DB,
    server_sender: RwServerSender,
    server_ip: String,
    options: ClientOptions,
) -> bool {
    match get_internal_websocket(db, server_sender, server_ip, options).await {
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
/// * `db` - Database instance for storing connection state
/// * `server_sender` - Server sender for message handling
/// * `server_ip` - Server address to connect to
/// * `options` - Client connection options
///
/// # Returns
///
/// A Result indicating whether the connection process completed successfully
pub async fn get_internal_websocket(
    db: DB,
    server_sender: RwServerSender,
    server_ip: String,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
    log_debug!("Connecting to {}", server_ip);
    match timeout(
        Duration::from_secs(options.connect_timeout_seconds),
        connect_async(&server_ip),
    )
    .await
    {
        Ok(Ok((ws_stream, _))) => {
            if let Err(err) = handle_websocket(
                db,
                server_sender.clone(),
                options,
                server_ip.clone(),
                ws_stream,
            )
            .await
            {
                server_sender.write().await.is_try_connect = false;
                log_error!("Error handling websocket: {:?}", err);
            }
        }
        Err(e) => {
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
/// * `db` - Database instance for storing connection state
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options
/// * `server_ip` - Server address connected to
/// * `ws_stream` - Established WebSocket stream
///
/// # Returns
///
/// A Result indicating whether the connection handling completed successfully
#[cfg(feature = "bebop")]
pub async fn handle_websocket(
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
    server_ip: String,
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> tokio_tungstenite::tungstenite::Result<()> {
    {
        let mut guard = server_sender.write().await;
        if guard.is_try_connect {
            return Ok(());
        }
        guard.is_try_connect = true;
    }
    let (mut ostream, mut istream) = ws_stream.split();
    log_debug!("Connected to {} for web socket", server_ip);

    let (sx, mut rx) = mpsc::channel(8);
    let id = get_id(db.clone()).await;
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
    server_sender.write().await.is_try_connect = false;
    Ok(())
}

/// Handles an established WebSocket connection (raw bytes version).
#[cfg(not(feature = "bebop"))]
pub async fn handle_websocket(
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
    server_ip: String,
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> tokio_tungstenite::tungstenite::Result<()> {
    {
        let mut guard = server_sender.write().await;
        if guard.is_try_connect {
            return Ok(());
        }
        guard.is_try_connect = true;
    }
    let (mut ostream, mut istream) = ws_stream.split();
    log_debug!("Connected to {} for web socket", server_ip);

    let (sx, mut rx) = mpsc::channel(8);
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
    server_sender.write().await.is_try_connect = false;
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
    let db = db.lock().await;
    let Ok(reader) = db.r_transaction() else {
        return String::new();
    };

    let Ok(Some(data)) = reader.get().primary::<Settings>(save_key::CLIENT_ID) else {
        return String::new();
    };

    String::from_utf8(data.value).unwrap_or_default()
}

/// Retrieves the client identifier from in-memory storage.
#[cfg(not(feature = "native-db"))]
pub async fn get_id(db: DB) -> String {
    let db = db.lock().await;
    db.get(save_key::CLIENT_ID)
        .map(|v| String::from_utf8(v.clone()).unwrap_or_default())
        .unwrap_or_default()
}
