use std::time::Duration;

use async_trait::async_trait;
use bebop::Record;
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    time::sleep,
};
use tokio_tungstenite::tungstenite::Message;

use crate::{
    generated::schema::{Data, SaveKey, ServerConnectInfo},
    helpers::{
        common::get_setting_by_key, get_internal_websocket::wrap_get_internal_websocket,
        traits::StringUtil,
    },
    log_debug, log_error, Settings,
};

use crate::helpers::traits::date_time::now;

use super::{
    common::make_disconnect_message,
    internal_client::ClientOptions,
    types::{RwServerSender, DB},
};

#[derive(Clone, Debug, PartialEq)]
pub enum SenderStatus {
    Start,
    Connected,
    Disconnected,
}

pub struct ServerSender {
    sx: Option<mpsc::Sender<Message>>,
    pub db: DB,
    pub server_sender: Option<RwServerSender>,
    pub server_ip: String,
    pub server_received_times: i64,
    status_tx: Sender<SenderStatus>,
    status_rx: Option<Receiver<SenderStatus>>,
    handle_message_tx: Sender<Vec<u8>>,
    handle_message_rx: Option<Receiver<Vec<u8>>>,
    pub options: ClientOptions,
    pub is_try_connect: bool,
}

impl ServerSender {
    pub fn new(db: DB, server_ip: String, options: ClientOptions) -> Self {
        let (status_tx, status_rx) = mpsc::channel(8);
        let (handle_message_tx, handle_message_rx) = mpsc::channel(8);

        Self {
            sx: None,
            db,
            server_sender: None,
            server_ip,
            server_received_times: 0,
            status_tx,
            status_rx: Some(status_rx), // 초기에 저장
            handle_message_tx,
            handle_message_rx: Some(handle_message_rx),
            options,
            is_try_connect: false,
        }
    }
    pub fn get_status_receiver(&mut self) -> Receiver<SenderStatus> {
        self.status_rx.take().expect("Receiver already taken")
    }
    pub fn get_handle_message_receiver(&mut self) -> Receiver<Vec<u8>> {
        self.handle_message_rx
            .take()
            .expect("Receiver already taken")
    }
    pub fn regist(&mut self, server_sender: RwServerSender) {
        self.server_sender = Some(server_sender);
    }
    fn sx_drop(&mut self) {
        if self.sx.is_some() {
            let sender = self.sx.clone().unwrap();
            let prev_server_ip = self.server_ip.copy_string();
            tokio::spawn(async move {
                let _ = sender.send(make_disconnect_message(&prev_server_ip)).await;
                sender.closed().await;
            });
            self.sx = None;
        }
    }
    pub fn add(&mut self, sx: mpsc::Sender<Message>, server_ip: &str) {
        self.sx_drop();
        self.sx = Some(sx);
        self.server_ip = server_ip.into();
    }
    pub fn remove_ip(&mut self) {
        if !self.server_ip.is_empty() {
            self.sx_drop();
            self.server_ip = "".into();
        }
    }
    pub fn send_status(&self, status: SenderStatus) {
        let status_sx = self.status_tx.clone();
        let _ = status_sx.try_send(status);
    }
    pub fn send_handle_message(&self, data: Vec<u8>) {
        let handle_message_tx = self.handle_message_tx.clone();
        let _ = handle_message_tx.try_send(data);
    }
    pub async fn send(&mut self, message: Message) {
        if let Some(sx) = &self.sx {
            let sender = sx.clone();
            let mut backoff = Duration::from_millis(50); // 시작은 50ms로
            let max_backoff = Duration::from_secs(1); // 최대 1초
            let mut count = 0;

            let limit_count = match self.options.retry_seconds > 5 {
                true => 5,
                false => match self.options.retry_seconds {
                    0 | 1 => 1,
                    _ => self.options.retry_seconds - 1,
                },
            };

            match sender.send(message.clone()).await {
                Ok(_) => return,
                Err(e) => {
                    log_error!("Initial send error: {:?}", e);
                }
            }

            loop {
                if count >= limit_count {
                    self.send_status(SenderStatus::Disconnected);

                    if let Some(server_sender) = &self.server_sender {
                        tokio::spawn(wrap_get_internal_websocket(
                            self.db.clone(),
                            server_sender.clone(),
                            self.server_ip.copy_string(),
                            self.options.clone(),
                        ));
                    }
                    break;
                }

                backoff = std::cmp::min(backoff * 2, max_backoff);
                sleep(backoff).await;

                count += 1;
                log_debug!("Retrying send (attempt {})", count + 1);

                match sx.clone().send(message.clone()).await {
                    Ok(_) => {
                        log_debug!("Send succeeded on retry {}", count);
                        return;
                    }
                    Err(e) => {
                        log_error!("Retry {} failed: {:?}", count, e);
                        continue;
                    }
                }
            }
        }
    }
}

