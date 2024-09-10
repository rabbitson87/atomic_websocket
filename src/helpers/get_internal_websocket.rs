use std::sync::Arc;

use bebop::Record;
use futures_util::{SinkExt, StreamExt};
use native_db::Database;
use std::time::Duration;
use tokio::{
    net::TcpStream,
    sync::{mpsc, RwLock},
    time::sleep,
};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

use crate::{
    generated::schema::{Category, Data, SaveKey},
    helpers::{
        common::make_ping_message,
        server_sender::{SenderStatus, ServerSender, ServerSenderTrait},
        traits::StringUtil,
    },
    log_debug, log_error, Settings,
};

use super::internal_client::ClientOptions;

pub async fn wrap_get_internal_websocket(
    db: Arc<RwLock<Database<'static>>>,
    server_sender: Arc<RwLock<ServerSender>>,
    server_ip: String,
    options: ClientOptions,
) -> bool {
    match get_internal_websocket(db, server_sender, server_ip, options).await {
        Ok(_) => true,
        Err(e) => {
            log_error!("Error getting websocket: {:?}", e);
            false
        }
    }
}

pub async fn get_internal_websocket(
    db: Arc<RwLock<Database<'static>>>,
    server_sender: Arc<RwLock<ServerSender>>,
    server_ip: String,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
    log_debug!("Connecting to {}", server_ip);
    if let Ok((ws_stream, _)) = connect_async(&server_ip).await {
        handle_websocket(
            db,
            server_sender,
            options,
            server_ip.copy_string(),
            ws_stream,
        )
        .await?;
    }
    log_debug!("Failed to server connect to {}", server_ip);
    Ok(())
}

pub async fn handle_websocket(
    db: Arc<RwLock<Database<'static>>>,
    server_sender: Arc<RwLock<ServerSender>>,
    options: ClientOptions,
    server_ip: String,
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> tokio_tungstenite::tungstenite::Result<()> {
    let (mut ostream, mut istream) = ws_stream.split();
    log_debug!("Connected to {} for web socket", server_ip);

    let (sx, mut rx) = mpsc::channel(8);
    let server_ip = server_ip.copy_string();
    let id = get_id(db.clone()).await;
    server_sender.add(sx.clone(), server_ip).await;

    if options.use_ping {
        log_debug!("Client send message: {:?}", make_ping_message(&id));
        server_sender.send(make_ping_message(&id)).await;
    }

    let mut is_first = true;
    tokio::spawn(async move {
        while let Some(Ok(Message::Binary(value))) = istream.next().await {
            if let Ok(data) = Data::deserialize(&value) {
                let id_clone = id.copy_string();
                log_debug!("Client receive message: {:?}", data);
                if data.category == Category::Pong as u16 {
                    if is_first {
                        is_first = false;
                        server_sender.send_status(SenderStatus::Connected);
                    }
                    let server_sender_clone = server_sender.clone();
                    tokio::spawn(async move {
                        sleep(Duration::from_secs(15)).await;
                        server_sender_clone.send(make_ping_message(&id_clone)).await;
                    });
                    continue;
                } else if data.category == Category::Disconnect as u16 {
                    break;
                }
                server_sender.send_handle_message(data);
            }
        }
    });

    while let Some(message) = rx.recv().await {
        match ostream.send(message.clone()).await {
            Ok(_) => {
                let data = message.into_data();
                if let Ok(data) = Data::deserialize(&data) {
                    log_debug!("Send message: {:?}", data);
                    if data.category == Category::Disconnect as u16 {
                        break;
                    }
                }
            }
            Err(e) => {
                log_error!("Error sending message: {:?}", e);
                break;
            }
        }
    }
    log_debug!("WebSocket closed");
    ostream.flush().await?;
    Ok(())
}

pub async fn get_id(db: Arc<RwLock<Database<'static>>>) -> String {
    let db = db.read().await;
    let reader = db.r_transaction().unwrap();
    let data = reader.scan().primary::<Settings>().unwrap();
    drop(reader);
    let mut id: Option<String> = None;
    for setting in data.all() {
        if let Ok(setting) = setting {
            if setting.key == format!("{:?}", SaveKey::ClientId) {
                id = Some(String::from_utf8(setting.value).unwrap().into());
                break;
            }
        }
    }
    id.unwrap()
}
