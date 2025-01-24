use std::sync::Arc;

use helpers::{
    internal_client::{AtomicClient, ClientOptions},
    internal_server::{AtomicServer, ServerOptions},
    types::DB,
};
use native_db::{native_db, ToKey};
use native_model::{native_model, Model};
use serde::{Deserialize, Serialize};

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

pub mod schema {
    pub use crate::generated::schema::*;
}

pub mod client_sender {
    pub use crate::helpers::client_sender::*;
    pub use crate::helpers::internal_server::ServerOptions;
}

pub mod server_sender {
    pub use crate::helpers::internal_client::{
        get_internal_connect, get_ip_address, ClientOptions,
    };
    pub use crate::helpers::server_sender::*;
}

pub mod common {
    pub use crate::helpers::common::{
        get_setting_by_key, make_atomic_message, make_response_message, set_setting,
    };
    pub use crate::helpers::get_internal_websocket::get_id;
}
pub mod types {
    pub use crate::helpers::types::*;
}

use server_sender::{ServerSender, ServerSenderTrait};
use tokio::sync::RwLock;
use types::{RwClientSenders, RwServerSender};

mod generated;
mod helpers;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[native_model(id = 1004, version = 1)]
#[native_db]
pub struct Settings {
    #[primary_key]
    pub key: String,
    pub value: Vec<u8>,
}

pub struct AtomicWebsocket {}

enum AtomicWebsocketType {
    Internal,
    Outer,
}

impl AtomicWebsocket {
    pub async fn get_internal_client(db: DB, options: ClientOptions) -> AtomicClient {
        get_client(db, options, AtomicWebsocketType::Internal, None).await
    }
    pub async fn get_internal_client_with_server_sender(
        db: DB,
        options: ClientOptions,
        server_sender: RwServerSender,
    ) -> AtomicClient {
        get_client(
            db,
            options,
            AtomicWebsocketType::Internal,
            Some(server_sender),
        )
        .await
    }

    pub async fn get_outer_client(db: DB, options: ClientOptions) -> AtomicClient {
        get_client(db, options, AtomicWebsocketType::Outer, None).await
    }

    pub async fn get_outer_client_with_server_sender(
        db: DB,
        options: ClientOptions,
        server_sender: RwServerSender,
    ) -> AtomicClient {
        get_client(db, options, AtomicWebsocketType::Outer, Some(server_sender)).await
    }

    pub async fn get_internal_server(addr: String, option: ServerOptions) -> AtomicServer {
        AtomicServer::new(&addr, option, None).await
    }

    pub async fn get_internal_server_with_client_senders(
        addr: String,
        option: ServerOptions,
        client_senders: RwClientSenders,
    ) -> AtomicServer {
        AtomicServer::new(&addr, option, Some(client_senders)).await
    }
}

async fn get_client(
    db: DB,
    options: ClientOptions,
    atomic_websocket_type: AtomicWebsocketType,
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
    match atomic_websocket_type {
        AtomicWebsocketType::Internal => atomic_websocket.internal_initialize(db.clone()).await,
        AtomicWebsocketType::Outer => atomic_websocket.outer_initialize(db.clone()).await,
    }
    atomic_websocket
}
