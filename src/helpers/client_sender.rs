//! Client connection management module for the atomic_websocket server.
//!
//! This module provides functionality for managing multiple WebSocket client connections
//! on the server side, including message handling and connection timeouts.
use std::time::Duration;

use async_trait::async_trait;
use bebop::Record;
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    time::sleep,
};
use tokio_tungstenite::tungstenite::Message;

use crate::{
    client_sender::ServerOptions,
    helpers::{common::make_disconnect_message, traits::date_time::now},
    log_debug, log_error,
    schema::Data,
};

use super::{common::make_expired_output_message, traits::StringUtil, types::RwClientSenders};

/// Manages a collection of connected WebSocket clients on the server side.
///
/// This struct maintains a list of client connections and provides methods for
/// sending messages to specific clients, handling client timeouts, and processing
/// incoming messages.
pub struct ClientSenders {
    /// List of connected clients
    lists: Vec<ClientSender>,
    /// Channel sender for passing received messages to the application
    handle_message_sx: Sender<(Vec<u8>, String)>,
    /// Channel receiver for obtaining received messages (consumed once)
    handle_message_rx: Option<Receiver<(Vec<u8>, String)>>,
    /// Server options for connection management
    pub options: ServerOptions,
}

impl ClientSenders {
    /// Creates a new ClientSenders instance.
    ///
    /// Initializes an empty list of client connections and sets up message channels.
    ///
    /// # Returns
    ///
    /// A new ClientSenders instance
    pub fn new() -> Self {
        let (handle_message_sx, handle_message_rx) = mpsc::channel(1024);
        Self {
            lists: Vec::new(),
            handle_message_sx,
            handle_message_rx: Some(handle_message_rx),
            options: ServerOptions::default(),
        }
    }

    /// Adds or updates a client connection.
    ///
    /// If a client with the same peer identifier already exists, it replaces
    /// the sender channel with the new one after sending a disconnect message to
    /// the previous connection. Otherwise, it adds a new client to the list.
    ///
    /// # Arguments
    ///
    /// * `peer` - Client identifier (typically an address)
    /// * `sx` - Message sender channel for the client
    pub async fn add(&mut self, peer: &str, sx: Sender<Message>) {
        let list = self.lists.iter().position(|x| x.peer == peer);
        log_debug!("Add peer: {:?}, list: {:?}", peer, list);
        match list {
            Some(index) => {
                let list = self.lists.get_mut(index).unwrap();
                let _ = list.sx.send(make_disconnect_message(&peer)).await;
                list.sx = sx;
            }
            None => self.lists.push(ClientSender {
                peer: peer.into(),
                sx,
                send_time: 0,
            }),
        };
    }

    /// Retrieves the message receiver channel.
    ///
    /// This method can only be called once per instance as it consumes the receiver.
    ///
    /// # Returns
    ///
    /// The message receiver channel
    ///
    /// # Panics
    ///
    /// Panics if the receiver has already been taken
    pub fn get_handle_message_receiver(&mut self) -> Receiver<(Vec<u8>, String)> {
        self.handle_message_rx
            .take()
            .expect("Receiver already taken")
    }

    /// Sends a message to the application message handler.
    ///
    /// # Arguments
    ///
    /// * `data` - Binary message data
    /// * `peer` - Client identifier
    pub async fn send_handle_message(&self, data: Vec<u8>, peer: &str) {
        let handle_message_sx = self.handle_message_sx.clone();
        let _ = handle_message_sx.send((data, peer.into())).await;
    }

    /// Checks for client timeouts and removes inactive clients.
    ///
    /// Clients that haven't sent a message within 30 seconds of the current time
    /// are considered inactive and are removed from the client list.
    pub fn check_client_send_time(&mut self) {
        let now = now().timestamp();
        let mut remove_list = Vec::new();
        for client in self.lists.iter() {
            if client.send_time + 30 < now {
                remove_list.push(client.peer.copy_string());
            }
        }
        self.lists.retain(|x| !remove_list.contains(&x.peer));
    }

    /// Removes a client from the list.
    ///
    /// # Arguments
    ///
    /// * `peer` - Client identifier to remove
    pub fn remove(&mut self, peer: &str) {
        self.lists.retain(|x| x.peer != peer);
        log_debug!("Remove peer: {:?}", peer);
    }

    /// Updates the last message time for a client.
    ///
    /// # Arguments
    ///
    /// * `peer` - Client identifier
    pub fn write_time(&mut self, peer: &str) {
        for client in self.lists.iter_mut() {
            if client.peer == peer {
                client.write_time();
            }
        }
    }