#[async_trait]
pub trait ServerSenderTrait {
    async fn add(&self, sx: mpsc::Sender<Message>, server_ip: &str);
    async fn send_status(&self, status: SenderStatus);
    async fn send_handle_message(&self, data: Data<'_>);
    async fn get_status_receiver(&self) -> Receiver<SenderStatus>;
    async fn get_handle_message_receiver(&self) -> Receiver<Vec<u8>>;
    async fn send(&self, message: Message);
    async fn regist(&mut self, server_sender: RwServerSender);
    async fn is_valid_server_ip(&self) -> bool;
    async fn remove_ip(&self);
    async fn remove_ip_if_valid_server_ip(&self, server_ip: &str);
    async fn write_received_times(&self);
    async fn is_need_connect(&self, server_ip: &str) -> bool;
}

#[async_trait]
impl ServerSenderTrait for RwServerSender {
    async fn add(&self, sx: mpsc::Sender<Message>, server_ip: &str) {
        let mut clone = self.write().await;
        clone.add(sx, server_ip.into());
        let db = clone.db.clone();
        drop(clone);

        log_debug!("set start server_ip: {:?}", server_ip);
        let server_connect_info =
            match get_setting_by_key(db.clone(), format!("{:?}", SaveKey::ServerConnectInfo)).await
            {
                Ok(server_connect_info) => server_connect_info,
                Err(error) => {
                    log_debug!("Failed to get server_connect_info {error:?}");
                    None
                }
            };
        let db = db.lock().await;
        let writer = db.rw_transaction().unwrap();
        match server_connect_info {
            Some(before_data) => {
                let before_value = before_data.value.clone();
                let mut data = ServerConnectInfo::deserialize(&before_value).unwrap();

                data.server_ip = &server_ip;
                let mut value = Vec::new();
                data.serialize(&mut value).unwrap();

                writer.remove::<Settings>(before_data).unwrap();
                writer
                    .insert::<Settings>(Settings {
                        key: format!("{:?}", SaveKey::ServerConnectInfo),
                        value,
                    })
                    .unwrap();
            }
            None => {
                let mut value = Vec::new();
                let data = ServerConnectInfo {
                    server_ip,
                    port: match server_ip.contains(":") {
                        true => server_ip.split(":").nth(1).unwrap(),
                        false => "",
                    },
                };

                data.serialize(&mut value).unwrap();
                writer
                    .insert::<Settings>(Settings {
                        key: format!("{:?}", SaveKey::ServerConnectInfo),
                        value,
                    })
                    .unwrap();
            }
        }
        writer.commit().unwrap();
    }

    async fn get_status_receiver(&self) -> Receiver<SenderStatus> {
        self.write().await.get_status_receiver()
    }

    async fn get_handle_message_receiver(&self) -> Receiver<Vec<u8>> {
        self.write().await.get_handle_message_receiver()
    }

    async fn send_status(&self, status: SenderStatus) {
        self.read().await.send_status(status);
    }

    async fn send_handle_message(&self, data: Data<'_>) {
        let mut buf = Vec::new();
        data.serialize(&mut buf).unwrap();
        self.write().await.send_handle_message(buf);
    }

    async fn send(&self, message: Message) {
        self.write().await.send(message).await;
    }

    async fn regist(&mut self, server_sender: RwServerSender) {
        self.write().await.regist(server_sender);
    }

    async fn is_valid_server_ip(&self) -> bool {
        let clone = self.read().await;
        let result = !clone.server_ip.is_empty()
            && clone.server_received_times
                + (match clone.options.retry_seconds {
                    0 => 1,
                    _ => clone.options.retry_seconds as i64,
                } * 2)
                > now().timestamp();
        drop(clone);
        result
    }

    async fn remove_ip(&self) {
        self.write().await.remove_ip();
    }

    async fn remove_ip_if_valid_server_ip(&self, server_ip: &str) {
        let db = self.read().await.db.clone();
        let server_connect_info =
            match get_setting_by_key(db.clone(), format!("{:?}", SaveKey::ServerConnectInfo)).await
            {
                Ok(server_connect_info) => server_connect_info,
                Err(error) => {
                    log_error!("Failed to get server_connect_info {error:?}");
                    None
                }
            };
        if let Some(server_connect_info) = server_connect_info {
            let mut info = ServerConnectInfo::deserialize(&server_connect_info.value).unwrap();

            if info.server_ip == server_ip {
                self.remove_ip().await;
                info.server_ip = "".into();
                let mut value = Vec::new();
                info.serialize(&mut value).unwrap();
                let db = db.lock().await;
                let writer = db.rw_transaction().unwrap();
                writer
                    .update::<Settings>(
                        server_connect_info,
                        Settings {
                            key: format!("{:?}", SaveKey::ServerConnectInfo),
                            value,
                        },
                    )
                    .unwrap();
                writer.commit().unwrap();
                drop(db);
            }
        }
    }

    async fn write_received_times(&self) {
        self.write().await.server_received_times = now().timestamp();
    }

    async fn is_need_connect(&self, server_ip: &str) -> bool {
        let clone = self.read().await;
        server_ip != &clone.server_ip && !clone.is_try_connect
    }
}

#[test]
fn get_sercer_connect_info() {
    let binary: Vec<u8> = vec![0, 0, 0, 0, 5, 0, 0, 0, 49, 54, 50, 53, 48];
    let data = ServerConnectInfo::deserialize(&binary).unwrap();

    println!("{:?}", data);
}
