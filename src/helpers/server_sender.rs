use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use bebop::{Record, SliceWrapper};
use native_db::Database;
use tokio::{
    sync::{
        watch::{self, Receiver, Sender},
        RwLock,
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

use super::internal_client::ClientOptions;

#[derive(Clone, Debug, PartialEq)]
pub enum SenderStatus {
    Start,
    Connected,
    Disconnected,
}

pub struct ServerSender {
    sx: Option<Sender<Message>>,
    pub db: Arc<RwLock<Database<'static>>>,
    pub server_sender: Option<Arc<RwLock<ServerSender>>>,
    pub server_ip: String,
    pub server_send_times: i64,
    status_sx: Sender<SenderStatus>,
    handle_message_sx: Sender<Vec<u8>>,
    pub options: ClientOptions,
}

impl ServerSender {
    pub fn new(
        db: Arc<RwLock<Database<'static>>>,
        server_ip: String,
        options: ClientOptions,
    ) -> Self {
        let data = vec![0_u8];
        let mut buf = vec![];
        Data {
            category: 65535,
            datas: SliceWrapper::from_raw(&data),
        }
        .serialize(&mut buf)
        .unwrap();
        let (status_sx, _) = watch::channel(SenderStatus::Start);
        let (handle_message_sx, _) = watch::channel(buf);
        Self {
            sx: None,
            db,
            server_sender: None,
            server_ip,
            server_send_times: 0,
            status_sx,
            handle_message_sx,
            options,
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
    pub fn add(&mut self, sx: Sender<Message>, server_ip: String) {
        self.sx = Some(sx);
        self.server_ip = server_ip;
    }
    pub fn change_ip(&mut self, server_ip: String) {
        self.server_ip = server_ip;
    }
    pub async fn send_status(&self, status: SenderStatus) {
        let status_sx = self.status_sx.clone();
        let _ = status_sx.send(status);
    }
    pub async fn send_handle_message(&self, data: Vec<u8>) {
        let handle_message_sx = self.handle_message_sx.clone();
        let _ = handle_message_sx.send(data);
    }
    pub async fn send(&mut self, message: Message) {
        if let Some(sx) = &self.sx {
            let sender = sx.clone();
            match sender.send(message.clone()) {
                Ok(_) => {
                    self.server_send_times = now().timestamp();
                }
                Err(e) => {
                    log_error!("Error server sending message: {:?}", e);
                    self.send_status(SenderStatus::Disconnected).await;

                    let mut send_result = false;
                    let mut count = 0;
                    let limit_count = match self.options.retry_seconds > 5 {
                        true => 5,
                        false => match self.options.retry_seconds {
                            0 => 0,
                            _ => self.options.retry_seconds - 1,
                        },
                    };
                    while send_result == false {
                        sleep(Duration::from_secs(1)).await;
                        match sender.send(message.clone()) {
                            Ok(_) => {
                                send_result = true;
                                self.server_send_times = now().timestamp();
                            }
                            Err(e) => {
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
    async fn add(&self, sx: Sender<Message>, server_ip: String);
    fn send_status(&self, status: SenderStatus);
    fn send_handle_message(&self, data: Data<'_>);
    async fn get_status_receiver(&self) -> Receiver<SenderStatus>;
    async fn get_handle_message_receiver(&self) -> Receiver<Vec<u8>>;
    async fn send(&self, message: Message);
    async fn regist(&mut self, server_sender: Arc<RwLock<ServerSender>>);
    async fn is_valid_server_ip(&self) -> bool;
    async fn get_server_ip(&self) -> String;
}

#[async_trait]
impl ServerSenderTrait for Arc<RwLock<ServerSender>> {
    async fn add(&self, sx: Sender<Message>, server_ip: String) {
        let mut clone = self.write().await;
        clone.add(sx, server_ip.copy_string());

        log_debug!("set start server_ip: {:?}", server_ip);
        let server_connect_info = match get_setting_by_key(
            clone.db.clone(),
            format!("{:?}", SaveKey::ServerConnectInfo),
        )
        .await
        {
            Ok(server_connect_info) => server_connect_info,
            Err(error) => {
                log_debug!("Failed to get server_connect_info {error:?}");
                None
            }
        };
        match server_connect_info {
            Some(data) => {
                let mut data = ServerConnectInfo::deserialize(&data.value).unwrap();

                let mut before_value = Vec::new();
                data.serialize(&mut before_value).unwrap();

                data.server_ip = &server_ip;
                let mut value = Vec::new();
                data.serialize(&mut value).unwrap();

                let db = clone.db.read().await;
                let writer = db.rw_transaction().unwrap();
                writer
                    .update::<Settings>(
                        Settings {
                            key: format!("{:?}", SaveKey::ServerConnectInfo),
                            value: before_value,
                        },
                        Settings {
                            key: format!("{:?}", SaveKey::ServerConnectInfo),
                            value,
                        },
                    )
                    .unwrap();
                writer.commit().unwrap();
                drop(db);
            }
            None => {}
        }
    }

    async fn get_status_receiver(&self) -> Receiver<SenderStatus> {
        let clone = self.read().await;
        clone.get_status_receiver()
    }

    async fn get_handle_message_receiver(&self) -> Receiver<Vec<u8>> {
        let clone = self.read().await;
        clone.get_handle_message_receiver()
    }

    fn send_status(&self, status: SenderStatus) {
        let clone = self.clone();
        tokio::spawn(async move {
            clone.read().await.send_status(status).await;
        });
    }

    fn send_handle_message(&self, data: Data<'_>) {
        let clone = self.clone();

        let mut buf = Vec::new();
        data.serialize(&mut buf).unwrap();
        tokio::spawn(async move {
            let _ = clone.write().await.send_handle_message(buf).await;
        });
    }

    async fn send(&self, message: Message) {
        let clone = self.clone();
        clone.write().await.send(message).await;
        drop(clone);
    }

    async fn regist(&mut self, server_sender: Arc<RwLock<ServerSender>>) {
        let clone = self.clone();
        clone.write().await.regist(server_sender);
        drop(clone);
    }

    async fn is_valid_server_ip(&self) -> bool {
        let clone = self.read().await;
        let result = !clone.server_ip.is_empty();
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
}
