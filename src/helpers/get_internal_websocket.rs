use std::sync::{atomic::AtomicBool, Arc};

use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{sleep, timeout},
};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::{
    generated::schema::{Category, SaveKey},
    helpers::{
        common::{get_data_schema, make_disconnect_message, make_ping_message},
        server_sender::{SenderStatus, ServerSenderTrait},
        traits::{atomic::FlagAtomic, StringUtil},
    },
    log_debug, log_error, Settings,
};

use super::{
    internal_client::ClientOptions,
    types::{RwServerSender, DB},
};

pub async fn wrap_get_internal_websocket(
    db: DB,
    server_sender: RwServerSender,
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
    db: DB,
    server_sender: RwServerSender,
    server_ip: String,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
    log_debug!("Connecting to {}", server_ip);
    match timeout(
        Duration::from_secs(options.connect_timeout_seconds),
        connect_async(&server_ip),
    )
    .await
    {
        Ok(Ok((ws_stream, _))) => {
            handle_websocket(
                db,
                server_sender.clone(),
                options,
                server_ip.copy_string(),
                ws_stream,
            )
            .await?
        }
        Err(e) => {
            server_sender.remove_ip_if_valid_server_ip(&server_ip).await;
            log_error!("Error connecting to {}: {:?}", server_ip, e);
        }
        Ok(Err(e)) => {
            server_sender.remove_ip_if_valid_server_ip(&server_ip).await;
            log_error!("Error connecting to {}: {:?}", server_ip, e);
        }
    }
    log_debug!("Failed to server connect to {}", server_ip);
    Ok(())
}

pub async fn handle_websocket(
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
    server_ip: String,
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> tokio_tungstenite::tungstenite::Result<()> {
    let (mut ostream, mut istream) = ws_stream.split();
    log_debug!("Connected to {} for web socket", server_ip);

    let (sx, mut rx) = mpsc::channel(8);
    let id = get_id(db.clone()).await;
    server_sender.add(sx.clone(), &server_ip).await;

    if options.use_ping {
        log_debug!("Client send message: {:?}", make_ping_message(&id));
        server_sender.send(make_ping_message(&id)).await;
    }

    let retry_seconds = options.retry_seconds;
    let mut is_first = true;
    let server_sender_clone = server_sender.clone();
    let server_ip_clone = server_ip.copy_string();
    tokio::spawn(async move {
        let server_ip = server_ip_clone;
        let server_sender = server_sender_clone;
        let is_wait_ping = Arc::new(AtomicBool::new(false));

        while let Some(Ok(message)) = istream.next().await {
            let value = message.into_data();
            let data = match get_data_schema(&value) {
                Ok(data) => data,
                Err(e) => {
                    log_error!("Error getting data schema: {:?}", e);
                    continue;
                }
            };

            let id = id.copy_string();
            log_debug!("Client receive message: {:?}", data);
            if data.category == Category::Pong as u16 {
                if is_first {
                    is_first = false;
                    server_sender.send_status(SenderStatus::Connected).await;
                }
                if !is_wait_ping.is_true() {
                    is_wait_ping.set_bool(true);
                    server_sender.write_received_times().await;
                    let server_sender_clone = server_sender.clone();
                    let is_wait_ping_clone = is_wait_ping.clone();
                    tokio::spawn(async move {
                        sleep(Duration::from_secs(retry_seconds)).await;
                        server_sender_clone.send(make_ping_message(&id)).await;
                        is_wait_ping_clone.set_bool(false);
                    });
                }
                continue;
            } else if data.category == Category::Disconnect as u16 {
                let _ = sx
                    .send(make_disconnect_message(
                        server_ip
                            .split("://")
                            .nth(1)
                            .unwrap()
                            .split(":")
                            .nth(0)
                            .unwrap(),
                    ))
                    .await;
                break;
            }
            server_sender.send_handle_message(data).await;
        }
    });

    while let Some(message) = rx.recv().await {
        match ostream.send(message.clone()).await {
            Ok(_) => {
                let data = message.into_data();
                let data = match get_data_schema(&data) {
                    Ok(data) => data,
                    Err(e) => {
                        log_error!("Error getting data schema: {:?}", e);
                        rx.close();
                        break;
                    }
                };
                log_debug!("Send message: {:?}", data);
                if data.category == Category::Disconnect as u16 {
                    rx.close();
                    break;
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

pub async fn get_id(db: DB) -> String {
    let db = db.lock().await;
    let reader = db.r_transaction().unwrap();

    let mut return_string = String::new();
    if let Some(data) = reader
        .get()
        .primary::<Settings>(format!("{:?}", SaveKey::ClientId))
        .unwrap()
    {
        return_string = String::from_utf8(data.value).unwrap().into()
    }
    return_string
}
