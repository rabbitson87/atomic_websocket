use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use bebop::Record;
use tokio::{
    sync::{broadcast, mpsc::Sender, RwLock},
    time::sleep,
};
use tokio_tungstenite::tungstenite::Message;

use crate::{
    helpers::{common::make_disconnect_message, traits::date_time::now},
    log_debug, log_error,
    schema::Data,
};

use super::common::make_expired_output_message;

pub struct ClientSenders {
    lists: Vec<ClientSender>,
    handle_message_sx: broadcast::Sender<(Vec<u8>, String)>,
}

impl ClientSenders {
    pub fn new() -> Self {
        let (handle_message_sx, rx) = broadcast::channel(1024);
        drop(rx);
        Self {
            lists: Vec::new(),
            handle_message_sx,
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

    pub fn get_handle_message_receiver(&self) -> broadcast::Receiver<(Vec<u8>, String)> {
        self.handle_message_sx.subscribe()
    }

    pub fn send_handle_message(&self, data: Vec<u8>, peer: &str) {
        let handle_message_sx = self.handle_message_sx.clone();
        let _ = handle_message_sx.send((data, peer.into()));
    }

    pub fn check_client_send_time(&mut self) {
        let now = now().timestamp();
        let mut remove_list = Vec::new();
        for (index, client) in self.lists.iter().enumerate() {
            if client.send_time + 30 < now {
                remove_list.push(index);
            }
        }
        for index in remove_list {
            self.lists.remove(index);
        }
    }

    pub fn remove(&mut self, peer: &str) {
        let list = self.lists.iter().position(|x| x.peer == peer);
        log_debug!("Remove peer: {:?}, list: {:?}", peer, list);
        match list {
            Some(index) => {
                self.lists.remove(index);
            }
            None => {}
        };
    }

    pub async fn send(&mut self, peer: &str, message: Message) -> bool {
        let mut result = true;
        for client in self.lists.iter_mut() {
            if client.peer == peer {
                let sender = client.sx.clone();
                let mut count = 0;
                loop {
                    match sender.send(message.clone()).await {
                        Ok(_) => {
                            client.write_time();
                            break;
                        }
                        Err(e) => {
                            if count > 5 {
                                drop(sender);
                                result = false;
                                break;
                            }
                            log_error!("Error client sending message: {:?}", e);
                            count += 1;
                            sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
            }
        }
        result
    }
    pub fn is_active(&self, peer: &str) -> bool {
        self.lists.iter().any(|x| x.peer == peer)
    }
}

#[async_trait]
pub trait ClientSendersTrait {
    async fn add(&self, peer: &str, sx: Sender<Message>);
    async fn get_handle_message_receiver(&self) -> broadcast::Receiver<(Vec<u8>, String)>;
    async fn send_handle_message(&self, data: Data<'_>, peer: &str);
    async fn send(&self, peer: &str, message: Message);
    async fn expire_send(&self, peer_list: Vec<String>);
    async fn is_active(&self, peer: &str) -> bool;
}

#[async_trait]
impl ClientSendersTrait for Arc<RwLock<ClientSenders>> {
    async fn add(&self, peer: &str, sx: Sender<Message>) {
        self.write().await.add(peer, sx).await;
    }

    async fn get_handle_message_receiver(&self) -> broadcast::Receiver<(Vec<u8>, String)> {
        self.read().await.get_handle_message_receiver()
    }

    async fn send_handle_message(&self, data: Data<'_>, peer: &str) {
        let mut buf = Vec::new();
        data.serialize(&mut buf).unwrap();
        self.write().await.send_handle_message(buf, peer);
    }

    async fn send(&self, peer: &str, message: Message) {
        let mut clone = self.write().await;
        let result = clone.send(peer, message).await;

        if result == false {
            clone.remove(peer);
        }
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
