use std::{
    env::current_dir,
    error::Error,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use atomic_websocket::{
    common::{get_id, make_response_message},
    external::native_db::{Builder, Database, Models},
    schema::{AppStartup, Category, Data, ServerConnectInfo},
    server_sender::{ClientOptions, SenderStatus, ServerSender, ServerSenderTrait},
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
    tokio::spawn(internal_client_start(port));

    // tokio::spawn(outer_client_start());

    loop {
        sleep(Duration::from_secs(100)).await;
    }
}

async fn outer_client_start() {
    let mut client_options = ClientOptions::default();
    client_options.url = "example.com/websocket".into();
    let atomic_client = AtomicWebsocket::get_outer_client(db().clone(), client_options).await;

    let status_receiver = atomic_client.get_status_receiver().await;
    let handle_message_receiver = atomic_client.get_handle_message_receiver().await;

    tokio::spawn(receive_status(status_receiver));
    tokio::spawn(receive_handle_message(handle_message_receiver));

    let _ = atomic_client.get_outer_connect(db().clone()).await;
}

async fn internal_client_start(port: &str) {
    let mut client_options = ClientOptions::default();
    client_options.retry_seconds = 2;
    client_options.use_keep_ip = true;
    let atomic_client = AtomicWebsocket::get_internal_client_with_server_sender(
        db().clone(),
        client_options,
        server_sender().clone(),
    )
    .await;

    let status_receiver = atomic_client.get_status_receiver().await;
    let handle_message_receiver = atomic_client.get_handle_message_receiver().await;

    tokio::spawn(receive_status(status_receiver));
    tokio::spawn(receive_handle_message(handle_message_receiver));

    let _ = atomic_client
        .get_internal_connect(
            Some(ServerConnectInfo {
                server_ip: "",
                port,
            }),
            db().clone(),
        )
        .await;
}

pub async fn receive_status(mut receiver: Receiver<SenderStatus>) {
    while let Ok(status) = receiver.recv().await {
        log::debug!("Status: {:?}", status);
        if status == SenderStatus::Disconnected {
            log::debug!("Disconnected");
        }
        if status == SenderStatus::Connected {
            log::debug!("Connected");
            let id = get_id(db().clone()).await;
            let mut datas = vec![];
            AppStartup {
                id: &id,
                app_type: 1,
            }
            .serialize(&mut datas)
            .unwrap();
            server_sender()
                .send(make_response_message(
                    atomic_websocket::schema::Category::AppStartup,
                    datas,
                ))
                .await;
        }
    }
}

pub async fn receive_handle_message(mut receiver: Receiver<Vec<u8>>) {
    while let Ok(message) = receiver.recv().await {
        if let Ok(data) = Data::deserialize(&message) {
            match Category::try_from(data.category as u32).unwrap() {
                Category::AppStartupOutput => {
                    log::debug!("{:?}", data);
                    sleep(Duration::from_secs(2)).await;
                    let id = get_id(db().clone()).await;
                    let mut datas = vec![];
                    AppStartup {
                        id: &id,
                        app_type: 1,
                    }
                    .serialize(&mut datas)
                    .unwrap();
                    server_sender()
                        .send(make_response_message(
                            atomic_websocket::schema::Category::AppStartup,
                            datas,
                        ))
                        .await;
                }
                _ => {
                    log::debug!("Unknown category: {:?}", data);
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

pub fn server_sender() -> &'static Arc<RwLock<ServerSender>> {
    static BUILDER: OnceLock<Arc<RwLock<ServerSender>>> = OnceLock::new();
    BUILDER.get_or_init(|| {
        Arc::new(RwLock::new(ServerSender::new(
            db().clone(),
            "".into(),
            ClientOptions::default(),
        )))
    })
}
