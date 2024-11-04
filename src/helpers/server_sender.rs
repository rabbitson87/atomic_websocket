use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use bebop::Record;
use native_db::Database;
use tokio::{
    sync::{
        broadcast::{self, Receiver, Sender},
        mpsc, RwLock,
    },
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

use super::{common::make_disconnect_message, internal_client::ClientOptions};

#[derive(Clone, Debug, PartialEq)]
pub enum SenderStatus {
    Start,
    Connected,
    Disconnected,
}

pub struct ServerSender {
    sx: Option<mpsc::Sender<Message>>,
    pub db: Arc<RwLock<Database<'static>>>,
    pub server_sender: Option<Arc<RwLock<ServerSender>>>,
    pub server_ip: String,
    pub server_received_times: i64,
    status_sx: Sender<SenderStatus>,
    handle_message_sx: Sender<Vec<u8>>,
    pub options: ClientOptions,
    pub connect_list: Vec<String>,
}

impl ServerSender {
    pub fn new(
        db: Arc<RwLock<Database<'static>>>,
        server_ip: String,
        options: ClientOptions,
    ) -> Self {
        let (status_sx, _) = broadcast::channel(4);
        let (handle_message_sx, _) = broadcast::channel(4);
        Self {
            sx: None,
            db,
            server_sender: None,
            server_ip,
            server_received_times: 0,
            status_sx,
            handle_message_sx,
            options,
            connect_list: vec![],
        }
    }
    pub fn get_status_receiver(&self) -> Receiver<SenderStatus> {
        self.status_sx.subscribe()
    }
    pub fn get_handle_message_receiver(&self) -> Receiver<Vec<u8>> {
        self.handle_message_sx.subscribe()
    }
    pub fn regist(&mut self, server_sender: Arc<RwLock<ServerSender>>) {
        self.server_sender = Some(server_sender);
    }
    pub fn add(&mut self, sx: mpsc::Sender<Message>, server_ip: &str) {
        if self.sx.is_some() {
            let sender = self.sx.clone().unwrap();
            let server_ip = self.server_ip.copy_string();
            tokio::spawn(async move {
                let _ = sender.send(make_disconnect_message(&server_ip)).await;
                sender.closed().await;
            });
            self.sx = None;
        }
        self.sx = Some(sx);
        self.server_ip = server_ip.into();
    }
    pub fn change_ip(&mut self, server_ip: &str) {
        self.server_ip = server_ip.into();
    }
    pub fn send_status(&self, status: SenderStatus) {
        let status_sx = self.status_sx.clone();
        let _ = status_sx.send(status);
    }
    pub fn send_handle_message(&self, data: Vec<u8>) {
        let handle_message_sx = self.handle_message_sx.clone();
        let _ = handle_message_sx.send(data);
    }
    pub async fn send(&mut self, message: Message) {
        if let Some(sx) = &self.sx {
            let sender = sx.clone();
            match sender.send(message.clone()).await {
                Ok(_) => {}
                Err(e) => {
                    drop(sender);
                    log_error!("Error server sending message: {:?}", e);
                    self.send_status(SenderStatus::Disconnected);

                    let mut count = 0;
                    let limit_count = match self.options.retry_seconds > 5 {
                        true => 5,
                        false => match self.options.retry_seconds {
                            0 | 1 => 1,
                            _ => self.options.retry_seconds - 1,
                        },
                    };
                    loop {
                        let sender = sx.clone();
                        match sender.send(message.clone()).await {
                            Ok(_) => {
                                break;
                            }
                            Err(e) => {
                                drop(sender);
                                if count > limit_count {
                                    tokio::spawn(wrap_get_internal_websocket(
                                        self.db.clone(),
                                        self.server_sender.as_ref().unwrap().clone(),
                                        self.server_ip.copy_string(),
                                        self.options.clone(),
                                    ));
                                    break;
                                }
                                log_error!("Error server sending message: {:?}", e);
                                count += 1;
                                sleep(Duration::from_secs(1)).await;
                            }
                        };
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
    async fn regist(&mut self, server_sender: Arc<RwLock<ServerSender>>);
    async fn is_valid_server_ip(&self) -> bool;
    async fn get_server_ip(&self) -> String;
    async fn change_ip(&self, server_ip: &str);
    async fn change_ip_if_valid_server_ip(&self, server_ip: &str);
    async fn write_received_times(&self);
    async fn is_connect_list(&self, server_ip: &str) -> bool;
    async fn add_connect_list(&self, server_ip: &str);
    async fn remove_connect_list(&self, server_ip: &str);
    async fn is_start_connect(&self, server_ip: &str) -> bool;
}

#[async_trait]
impl ServerSenderTrait for Arc<RwLock<ServerSender>> {
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
        let db = db.read().await;
        let writer = db.rw_transaction().unwrap();
        match server_connect_info {
            Some(before_data) => {
                let mut data = ServerConnectInfo::deserialize(&before_data.value).unwrap();

                data.server_ip = &server_ip;
                let mut value = Vec::new();
                data.serialize(&mut value).unwrap();

                writer
                    .update::<Settings>(
                        before_data,
                        Settings {
                            key: format!("{:?}", SaveKey::ServerConnectInfo),
                            value,
                        },
                    )
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
        self.read().await.get_status_receiver()
    }

    async fn get_handle_message_receiver(&self) -> Receiver<Vec<u8>> {
        self.read().await.get_handle_message_receiver()
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

    async fn regist(&mut self, server_sender: Arc<RwLock<ServerSender>>) {
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

    async fn get_server_ip(&self) -> String {
        self.read()
            .await
            .server_ip
            .copy_string()
            .split("://")
            .nth(1)
            .unwrap()
            .split(":")
            .nth(0)
            .unwrap()
            .into()
    }

    async fn change_ip(&self, server_ip: &str) {
        self.write().await.change_ip(server_ip);
    }

    async fn change_ip_if_valid_server_ip(&self, server_ip: &str) {
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
                self.change_ip("".into()).await;
                info.server_ip = "".into();
                let mut value = Vec::new();
                info.serialize(&mut value).unwrap();
                let db = db.read().await;
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
            }
        }
    }

    async fn write_received_times(&self) {
        self.write().await.server_received_times = now().timestamp();
    }

    async fn is_connect_list(&self, server_ip: &str) -> bool {
        self.read()
            .await
            .connect_list
            .iter()
            .any(|x| x == server_ip)
    }

    async fn add_connect_list(&self, server_ip: &str) {
        self.write().await.connect_list.push(server_ip.into());
    }

    async fn remove_connect_list(&self, server_ip: &str) {
        self.write().await.connect_list.retain(|x| x != server_ip);
    }

    async fn is_start_connect(&self, server_ip: &str) -> bool {
        if self.is_connect_list(server_ip).await {
            return false;
        }
        self.add_connect_list(server_ip).await;
        true
    }
}
