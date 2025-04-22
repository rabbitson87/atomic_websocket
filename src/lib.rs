//! # atomic_websocket
//!
//! `atomic_websocket` is a high-level WebSocket client and server implementation for Rust built on top of tokio-tungstenite.
//! It provides resilient WebSocket connections with the following features:
//!
//! - Automatic connection recovery and ping/pong handling
//! - Local network server auto-discovery
//! - Connection status monitoring and events
//! - Serialization/deserialization support
//!
//! ## Basic Usage
//!
//! ### Client Example
//!
//! ```rust
//! use atomic_websocket::{
//!     AtomicWebsocket,
//!     server_sender::{ClientOptions, SenderStatus},
//!     schema::ServerConnectInfo,
//! };
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Configure client options
//!     let mut client_options = ClientOptions::default();
//!     client_options.retry_seconds = 2;
//!     client_options.use_keep_ip = true;
//!     
//!     // Initialize DB (implementation details omitted)
//!     let db = initialize_database().await?;
//!     let server_sender = initialize_server_sender().await?;
//!     
//!     // Create client
//!     let atomic_client = AtomicWebsocket::get_internal_client_with_server_sender(
//!         db.clone(),
//!         client_options,
//!         server_sender.clone(),
//!     ).await;
//!     
//!     // Connect to server
//!     let result = atomic_client
//!         .get_internal_connect(
//!             Some(ServerConnectInfo {
//!                 server_ip: "",
//!                 port: "9000",
//!             }),
//!             db.clone(),
//!         )
//!         .await;
//!     
//!     Ok(())
//! }
//! ```
//!
//! ### Server Example
//!
//! ```rust
//! use atomic_websocket::{
//!     AtomicWebsocket,
//!     client_sender::ServerOptions,
//! };
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Configure server options
//!     let options = ServerOptions::default();
//!     
//!     // Initialize client senders for managing connections
//!     let client_senders = initialize_client_senders().await?;
//!     
//!     // Create and start server
//!     let address = "0.0.0.0:9000";
//!     let atomic_server = AtomicWebsocket::get_internal_server_with_client_senders(
//!         address.to_string(),
//!         options,
//!         client_senders.clone(),
//!     ).await;
//!     
//!     // Set up message handler
//!     let handle_message_receiver = atomic_server.get_handle_message_receiver().await;
//!     tokio::spawn(handle_messages(handle_message_receiver));
//!     
//!     Ok(())
//! }
//! ```

use std::sync::Arc;

use helpers::{
    internal_client::{AtomicClient, ClientOptions},
    internal_server::{AtomicServer, ServerOptions},
    types::DB,
};
use native_db::{native_db, ToKey};
use native_model::{native_model, Model};
use serde::{Deserialize, Serialize};

/// Module that re-exports various external dependencies.
pub mod external {
    pub use async_trait;
    pub use futures_util;
    pub use nanoid;
    pub use native_db;
    pub use native_model;
    #[cfg(feature = "rustls")]
    pub use rustls;
    pub use tokio;
    pub use tokio_tungstenite;
}

/// Module containing message schema definitions.
///
/// This module defines the message formats exchanged between clients and servers.
pub mod schema {
    pub use crate::generated::schema::*;
}

/// Module providing functionality for managing client connections on the server side.
pub mod client_sender {
    pub use crate::helpers::client_sender::*;
    pub use crate::helpers::internal_server::ServerOptions;
}

/// Module providing functionality for managing server connections on the client side.
pub mod server_sender {
    pub use crate::helpers::internal_client::{
        get_internal_connect, get_ip_address, ClientOptions,
    };
    pub use crate::helpers::server_sender::*;
}

/// Module providing common utility functions for WebSocket communication.
pub mod common {
    pub use crate::helpers::common::{
        get_setting_by_key, make_atomic_message, make_response_message, set_setting,
    };
    pub use crate::helpers::get_internal_websocket::get_id;
}

/// Module containing common type definitions used throughout the library.
pub mod types {
    pub use crate::helpers::types::*;
}

use server_sender::{ServerSender, ServerSenderTrait};
use tokio::sync::RwLock;
use types::{RwClientSenders, RwServerSender};

mod generated;
mod helpers;

/// Database model for storing client settings and state.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[native_model(id = 1004, version = 1)]
#[native_db]
pub struct Settings {
    #[primary_key]
    pub key: String,
    pub value: Vec<u8>,
}

/// Primary entry point for creating WebSocket clients and servers.
///
/// This struct provides static methods for creating client and server
/// instances for both internal and external WebSocket connections.
pub struct AtomicWebsocket {}

/// Internal enum to distinguish WebSocket connection types.
#[derive(Debug, Clone)]
pub enum AtomicWebsocketType {
    /// Connection to a server on the local network
    Internal,
    /// Connection to an external internet server
    External,
}

