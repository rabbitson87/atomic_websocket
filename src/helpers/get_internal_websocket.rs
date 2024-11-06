use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use native_db::Database;
use std::time::Duration;
use tokio::{
    net::TcpStream,
    sync::{mpsc, Mutex, RwLock},
    time::{sleep, timeout},
};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::{
    generated::schema::{Category, SaveKey},
    helpers::{
        common::{get_data_schema, make_disconnect_message, make_ping_message},
        server_sender::{SenderStatus, ServerSender, ServerSenderTrait},
        traits::{ping::PingCheck, StringUtil},
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
    if !server_sender.is_start_connect(&server_ip).await {
        return Ok(());
    }
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
            server_sender.change_ip_if_valid_server_ip(&server_ip).await;
            log_error!("Error connecting to {}: {:?}", server_ip, e);
        }
        Ok(Err(e)) => {
            server_sender.change_ip_if_valid_server_ip(&server_ip).await;
            log_error!("Error connecting to {}: {:?}", server_ip, e);
        }
    }
    server_sender.remove_connect_list(&server_ip).await;
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
    let id = get_id(db.clone()).await;
    server_sender.add(sx.clone(), &server_ip).await;

    if options.use_ping {
        log_debug!("Client send message: {:?}", make_ping_message(&id));
        server_sender.send(make_ping_message(&id)).await;
    }

    let retry_seconds = options.retry_seconds;
    let mut is_first = true;
    let server_sender_clone = server_sender.clone();
    tokio::spawn(async move {
        let is_wait_ping = Arc::new(Mutex::new(false));
        while let Some(Ok(message)) = istream.next().await {
            let value = message.into_data();
            let data = get_data_schema(&value);

            let id_clone = id.copy_string();
            log_debug!("Client receive message: {:?}", data);
            if data.category == Category::Pong as u16 {
                if is_first {
                    is_first = false;
                    server_sender_clone
                        .send_status(SenderStatus::Connected)
                        .await;
                }
                if !is_wait_ping.is_wait_ping().await {
                    is_wait_ping.set_wait_ping(true).await;
                    server_sender_clone.write_received_times().await;
                    let server_sender_clone2 = server_sender_clone.clone();
                    let is_wait_ping_clone = is_wait_ping.clone();
                    tokio::spawn(async move {
                        sleep(Duration::from_secs(retry_seconds)).await;
                        server_sender_clone2
                            .send(make_ping_message(&id_clone))
                            .await;
                        is_wait_ping_clone.set_wait_ping(false).await;
                    });
                }
                continue;
            } else if data.category == Category::Disconnect as u16 {
                let _ = sx
                    .send(make_disconnect_message(
                        &server_sender_clone.get_server_ip().await,
                    ))
                    .await;
                break;
            }
            server_sender_clone.send_handle_message(data).await;
        }
    });

    while let Some(message) = rx.recv().await {
        match ostream.send(message.clone()).await {
            Ok(_) => {
                let data = message.into_data();
                let data = get_data_schema(&data);
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
    server_sender.remove_connect_list(&server_ip).await;
    ostream.flush().await?;
    Ok(())
}

pub async fn get_id(db: Arc<RwLock<Database<'static>>>) -> String {
    let db = db.read().await;
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
