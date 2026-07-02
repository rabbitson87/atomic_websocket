use std::{
    env::current_dir,
    error::Error,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use atomic_websocket::{
    common::{get_id, make_response_message, set_setting},
    external::native_db::{Builder, Models},
    schema::{AppStartup, AppStartupOutput, Category, Data, ServerConnectInfo},
    server_sender::{ClientOptions, SenderStatus, ServerSender, ServerSenderTrait},
    types::{save_key, RwServerSender, DB},
    AtomicWebsocket, Settings,
};
use bebop::Record;
use tokio::{
    sync::{mpsc::Receiver, Mutex, RwLock},
    time::sleep,
};

#[tokio::main]
async fn main() {
    std::fs::write(current_dir().unwrap().join("log/debug.log"), vec![]).unwrap();

    let config_str = include_str!("log_config.yml");
    let config = serde_yaml::from_str(config_str).unwrap();
    log4rs::init_raw_config(config).unwrap();

    // Parameters come in as CLI args (fixed-IP deployment — no auto discovery):
    //   test_client <server_ip> <port> <client_id>
    // Sensible defaults are used when an argument is omitted so the app always
    // starts, even if the database server cannot be reached.
    let args: Vec<String> = std::env::args().collect();
    let server_ip = args.get(1).cloned().unwrap_or_else(|| "192.168.0.10".to_string());
    let port = args.get(2).cloned().unwrap_or_else(|| "9000".to_string());
    let client_id = args
        .get(3)
        .cloned()
        .unwrap_or_else(|| "tablet-line1".to_string());

    log::debug!(
        "test_client config: server_ip={}, port={}, client_id={}",
        server_ip,
        port,
        client_id
    );

    // Persist the parameters to the database before connecting.
    save_client_config(&server_ip, &port, &client_id).await;

    tokio::spawn(internal_client_start(port));

    // tokio::spawn(outer_client_start());

    loop {
        sleep(Duration::from_secs(100)).await;
    }
}

/// Saves the connection parameters (client id + fixed server IP/port) to the
/// database. The library reads these on connect and dials the fixed IP directly
/// — no subnet scan, no internet dependency.
async fn save_client_config(server_ip: &str, port: &str, client_id: &str) {
    if let Err(e) = set_setting(
        db().clone(),
        Settings {
            key: save_key::CLIENT_ID.to_owned(),
            value: client_id.as_bytes().to_vec(),
        },
    )
    .await
    {
        log::error!("Failed to save client id: {:?}", e);
    }

    // Only store a server address when a fixed IP was provided. Without one the
    // app still runs; the operator can enter the IP (or press search) later.
    if server_ip.is_empty() {
        return;
    }

    let url = format!("ws://{}:{}", server_ip, port);
    let mut buf = Vec::new();
    if let Err(e) = (ServerConnectInfo {
        server_ip: &url,
        port,
    })
    .serialize(&mut buf)
    {
        log::error!("Failed to serialize ServerConnectInfo: {:?}", e);
        return;
    }
    if let Err(e) = set_setting(
        db().clone(),
        Settings {
            key: save_key::SERVER_CONNECT_INFO.to_owned(),
            value: buf,
        },
    )
    .await
    {
        log::error!("Failed to save server connect info: {:?}", e);
    }
}

#[allow(dead_code)]
async fn outer_client_start() {
    let mut client_options = ClientOptions::default();
    client_options.url = "example.com/websocket".into();
    let atomic_client = AtomicWebsocket::get_outer_client(db().clone(), client_options).await;

    if let Some(status_receiver) = atomic_client.get_status_receiver().await {
        tokio::spawn(receive_status(status_receiver));
    }
    if let Some(handle_message_receiver) = atomic_client.get_handle_message_receiver().await {
        tokio::spawn(receive_handle_message(handle_message_receiver));
    }

    let _ = atomic_client.get_outer_connect(db().clone()).await;
}

async fn internal_client_start(port: String) {
    let mut client_options = ClientOptions::default();
    client_options.retry_seconds = 2;
    client_options.use_keep_ip = true;
    let atomic_client = AtomicWebsocket::get_internal_client_with_server_sender(
        db().clone(),
        client_options,
        server_sender().clone(),
    )
    .await;

    if let Some(status_receiver) = atomic_client.get_status_receiver().await {
        tokio::spawn(receive_status(status_receiver));
    }
    if let Some(handle_message_receiver) = atomic_client.get_handle_message_receiver().await {
        tokio::spawn(receive_handle_message(handle_message_receiver));
    }

    // server_ip is read from the database (saved in `save_client_config`), so we
    // pass an empty server_ip here and only supply the port.
    let _ = atomic_client
        .get_internal_connect(
            Some(ServerConnectInfo {
                server_ip: "",
                port: &port,
            }),
            db().clone(),
        )
        .await;
}

pub async fn receive_status(mut receiver: Receiver<SenderStatus>) {
    while let Some(status) = receiver.recv().await {
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
    while let Some(message) = receiver.recv().await {
        if let Ok(data) = Data::deserialize(&message) {
            match Category::try_from(data.category as u32).unwrap() {
                Category::AppStartupOutput => {
                    log::debug!("{:?}", data);
                    log::debug!(
                        "AppStartupOutput: {:?}",
                        AppStartupOutput::deserialize(&data.datas).unwrap()
                    );
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

pub fn db() -> &'static DB {
    static BUILDER: OnceLock<DB> = OnceLock::new();
    BUILDER.get_or_init(|| {
        Arc::new(Mutex::new(
            Builder::new()
                .create(&make_models(), get_db_path().unwrap())
                .unwrap(),
        ))
    })
}

pub fn server_sender() -> &'static RwServerSender {
    static BUILDER: OnceLock<RwServerSender> = OnceLock::new();
    BUILDER.get_or_init(|| {
        Arc::new(RwLock::new(ServerSender::new(
            db().clone(),
            "".into(),
            ClientOptions::default(),
        )))
    })
}
