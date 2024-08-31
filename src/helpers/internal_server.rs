use std::{net::SocketAddr, sync::Arc, time::Duration};

use tokio::{
    self,
    net::{TcpListener, TcpStream},
    sync::{watch, RwLock},
};
use tokio_tungstenite::{tungstenite::Error, WebSocketStream};

use crate::{
    dev_print,
    helpers::{
        client_sender::ClientSendersTrait,
        common::{make_disconnect_message, make_pong_message},
        traits::StringUtil,
    },
    schema::{Category, Data, Ping},
};
use bebop::Record;
use futures_util::{stream::SplitStream, SinkExt, StreamExt};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio_tungstenite::{
    accept_async,
    tungstenite::{self, Message},
};

use super::client_sender::ClientSenders;

pub struct AtomicServer {
    pub client_senders: Arc<RwLock<ClientSenders>>,
}

#[derive(Clone)]
pub struct ServerOptions {
    pub use_ping: bool,
    pub proxy_ping: i16,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            use_ping: true,
            proxy_ping: -1,
        }
    }
}

impl AtomicServer {
    pub async fn new(addr: &str, option: ServerOptions) -> Self {
        let listener = TcpListener::bind(&addr).await.expect("Can't listen");
        let client_senders = Arc::new(RwLock::new(ClientSenders::new()));
        tokio::spawn(handle_accept(listener, client_senders.clone(), option));

        tokio::spawn(loop_client_checker(client_senders.clone()));
        Self { client_senders }
    }

    pub async fn get_handle_message_receiver(&self) -> watch::Receiver<(Vec<u8>, String)> {
        self.client_senders.get_handle_message_receiver().await
    }
}

pub async fn loop_client_checker(server_sender: Arc<RwLock<ClientSenders>>) {
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let server_sender_clone = server_sender.clone();
        let mut server_sender_clone = server_sender_clone.write().await;
        server_sender_clone.check_client_send_time();
        drop(server_sender_clone);
        dev_print!("loop client cheker finish");
    }
}

pub async fn handle_accept(
    listener: TcpListener,
    client_senders: Arc<RwLock<ClientSenders>>,
    option: ServerOptions,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let peer = stream
                    .peer_addr()
                    .expect("connected streams should have a peer address");
                dev_print!("Peer address: {}", peer);
                tokio::spawn(accept_connection(
                    client_senders.clone(),
                    peer,
                    stream,
                    option.clone(),
                ));
            }
            Err(e) => {
                dev_print!("Error accepting connection: {:?}", e);
            }
        }
    }
}

pub async fn accept_connection(
    client_senders: Arc<RwLock<ClientSenders>>,
    peer: SocketAddr,
    stream: TcpStream,
    option: ServerOptions,
) {
    if let Err(e) = handle_connection(client_senders, peer, stream, option).await {
        match e {
            Error::ConnectionClosed | Error::Protocol(_) | Error::Utf8 => (),
            err => println!("Error processing connection: {}", err),
        }
    }
}

pub async fn handle_connection(
    client_senders: Arc<RwLock<ClientSenders>>,
    peer: SocketAddr,
    stream: TcpStream,
    option: ServerOptions,
) -> tungstenite::Result<()> {
    match accept_async(stream).await {
        Ok(ws_stream) => {
            dev_print!("New WebSocket connection: {}", peer);
            let (mut ostream, mut istream) = ws_stream.split();

            let (sx, mut rx): (Sender<Message>, Receiver<Message>) = mpsc::channel(1024);
            tokio::spawn(async move {
                let use_ping = option.use_ping;
                let id = get_id_from_first_message(
                    &mut istream,
                    client_senders.clone(),
                    sx.clone(),
                    option,
                )
                .await;

                match id {
                    Some(id) => {
                        drop(sx);
                        while let Some(Ok(Message::Binary(value))) = istream.next().await {
                            if let Ok(data) = Data::deserialize(&value) {
                                if data.category == Category::Ping as u16 && use_ping {
                                    if let Ok(data) = Ping::deserialize(&data.datas) {
                                        client_senders
                                            .send(data.peer.into(), make_pong_message())
                                            .await;
                                        continue;
                                    }
                                }
                                if data.category == Category::Disconnect as u16 {
                                    break;
                                }
                                client_senders.send_handle_message(data, id.copy_string());
                            }
                        }
                    }
                    None => {
                        let _ = sx.send(make_disconnect_message(&peer.to_string())).await;
                    }
                }
            });

            while let Some(message) = rx.recv().await {
                ostream.send(message.clone()).await?;
                let data = message.into_data();
                if let Ok(data) = Data::deserialize(&data) {
                    dev_print!("Server sending message: {:?}", data);
                    if data.category == Category::Disconnect as u16 {
                        break;
                    }
                }
            }
            dev_print!("client: {} disconnected", peer);
            ostream.flush().await?;
        }
        Err(e) => {
            dev_print!("Error accepting WebSocket connection: {:?}", e);
        }
    }

    Ok(())
}

async fn get_id_from_first_message(
    istream: &mut SplitStream<WebSocketStream<TcpStream>>,
    client_senders: Arc<RwLock<ClientSenders>>,
    sx: Sender<Message>,
    options: ServerOptions,
) -> Option<String> {
    let mut _id: Option<String> = None;
    if let Some(Ok(Message::Binary(value))) = istream.next().await {
        if let Ok(mut data) = Data::deserialize(&value) {
            if data.category == Category::Ping as u16 {
                dev_print!("receive ping from client: {:?}", data);
                if let Ok(ping) = Ping::deserialize(&data.datas) {
                    _id = Some(ping.peer.into());
                    client_senders
                        .add(_id.as_ref().unwrap().copy_string(), sx)
                        .await;
                    if options.use_ping {
                        client_senders
                            .send(_id.as_ref().unwrap().copy_string(), make_pong_message())
                            .await;
                    } else {
                        if options.proxy_ping > 0 {
                            data.category = options.proxy_ping as u16;
                        }
                        client_senders
                            .send_handle_message(data, _id.as_ref().unwrap().copy_string());
                    }
                }
            }
        }
    }
    _id
}