    /// Sends a message to a specific client.
    ///
    /// Attempts to send a message to the specified client, with exponential backoff
    /// retry logic in case of failures.
    ///
    /// # Arguments
    ///
    /// * `peer` - Client identifier
    /// * `message` - WebSocket message to send
    ///
    /// # Returns
    ///
    /// `true` if the message was sent successfully, `false` otherwise
    pub async fn send(&self, peer: &str, message: Message) -> bool {
        for client in self.lists.iter() {
            if client.peer == peer {
                let sender = client.sx.clone();
                let mut backoff = Duration::from_millis(50); // 시작은 50ms로
                let max_backoff = Duration::from_secs(1); // 최대 1초
                let mut count = 0;

                loop {
                    match sender.send(message.clone()).await {
                        Ok(_) => return true,
                        Err(e) => {
                            if count > 5 {
                                log_error!("Failed to send after 5 retries: {:?}", e);
                                return false;
                            }

                            log_error!("Error sending message (attempt {}): {:?}", count + 1, e);
                            count += 1;

                            // Exponential backoff with max limit
                            backoff = std::cmp::min(backoff * 2, max_backoff);
                            sleep(backoff).await;
                        }
                    }
                }
            }
        }
        false
    }

    /// Checks if a client is active.
    ///
    /// # Arguments
    ///
    /// * `peer` - Client identifier
    ///
    /// # Returns
    ///
    /// `true` if the client exists in the list, `false` otherwise
    pub fn is_active(&self, peer: &str) -> bool {
        self.lists.iter().any(|x| x.peer == peer)
    }
}

/// Trait defining operations for client connection management.
///
/// This trait defines the interface for managing WebSocket client connections,
/// allowing for different implementations.
#[async_trait]
pub trait ClientSendersTrait {
    /// Adds or updates a client connection.
    async fn add(&self, peer: &str, sx: Sender<Message>);

    /// Gets the message receiver channel.
    async fn get_handle_message_receiver(&self) -> Receiver<(Vec<u8>, String)>;

    /// Sends a message to the application message handler.
    async fn send_handle_message(&self, data: Data<'_>, peer: &str);

    /// Sends a message to a specific client.
    async fn send(&self, peer: &str, message: Message) -> bool;

    /// Sends expiration messages to clients not in the provided list.
    async fn expire_send(&self, peer_list: Vec<String>);

    /// Checks if a client is active.
    async fn is_active(&self, peer: &str) -> bool;
}

/// Implementation of ClientSendersTrait for thread-safe client senders.
///
/// This implementation wraps a ClientSenders instance with read-write locks
/// to provide thread-safe access.
#[async_trait]
impl ClientSendersTrait for RwClientSenders {
    /// Adds or updates a client connection.
    async fn add(&self, peer: &str, sx: Sender<Message>) {
        self.write().await.add(peer, sx).await;
    }

    /// Gets the message receiver channel.
    async fn get_handle_message_receiver(&self) -> Receiver<(Vec<u8>, String)> {
        self.write().await.get_handle_message_receiver()
    }

    /// Sends a message to the application message handler.
    async fn send_handle_message(&self, data: Data<'_>, peer: &str) {
        let mut buf = Vec::new();
        data.serialize(&mut buf).unwrap();
        self.write().await.send_handle_message(buf, peer).await;
    }

    /// Sends a message to a specific client.
    ///
    /// Updates the client's last message time on success or removes the client on failure.
    async fn send(&self, peer: &str, message: Message) -> bool {
        let result = self.read().await.send(peer, message).await;

        match result {
            true => self.write().await.write_time(peer),
            false => self.write().await.remove(peer),
        }
        result
    }

    /// Sends expiration messages to clients not in the provided list.
    ///
    /// Used to notify clients when they are no longer valid or have been superseded.
    async fn expire_send(&self, peer_list: Vec<String>) {
        for peer in self.read().await.lists.iter() {
            if !peer_list.contains(&peer.peer) {
                self.send(&peer.peer, make_expired_output_message()).await;
            }
        }
    }

    /// Checks if a client is active.
    async fn is_active(&self, peer: &str) -> bool {
        self.read().await.is_active(peer)
    }
}

/// Represents a single WebSocket client connection.
///
/// Stores the client identifier, message sender channel, and the timestamp of the last message.
#[derive(Debug, Clone)]
struct ClientSender {
    /// Client identifier
    peer: String,
    /// Message sender channel
    sx: Sender<Message>,
    /// Timestamp of the last message sent
    send_time: i64,
}

impl ClientSender {
    /// Updates the last message timestamp to the current time.
    pub fn write_time(&mut self) {
        self.send_time = now().timestamp();
    }
}
