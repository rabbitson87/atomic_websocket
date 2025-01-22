use std::sync::Arc;

use native_db::Database;
use tokio::sync::{Mutex, RwLock};

use super::{client_sender::ClientSenders, server_sender::ServerSender};

pub type DB = Arc<Mutex<Database<'static>>>;

pub type RwServerSender = Arc<RwLock<ServerSender>>;

pub type RwClientSenders = Arc<RwLock<ClientSenders>>;
