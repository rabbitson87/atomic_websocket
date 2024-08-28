use std::{
    env::current_dir,
    error::Error,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use atomic_websocket::{
    external::native_db::{Builder, Database, Models},
    schema::WindowAppConnectInfo,
    server_sender::SenderStatus,
    AtomicWebsocket, Settings,
};
use tokio::{
    sync::{watch::Receiver, RwLock},
    time::sleep,
};

#[tokio::main]
async fn main() {
    let port = "9000";
    let address: String = format!("0.0.0.0:{}", port);

    tokio::spawn(server_start(address.clone()));
    tokio::spawn(client_start(port));

    loop {
        sleep(Duration::from_secs(100)).await;
    }
}

async fn server_start(address: String) {
    let server = AtomicWebsocket::get_internal_server(address).await;
    let handle_message_receiver = server.get_handle_message_receiver().await;

    tokio::spawn(receive_server_handle_message(handle_message_receiver));
}

pub async fn receive_server_handle_message(mut receiver: Receiver<(Vec<u8>, String)>) {
    loop {
        let message = receiver.borrow_and_update().clone();
        println!("Message: {:?}", message);
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

async fn client_start(port: &str) {
    let current_path = match get_db_path() {
        Ok(path) => path,
        Err(error) => {
            println!("Failed to get db path {error:?}");
            return;
        }
    };
    let models = make_models();
    let db = make_db(models, current_path);
    let db = Arc::new(RwLock::new(db));

    let client = AtomicWebsocket::get_internal_client(db.clone()).await;

    let status_receiver = client.get_status_receiver().await;
    let handle_message_receiver = client.get_handle_message_receiver().await;

    tokio::spawn(receive_status(status_receiver));
    tokio::spawn(receive_handle_message(handle_message_receiver));

    let _ = client
        .get_connect(
            Some(WindowAppConnectInfo {
                current_ip: "192.168.200.194",
                broadcast_ip: "192.168.200.255",
                gateway_ip: "192.168.200.254",
                server_ip: "",
                port,
            }),
            db.clone(),
        )
        .await;
}

pub async fn receive_status(mut receiver: Receiver<SenderStatus>) {
    loop {
        let status = receiver.borrow_and_update().clone();
        println!("Status: {:?}", status);
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

pub async fn receive_handle_message(mut receiver: Receiver<Vec<u8>>) {
    loop {
        let message = receiver.borrow_and_update().clone();
        println!("Message: {:?}", message);
        if receiver.changed().await.is_err() {
            break;
        }
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
            println!("Failed to define ClientTable");
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
