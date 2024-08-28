use std::sync::Arc;

use bebop::Record;
use futures_util::{SinkExt, StreamExt};
use native_db::Database;
use std::time::Duration;
use tokio::{
    sync::{watch, RwLock},
    time::sleep,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{
    dev_print,
    generated::schema::{Category, Data, SaveKey},
    helpers::{
        common::make_ping_message,
        server_sender::{SenderStatus, ServerSender, ServerSenderTrait},
        traits::StringUtil,
    },
    Settings,
};

pub async fn wrap_get_websocket(
    db: Arc<RwLock<Database<'static>>>,
    server_sender: Arc<RwLock<ServerSender>>,
    server_ip: String,
) -> bool {
    match get_websocket(db, server_sender, server_ip).await {
        Ok(_) => true,
        Err(e) => {
            println!("Error getting websocket: {:?}", e);
            false
        }
    }
}

pub async fn get_websocket(
    db: Arc<RwLock<Database<'static>>>,
    server_sender: Arc<RwLock<ServerSender>>,
    server_ip: String,
) -> tokio_tungstenite::tungstenite::Result<()> {
    dev_print!("Connecting to {}", server_ip);
    if let Ok((ws_stream, _)) = connect_async(&server_ip).await {
        let (mut ostream, mut istream) = ws_stream.split();
        dev_print!("Connected to {} for web socket", server_ip);

        let (sx, mut rx) = watch::channel(Message::Binary(vec![0u8]));
        let server_ip = server_ip.copy_string();
        let id = get_id(db.clone()).await;
        server_sender.add(sx, server_ip).await;

        dev_print!("Client send message: {:?}", make_ping_message(&id));
        server_sender.send(make_ping_message(&id)).await;

        server_sender.send_status(SenderStatus::Connected);
        tokio::spawn(async move {
            while let Some(Ok(Message::Binary(value))) = istream.next().await {
                let id_clone = id.copy_string();
                let server_sender_clone = server_sender.clone();
                if let Ok(data) = Data::deserialize(&value) {
                    dev_print!("Client receive message: {:?}", data);
                    if data.category == Category::Pong as u16 {
                        tokio::spawn(async move {
                            sleep(Duration::from_secs(15)).await;
                            server_sender_clone.send(make_ping_message(&id_clone)).await;
                        });
                    }
                    if data.category == Category::Disconnect as u16 {
                        break;
                    }
                    server_sender.send_handle_message(data);
                }
            }
        });

        loop {
            let message = rx.borrow_and_update().clone();

            match ostream.send(message.clone()).await {
                Ok(_) => {
                    let data = message.into_data();
                    if let Ok(data) = Data::deserialize(&data) {
                        println!("Send message: {:?}", data);
                        if data.category == Category::Disconnect as u16 {
                            break;
                        }
                    }
                }
                Err(e) => {
                    println!("Error sending message: {:?}", e);
                    break;
                }
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
        println!("WebSocket closed");
        ostream.flush().await?;
    }
    dev_print!("Failed to server connect to {}", server_ip);
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
