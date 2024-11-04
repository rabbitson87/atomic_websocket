use std::{
    env::current_dir,
    error::Error,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use atomic_websocket::{
    client_sender::{ClientSenders, ClientSendersTrait, ServerOptions},
    common::make_response_message,
    external::native_db::{Builder, Database, Models},
    schema::{AppStartupOutput, Category, Data},
    AtomicWebsocket, Settings,
};
use bebop::Record;
use tokio::{
    sync::{broadcast::Receiver, RwLock},
    time::sleep,
};

#[tokio::main]
async fn main() {
    std::fs::write(current_dir().unwrap().join("log/debug.log"), vec![]).unwrap();

    let config_str = include_str!("log_config.yml");
    let config = serde_yaml::from_str(config_str).unwrap();
    log4rs::init_raw_config(config).unwrap();

    let port = "9000";
    let address: String = format!("0.0.0.0:{}", port);

    log::debug!("Starting server on {}", address);
    tokio::spawn(server_start(address.clone()));

    loop {
        sleep(Duration::from_secs(100)).await;
    }
}

async fn server_start(address: String) {
    let option = ServerOptions::default();

    let atomic_server = AtomicWebsocket::get_internal_server_with_client_senders(
        address,
        option,
        client_senders().clone(),
    )
    .await;
    let handle_message_receiver = atomic_server.get_handle_message_receiver().await;

    tokio::spawn(receive_server_handle_message(handle_message_receiver));
}

pub async fn receive_server_handle_message(mut receiver: Receiver<(Vec<u8>, String)>) {
    while let Ok((data, peer)) = receiver.recv().await {
        // log::debug!("Message: {:?}", message);

        if let Ok(data) = Data::deserialize(&data) {
            match Category::try_from(data.category as u32).unwrap() {
                Category::AppStartup => {
                    log::debug!("peer: {} {:?}", peer, data);
                    let mut datas = vec![];
                    AppStartupOutput { success: true }
                        .serialize(&mut datas)
                        .unwrap();
                    client_senders()
                        .send(
                            &peer,
                            make_response_message(Category::AppStartupOutput, datas),
                        )
                        .await;
                }
                _ => {
                    log::debug!("peer: {} Unknown category: {:?}", peer, data);
                }
            }
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
            log::error!("Failed to define ClientTable");
        };
        models
    })
}

pub fn db() -> &'static Arc<RwLock<Database<'static>>> {
    static BUILDER: OnceLock<Arc<RwLock<Database<'static>>>> = OnceLock::new();
    BUILDER.get_or_init(|| {
        Arc::new(RwLock::new(
            Builder::new()
                .create(&make_models(), get_db_path().unwrap())
                .unwrap(),
        ))
    })
}

pub fn client_senders() -> &'static Arc<RwLock<ClientSenders>> {
    static BUILDER: OnceLock<Arc<RwLock<ClientSenders>>> = OnceLock::new();
    BUILDER.get_or_init(|| Arc::new(RwLock::new(ClientSenders::new())))
}
