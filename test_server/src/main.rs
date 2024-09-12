use std::{env::current_dir, error::Error, path::PathBuf, sync::OnceLock, time::Duration};

use atomic_websocket::{
    client_sender::ServerOptions,
    external::native_db::{Builder, Database, Models},
    AtomicWebsocket, Settings,
};
use tokio::{sync::broadcast::Receiver, time::sleep};

#[tokio::main]
async fn main() {
    let config_str = include_str!("log_config.yml");
    let config = serde_yaml::from_str(config_str).unwrap();
    log4rs::init_raw_config(config).unwrap();

    let port = "9000";
    let address: String = format!("0.0.0.0:{}", port);

    tokio::spawn(server_start(address.clone()));

    loop {
        sleep(Duration::from_secs(100)).await;
    }
}

async fn server_start(address: String) {
    let option = ServerOptions::default();

    let atomic_server = AtomicWebsocket::get_internal_server(address, option).await;
    let handle_message_receiver = atomic_server.get_handle_message_receiver().await;

    tokio::spawn(receive_server_handle_message(handle_message_receiver));
}

pub async fn receive_server_handle_message(mut receiver: Receiver<(Vec<u8>, String)>) {
    while let Ok(message) = receiver.recv().await {
        log::debug!("Message: {:?}", message);
    }
}

pub fn get_db_path() -> Result<PathBuf, Box<dyn Error>> {
    let mut current_path = PathBuf::from(current_dir().unwrap());
    current_path.push("database.redb");
    Ok(current_path)
}

pub fn make_models() -> &'static Models {
    static BUILDER: OnceLock<Models> = OnceLock::new();
    BUILDER.get_or_init(|| {
        let mut models = Models::new();
        if let Err(_) = models.define::<Settings>() {
            log::error!("Failed to define ClientTable");
        };
        models
    })
}

pub fn make_db(models: &'static Models, path: PathBuf) -> Database<'static> {
    let mut db = None;
    while db.is_none() {
        match Builder::new().create(models, &path) {
            Ok(database) => {
                db = Some(database);
            }
            Err(error) => {
                panic!("Failed to create db {error:?}");
            }
        }
    }
    db.unwrap()
}