impl AtomicWebsocket {
    /// Creates a basic client instance for internal network use.
    ///
    /// # Arguments
    ///
    /// * `db` - Database instance for storing settings and state
    /// * `options` - Client connection options (auto-reconnect, ping intervals, etc.)
    ///
    /// # Returns
    ///
    /// A newly created `AtomicClient` instance
    ///
    /// # Examples
    ///
    /// ```
    /// let client_options = ClientOptions::default();
    /// let client = AtomicWebsocket::get_internal_client(db.clone(), client_options).await;
    /// ```
    pub async fn get_internal_client(db: DB, mut options: ClientOptions) -> AtomicClient {
        options.atomic_websocket_type = AtomicWebsocketType::Internal;
        get_client(db, options, None).await
    }

    /// Creates a client instance for internal network use with an existing ServerSender.
    ///
    /// # Arguments
    ///
    /// * `db` - Database instance for storing settings and state
    /// * `options` - Client connection options
    /// * `server_sender` - Existing ServerSender instance
    ///
    /// # Returns
    ///
    /// A newly created `AtomicClient` instance
    pub async fn get_internal_client_with_server_sender(
        db: DB,
        mut options: ClientOptions,
        server_sender: RwServerSender,
    ) -> AtomicClient {
        options.atomic_websocket_type = AtomicWebsocketType::Internal;
        get_client(db, options, Some(server_sender)).await
    }

    /// Creates a client instance for external servers.
    ///
    /// # Arguments
    ///
    /// * `db` - Database instance for storing settings and state
    /// * `options` - Client connection options (including URL, TLS settings, etc.)
    ///
    /// # Returns
    ///
    /// A newly created `AtomicClient` instance
    pub async fn get_outer_client(db: DB, mut options: ClientOptions) -> AtomicClient {
        options.atomic_websocket_type = AtomicWebsocketType::External;
        get_client(db, options, None).await
    }

    /// Creates a client instance for external servers with an existing ServerSender.
    ///
    /// # Arguments
    ///
    /// * `db` - Database instance for storing settings and state
    /// * `options` - Client connection options
    /// * `server_sender` - Existing ServerSender instance
    ///
    /// # Returns
    ///
    /// A newly created `AtomicClient` instance
    pub async fn get_outer_client_with_server_sender(
        db: DB,
        mut options: ClientOptions,
        server_sender: RwServerSender,
    ) -> AtomicClient {
        options.atomic_websocket_type = AtomicWebsocketType::External;
        get_client(db, options, Some(server_sender)).await
    }

    /// Creates a server instance for internal network use.
    ///
    /// # Arguments
    ///
    /// * `addr` - Address to bind the server to (e.g., "0.0.0.0:9000")
    /// * `option` - Server configuration options
    ///
    /// # Returns
    ///
    /// A newly created `AtomicServer` instance
    ///
    /// # Examples
    ///
    /// ```
    /// let server_options = ServerOptions::default();
    /// let server = AtomicWebsocket::get_internal_server("127.0.0.1:9000".to_string(), server_options).await;
    /// ```
    pub async fn get_internal_server(addr: String, option: ServerOptions) -> AtomicServer {
        AtomicServer::new(&addr, option, None).await
    }

    /// Creates a server instance for internal network use with existing ClientSenders.
    ///
    /// # Arguments
    ///
    /// * `addr` - Address to bind the server to (e.g., "0.0.0.0:9000")
    /// * `option` - Server configuration options
    /// * `client_senders` - Existing ClientSenders instance
    ///
    /// # Returns
    ///
    /// A newly created `AtomicServer` instance
    pub async fn get_internal_server_with_client_senders(
        addr: String,
        option: ServerOptions,
        client_senders: RwClientSenders,
    ) -> AtomicServer {
        AtomicServer::new(&addr, option, Some(client_senders)).await
    }
}

/// Internal helper function: Creates a client instance.
///
/// # Arguments
///
/// * `db` - Database instance
/// * `options` - Client options
/// * `atomic_websocket_type` - Connection type (Internal or External)
/// * `server_sender` - Optional existing ServerSender instance
///
/// # Returns
///
/// An initialized AtomicClient instance
async fn get_client(
    db: DB,
    options: ClientOptions,
    server_sender: Option<RwServerSender>,
) -> AtomicClient {
    let mut server_sender = match server_sender {
        Some(server_sender) => {
            let server_sender_clone = server_sender.clone();
            let mut server_sender_clone = server_sender_clone.write().await;
            server_sender_clone.server_ip = options.url.clone();
            server_sender_clone.options = options.clone();
            drop(server_sender_clone);
            server_sender
        }
        None => Arc::new(RwLock::new(ServerSender::new(
            db.clone(),
            options.url.clone(),
            options.clone(),
        ))),
    };
    server_sender.regist(server_sender.clone()).await;

    let atomic_websocket: AtomicClient = AtomicClient {
        server_sender,
        options,
    };
    match atomic_websocket.options.atomic_websocket_type {
        AtomicWebsocketType::Internal => atomic_websocket.internal_initialize(db.clone()).await,
        AtomicWebsocketType::External => atomic_websocket.outer_initialize(db.clone()).await,
    }
    atomic_websocket
}
