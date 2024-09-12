use std::sync::Arc;

use helpers::{
    internal_client::{AtomicClient, ClientOptions},
    internal_server::{AtomicServer, ServerOptions},
};
use native_db::{native_db, Database, ToKey};
use native_model::{native_model, Model};
use serde::{Deserialize, Serialize};

pub mod external {
    pub use async_trait;
    pub use futures_util;
    pub use nanoid;
    pub use native_db;
    pub use native_model;
    #[cfg(feature = "native_tls")]
    pub use native_tls;
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
    pub use crate::helpers::internal_client::{get_internal_connect, ClientOptions};
    pub use crate::helpers::server_sender::*;
}

pub mod common {
    pub use crate::helpers::common::{get_setting_by_key, make_response_message, set_setting};
    pub use crate::helpers::get_internal_websocket::get_id;
}

use server_sender::{ServerSender, ServerSenderTrait};
use tokio::sync::RwLock;

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

impl AtomicWebsocket {
    pub async fn get_internal_client(
        db: Arc<RwLock<Database<'static>>>,
        options: ClientOptions,
    ) -> AtomicClient {
        let mut server_sender = Arc::new(RwLock::new(ServerSender::new(
            db.clone(),
            "".into(),
            options.clone(),
        )));
        server_sender.regist(server_sender.clone()).await;

        let atomic_websocket: AtomicClient = AtomicClient {
            server_sender,
            options,
        };
        atomic_websocket.internal_initialize(db.clone()).await;
        atomic_websocket
    }

    pub async fn get_outer_client(
        db: Arc<RwLock<Database<'static>>>,
        options: ClientOptions,
    ) -> AtomicClient {
        let mut server_sender = Arc::new(RwLock::new(ServerSender::new(
            db.clone(),
            match options.url.is_empty() {
                true => "".into(),
                false => options.url.clone(),
            },
            options.clone(),
        )));
        server_sender.regist(server_sender.clone()).await;

        let atomic_websocket: AtomicClient = AtomicClient {
            server_sender,
            options,
        };
        atomic_websocket.outer_initialize(db.clone()).await;
        atomic_websocket
    }

    pub async fn get_internal_server(addr: String, option: ServerOptions) -> AtomicServer {
        AtomicServer::new(&addr, option).await
    }
}
