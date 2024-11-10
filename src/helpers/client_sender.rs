use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use bebop::Record;
use tokio::{
    sync::{
        mpsc::{self, Receiver, Sender},
        RwLock,
    },
    time::sleep,
};
use tokio_tungstenite::tungstenite::Message;

use crate::{
    helpers::{common::make_disconnect_message, traits::date_time::now},
    log_debug, log_error,
    schema::Data,
};

use super::{common::make_expired_output_message, traits::StringUtil};

pub struct ClientSenders {
    lists: Vec<ClientSender>,
    handle_message_sx: Sender<(Vec<u8>, String)>,
    handle_message_rx: Option<Receiver<(Vec<u8>, String)>>,
}

impl ClientSenders {
    pub fn new() -> Self {
        let (handle_message_sx, handle_message_rx) = mpsc::channel(1024);
        Self {
            lists: Vec::new(),
            handle_message_sx,
            handle_message_rx: Some(handle_message_rx),
        }
    }

    pub async fn add(&mut self, peer: &str, sx: Sender<Message>) {
        let list = self.lists.iter().position(|x| x.peer == peer);
        log_debug!("Add peer: {:?}, list: {:?}", peer, list);
        match list {
            Some(index) => {
                let list = self.lists.get_mut(index).unwrap();
                let _ = list.sx.send(make_disconnect_message(&peer)).await;
                list.sx = sx;
            }
            None => self.lists.push(ClientSender {
                peer: peer.into(),
                sx,
                send_time: 0,
            }),
        };
    }

    pub fn get_handle_message_receiver(&mut self) -> Receiver<(Vec<u8>, String)> {
        self.handle_message_rx
            .take()
            .expect("Receiver already taken")
    }

    pub async fn send_handle_message(&self, data: Vec<u8>, peer: &str) {
        let handle_message_sx = self.handle_message_sx.clone();
        let _ = handle_message_sx.send((data, peer.into())).await;
    }

    pub fn check_client_send_time(&mut self) {
        let now = now().timestamp();
        let mut remove_list = Vec::new();
        for client in self.lists.iter() {
            if client.send_time + 30 < now {
                remove_list.push(client.peer.copy_string());
            }
        }
        self.lists.retain(|x| !remove_list.contains(&x.peer));
    }

    pub fn remove(&mut self, peer: &str) {
        self.lists.retain(|x| x.peer != peer);
        log_debug!("Remove peer: {:?}", peer);
    }

    pub fn write_time(&mut self, peer: &str) {
        for client in self.lists.iter_mut() {
            if client.peer == peer {
                client.write_time();
            }
        }
    }

    pub async fn send(&self, peer: &str, message: Message) -> bool {
        for client in self.lists.iter() {
            if client.peer == peer {
                let sender = client.sx.clone();
                let mut backoff = Duration::from_millis(50); // 시작은 50ms로
                let max_backoff = Duration::from_secs(1); // 최대 1초
                let mut count = 0;

                loop {
                    match sender.send(message.clone()).await {
                        Ok(_) => return true,
                        Err(e) => {
                            if count > 5 {
                                log_error!("Failed to send after 5 retries: {:?}", e);
                                return false;
                            }

                            log_error!("Error sending message (attempt {}): {:?}", count + 1, e);
                            count += 1;

                            // Exponential backoff with max limit
                            backoff = std::cmp::min(backoff * 2, max_backoff);
                            sleep(backoff).await;
                        }
                    }
                }
            }
        }
        false
    }
    pub fn is_active(&self, peer: &str) -> bool {
        self.lists.iter().any(|x| x.peer == peer)
    }
}

#[async_trait]
pub trait ClientSendersTrait {
    async fn add(&self, peer: &str, sx: Sender<Message>);
    async fn get_handle_message_receiver(&self) -> Receiver<(Vec<u8>, String)>;
    async fn send_handle_message(&self, data: Data<'_>, peer: &str);
    async fn send(&self, peer: &str, message: Message) -> bool;
    async fn expire_send(&self, peer_list: Vec<String>);
    async fn is_active(&self, peer: &str) -> bool;
}

#[async_trait]
impl ClientSendersTrait for Arc<RwLock<ClientSenders>> {
    async fn add(&self, peer: &str, sx: Sender<Message>) {
        self.write().await.add(peer, sx).await;
    }

    async fn get_handle_message_receiver(&self) -> Receiver<(Vec<u8>, String)> {
        self.write().await.get_handle_message_receiver()
    }

    async fn send_handle_message(&self, data: Data<'_>, peer: &str) {
        let mut buf = Vec::new();
        data.serialize(&mut buf).unwrap();
        self.write().await.send_handle_message(buf, peer).await;
    }

    async fn send(&self, peer: &str, message: Message) -> bool {
        let result = self.read().await.send(peer, message).await;

        match result {
            true => self.write().await.write_time(peer),
            false => self.write().await.remove(peer),
        }
        result
    }

    async fn expire_send(&self, peer_list: Vec<String>) {
        for peer in self.read().await.lists.iter() {
            if !peer_list.contains(&peer.peer) {
                self.send(&peer.peer, make_expired_output_message()).await;
            }
        }
    }
    async fn is_active(&self, peer: &str) -> bool {
        self.read().await.is_active(peer)
    }
}

#[derive(Debug, Clone)]
struct ClientSender {
    peer: String,
    sx: Sender<Message>,
    send_time: i64,
}

impl ClientSender {
    pub fn write_time(&mut self) {
        self.send_time = now().timestamp();
    }
}
