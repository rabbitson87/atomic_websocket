//! WebSocket server implementation for atomic_websocket.
//!
//! This module provides the server-side functionality for accepting and managing
//! WebSocket connections, including client tracking, message routing, and automatic
//! ping/pong handling.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use tokio::{
    self,
    net::{TcpListener, TcpStream},
    sync::{mpsc::Receiver, RwLock},
    time::{Instant, MissedTickBehavior},
};
use tokio_tungstenite::{tungstenite::Error, WebSocketStream};

use crate::{
    helpers::{
        client_sender::ClientSendersTrait,
        common::{get_data_schema, make_disconnect_message, make_pong_message},
    },
    log_debug, log_error,
    schema::{Category, Ping},
};
use bebop::Record;
use futures_util::{stream::SplitStream, SinkExt, StreamExt};
use tokio::sync::mpsc::{self, Sender};
use tokio_tungstenite::{
    accept_async,
    tungstenite::{self, Message},
};

use super::{client_sender::ClientSenders, types::RwClientSenders};

/// WebSocket server implementation for accepting and managing client connections.
///
/// Manages WebSocket client connections and routes messages between clients.
pub struct AtomicServer {
    /// Collection of connected clients
    pub client_senders: RwClientSenders,
}

/// Configuration options for the WebSocket server.
///
/// Controls aspects of server behavior such as ping handling.
#[derive(Clone)]
pub struct ServerOptions {
    /// Whether to automatically respond to ping messages with pongs
    pub use_ping: bool,

    /// Category ID to use when proxying ping messages instead of responding directly
    /// A value of -1 disables ping proxying
    pub proxy_ping: i16,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            use_ping: true,
            proxy_ping: -1,
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
    /// A new AtomicServer instance
    ///
    /// # Panics
    ///
    /// Panics if the server cannot bind to the specified address
    pub async fn new(
        addr: &str,
        option: ServerOptions,
        client_senders: Option<RwClientSenders>,
    ) -> Self {
        let listener = TcpListener::bind(&addr).await.expect("Can't listen");
        let client_senders = match client_senders {
            Some(client_senders) => client_senders,
            None => Arc::new(RwLock::new(ClientSenders::new())),
        };
        tokio::spawn(handle_accept(listener, client_senders.clone(), option));

        tokio::spawn(loop_client_checker(client_senders.clone()));
        Self { client_senders }
    }

    /// Gets a receiver for incoming messages from clients.
    ///
    /// # Returns
    ///
    /// A channel receiver for message data along with client identifiers
    pub async fn get_handle_message_receiver(&self) -> Receiver<(Vec<u8>, String)> {
        self.client_senders.get_handle_message_receiver().await
    }
}

/// Periodically checks for and removes inactive clients.
///
/// # Arguments
///
/// * `server_sender` - Shared client senders collection
pub async fn loop_client_checker(server_sender: RwClientSenders) {
    let mut interval = tokio::time::interval_at(
        Instant::now() + Duration::from_secs(15),
        Duration::from_secs(15),
    );
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        server_sender.write().await.check_client_send_time();
        log_debug!("loop client cheker finish");
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
/// * `option` - Server configuration options
pub async fn handle_accept(
    listener: TcpListener,
    client_senders: RwClientSenders,
    option: ServerOptions,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let peer = stream
                    .peer_addr()
                    .expect("connected streams should have a peer address");
                log_debug!("Peer address: {}", peer);
                tokio::spawn(accept_connection(
                    client_senders.clone(),
                    peer,
                    stream,
                    option.clone(),
                ));
            }
            Err(e) => {
                log_error!("Error accepting connection: {:?}", e);
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
/// * `option` - Server configuration options
pub async fn accept_connection(
    client_senders: RwClientSenders,
    peer: SocketAddr,
    stream: TcpStream,
    option: ServerOptions,
) {
    if let Err(e) = handle_connection(client_senders, peer, stream, option).await {
        match e {
            Error::ConnectionClosed | Error::Protocol(_) | Error::Utf8 => (),
            err => log_error!("Error processing connection: {}", err),
        }
    }
}

/// Handles an established WebSocket connection.
///
/// Sets up bidirectional message handling for the client connection.
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
pub async fn handle_connection(
    client_senders: RwClientSenders,
    peer: SocketAddr,
    stream: TcpStream,
    option: ServerOptions,
) -> tungstenite::Result<()> {
    match accept_async(stream).await {
        Ok(ws_stream) => {
            log_debug!("New WebSocket connection: {}", peer);
            let (mut ostream, mut istream) = ws_stream.split();

            let (sx, mut rx) = mpsc::channel(8);
            tokio::spawn(async move {
                let use_ping = option.use_ping;
                let id = get_id_from_first_message(
                    &mut istream,
                    client_senders.clone(),
                    sx.clone(),
                    option,
                )
                .await;

                match id {
                    Some(id) => {
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
                                    client_senders
                                        .send(data.peer.into(), make_pong_message())
                                        .await;
                                    continue;
                                }
                            }

                            // Handle disconnect messages
                            if data.category == Category::Disconnect as u16 {
                                let _ = sx.send(make_disconnect_message(&peer.to_string())).await;
                                break;
                            }

                            // Forward other messages to application handler
                            client_senders.send_handle_message(data, &id).await;
                        }
                    }
                    None => {
                        let _ = sx.send(make_disconnect_message(&peer.to_string())).await;
                    }
                }
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
            ostream.flush().await?;
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
async fn get_id_from_first_message(
    istream: &mut SplitStream<WebSocketStream<TcpStream>>,
    client_senders: RwClientSenders,
    sx: Sender<Message>,
    options: ServerOptions,
) -> Option<String> {
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

        // Check if the first message is a ping
        if data.category == Category::Ping as u16 {
            log_debug!("receive ping from client: {:?}", data);
            if let Ok(ping) = Ping::deserialize(&data.datas) {
                _id = Some(ping.peer.into());
                client_senders.add(&_id.as_ref().unwrap(), sx).await;

                // Either respond with a pong or proxy the ping
                if options.use_ping {
                    client_senders
                        .send(&_id.as_ref().unwrap(), make_pong_message())
                        .await;
                } else {
                    // Optionally change the category when proxying
                    if options.proxy_ping > 0 {
                        data.category = options.proxy_ping as u16;
                    }
                    client_senders
                        .send_handle_message(data, &_id.as_ref().unwrap())
                        .await;
                }
            }
        } else if options.proxy_ping > 0 && data.category == options.proxy_ping as u16 {
            if let Ok(ping) = Ping::deserialize(&data.datas) {
                _id = Some(ping.peer.into());
                client_senders.add(&_id.as_ref().unwrap(), sx).await;

                // Optionally change the category when proxying
                data.category = options.proxy_ping as u16;
                client_senders
                    .send_handle_message(data, &_id.as_ref().unwrap())
                    .await;
            }
        }
    }
    _id
}
