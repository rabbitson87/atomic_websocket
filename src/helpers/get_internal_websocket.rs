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

use crate::{
    generated::schema::{Category, SaveKey},
    helpers::{
        common::{get_data_schema, make_disconnect_message, make_ping_message},
        server_sender::{SenderStatus, ServerSenderTrait},
        traits::{atomic::FlagAtomic, StringUtil},
    },
    log_debug, log_error, Settings,
};

use super::{
    internal_client::ClientOptions,
    types::{RwServerSender, DB},
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
                server_ip.copy_string(),
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
    log_debug!("Failed to server connect to {}", server_ip);
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
pub async fn handle_websocket(
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
    server_ip: String,
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> tokio_tungstenite::tungstenite::Result<()> {
    if !server_sender.is_need_connect(&server_ip).await {
        return Ok(());
    }

    server_sender.write().await.is_try_connect = true;
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
        if is_first {
            is_first = false;
            server_sender.send_status(SenderStatus::Connected).await;
        }
    }

    let retry_seconds = options.retry_seconds;
    let server_sender_clone = server_sender.clone();
    let server_ip_clone = server_ip.copy_string();

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
            let id = id.copy_string();
            log_debug!("Client receive message: {:?}", data);
            if data.category == Category::Pong as u16 {
                if !is_wait_ping.is_true() {
                    is_wait_ping.set_bool(true);
                    let server_sender_clone = server_sender.clone();
                    let is_wait_ping_clone = is_wait_ping.clone();
                    // Schedule the next ping after receiving a pong
                    tokio::spawn(async move {
                        sleep(Duration::from_secs(retry_seconds)).await;
                        server_sender_clone.send(make_ping_message(&id)).await;
                        is_wait_ping_clone.set_bool(false);
                    });
                }
                continue;
            } else if data.category == Category::Disconnect as u16 {
                let _ = sx
                    .send(make_disconnect_message(
                        server_ip
                            .split("://")
                            .nth(1)
                            .unwrap()
                            .split(":")
                            .nth(0)
                            .unwrap(),
                    ))
                    .await;
                break;
            }
            server_sender.send_handle_message(data).await;
        }
    });

    // Handle outgoing messages
    while let Some(message) = rx.recv().await {
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
    log_debug!("WebSocket closed");
    ostream.flush().await?;
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
/// The client identifier as a string
pub async fn get_id(db: DB) -> String {
    let db = db.lock().await;
    let reader = db.r_transaction().unwrap();

    let mut return_string = String::new();
    if let Some(data) = reader
        .get()
        .primary::<Settings>(format!("{:?}", SaveKey::ClientId))
        .unwrap()
    {
        return_string = String::from_utf8(data.value).unwrap().into()
    }
    return_string
}
