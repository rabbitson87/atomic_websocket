use std::error::Error;
use std::net::UdpSocket;
use std::time::Duration;

use crate::generated::schema::{SaveKey, ServerConnectInfo};
use crate::helpers::get_internal_websocket::handle_websocket;
use crate::helpers::get_outer_websocket::wrap_get_outer_websocket;
use crate::helpers::scan_manager::ScanManager;
use crate::helpers::{
    common::{get_setting_by_key, make_ping_message},
    get_internal_websocket::{get_id, wrap_get_internal_websocket},
    server_sender::{SenderStatus, ServerSenderTrait},
    traits::date_time::now,
};
use crate::{log_debug, log_error, Settings};
use bebop::Record;

use tokio::sync::mpsc::Receiver;
use tokio::time::{Instant, MissedTickBehavior};

use super::types::{RwServerSender, DB};

#[derive(Clone)]
pub struct ClientOptions {
    pub use_ping: bool,
    pub url: String,
    pub retry_seconds: u64,
    pub use_keep_ip: bool,
    pub connect_timeout_seconds: u64,
    #[cfg(feature = "rustls")]
    pub use_tls: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            use_ping: true,
            url: "".into(),
            retry_seconds: 30,
            use_keep_ip: false,
            connect_timeout_seconds: 3,
            #[cfg(feature = "rustls")]
            use_tls: true,
        }
    }
}

pub struct AtomicClient {
    pub server_sender: RwServerSender,
    pub options: ClientOptions,
}

impl AtomicClient {
    pub async fn internal_initialize(&self, db: DB) {
        self.regist_id(db.clone()).await;
        tokio::spawn(internal_ping_loop_cheker(
            self.server_sender.clone(),
            self.options.clone(),
        ));
    }

    pub async fn outer_initialize(&self, db: DB) {
        #[cfg(feature = "rustls")]
        self.initial_rustls();
        self.regist_id(db.clone()).await;
        tokio::spawn(outer_ping_loop_cheker(
            self.server_sender.clone(),
            self.options.clone(),
        ));
    }

    pub async fn get_outer_connect(&self, db: DB) -> Result<(), Box<dyn Error>> {
        get_outer_connect(db, self.server_sender.clone(), self.options.clone()).await
    }

    pub async fn get_internal_connect(
        &self,
        input: Option<ServerConnectInfo<'_>>,
        db: DB,
    ) -> Result<(), Box<dyn Error>> {
        get_internal_connect(input, db, self.server_sender.clone(), self.options.clone()).await
    }

    #[cfg(feature = "rustls")]
    pub fn initial_rustls(&self) {
        use rustls::crypto::{ring, CryptoProvider};
        if let None = CryptoProvider::get_default() {
            let provider = ring::default_provider();
            provider.install_default().unwrap();
        }
    }

    pub async fn regist_id(&self, db: DB) {
        let db = db.lock().await;
        let reader = db.r_transaction().unwrap();
        let data = reader
            .get()
            .primary::<Settings>(format!("{:?}", SaveKey::ClientId))
            .unwrap();
        drop(reader);
        if let None = data {
            use nanoid::nanoid;
            let writer = db.rw_transaction().unwrap();
            writer
                .insert::<Settings>(Settings {
                    key: format!("{:?}", SaveKey::ClientId),
                    value: nanoid!().as_bytes().to_vec(),
                })
                .unwrap();
            writer.commit().unwrap();
        }
        drop(db);
    }

    pub async fn get_status_receiver(&self) -> Receiver<SenderStatus> {
        self.server_sender.get_status_receiver().await
    }

    pub async fn get_handle_message_receiver(&self) -> Receiver<Vec<u8>> {
        self.server_sender.get_handle_message_receiver().await
    }
}

async fn internal_ping_loop_cheker(server_sender: RwServerSender, options: ClientOptions) {
    let retry_seconds = options.retry_seconds;
    let use_keep_ip = options.use_keep_ip;
    let mut interval = tokio::time::interval_at(
        Instant::now() + Duration::from_secs(retry_seconds),
        Duration::from_secs(retry_seconds),
    );
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        let server_sender_read = server_sender.read().await;
        if server_sender_read.server_received_times > 0
            && server_sender_read.server_received_times + (retry_seconds as i64 * 4)
                < now().timestamp()
        {
            drop(server_sender_read);
            server_sender.send_status(SenderStatus::Disconnected).await;
            if !use_keep_ip {
                server_sender.remove_ip().await;
                let db = server_sender.read().await.db.clone();
                let server_connect_info = match get_setting_by_key(
                    db.clone(),
                    format!("{:?}", SaveKey::ServerConnectInfo),
                )
                .await
                {
                    Ok(server_connect_info) => server_connect_info,
                    Err(error) => {
                        log_error!("Failed to get server_connect_info {error:?}");
                        None
                    }
                };
                if let Some(server_connect_info) = server_connect_info {
                    let mut info =
                        ServerConnectInfo::deserialize(&server_connect_info.value).unwrap();
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
                }
                drop(db);
            }
            let db = server_sender.read().await.db.clone();
            let server_sender = server_sender.clone();
            let options = options.clone();
            tokio::spawn(async move {
                let _ = get_internal_connect(None, db, server_sender, options).await;
                true
            });
        } else if server_sender_read.server_received_times + (retry_seconds as i64 * 2)
            < now().timestamp()
        {
            log_debug!(
                "send: {:?}, current: {:?}",
                server_sender_read.server_received_times,
                now().timestamp()
            );
            log_debug!("Try ping from loop checker");
            let id: String = get_id(server_sender_read.db.clone()).await;
            drop(server_sender_read);
            server_sender.send(make_ping_message(&id)).await;
        }
        log_debug!("loop server checker finish");
    }
}

async fn outer_ping_loop_cheker(server_sender: RwServerSender, options: ClientOptions) {
    let retry_seconds = options.retry_seconds;
    let use_keep_ip = options.use_keep_ip;
    let mut interval = tokio::time::interval_at(
        Instant::now() + Duration::from_secs(retry_seconds),
        Duration::from_secs(retry_seconds),
    );
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        let server_sender_read = server_sender.read().await;
        if server_sender_read.server_received_times + 90 < now().timestamp() {
            drop(server_sender_read);
            server_sender.send_status(SenderStatus::Disconnected).await;

            if !use_keep_ip {
                server_sender.remove_ip().await;
            }

            let server_sender = server_sender.clone();
            let options = options.clone();
            let db = server_sender.read().await.db.clone();
            tokio::spawn(async move {
                let _ = get_outer_connect(db, server_sender, options).await;
                true
            });
        } else if server_sender_read.server_received_times + 30 < now().timestamp() {
            log_debug!(
                "send: {:?}, current: {:?}",
                server_sender_read.server_received_times,
                now().timestamp()
            );
            log_debug!("Try ping from loop checker");
            let id: String = get_id(server_sender_read.db.clone()).await;
            drop(server_sender_read);
            server_sender.send(make_ping_message(&id)).await;
        }
        log_debug!("loop server checker finish");
    }
}

pub async fn get_outer_connect(
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
) -> Result<(), Box<dyn Error>> {
    if server_sender.is_valid_server_ip().await {
        server_sender.send_status(SenderStatus::Connected).await;
        return Ok(());
    }
    let server_connect_info =
        get_setting_by_key(db.clone(), format!("{:?}", SaveKey::ServerConnectInfo)).await?;
    log_debug!("server_connect_info: {:?}", server_connect_info);

    if options.url.is_empty() && !server_sender.is_valid_server_ip().await {
        server_sender.send_status(SenderStatus::Disconnected).await;
        return Ok(());
    }

    tokio::spawn(wrap_get_outer_websocket(db, server_sender, options));
    Ok(())
}

pub async fn get_internal_connect(
    input: Option<ServerConnectInfo<'_>>,
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
) -> Result<(), Box<dyn Error>> {
    if server_sender.read().await.is_try_connect {
        return Ok(());
    }
    if server_sender.is_valid_server_ip().await {
        server_sender.send_status(SenderStatus::Connected).await;
        return Ok(());
    }
    let server_connect_info =
        get_setting_by_key(db.clone(), format!("{:?}", SaveKey::ServerConnectInfo)).await?;
    log_debug!("server_connect_info: {:?}", server_connect_info);

    if input.is_some() && server_connect_info.is_none() {
        let input = input.as_ref().unwrap();
        let db_clone = db.lock().await;
        let writer = db_clone.rw_transaction()?;
        let mut value = Vec::new();
        ServerConnectInfo {
            server_ip: "".into(),
            port: &input.port,
        }
        .serialize(&mut value)?;
        writer.insert::<Settings>(Settings {
            key: format!("{:?}", SaveKey::ServerConnectInfo),
            value,
        })?;
        writer.commit()?;
        drop(db_clone);
    }

    if input.is_none() && server_connect_info.is_none() {
        server_sender.send_status(SenderStatus::Disconnected).await;
        return Ok(());
    }

    let connect_info_data = match input.as_ref() {
        Some(info) => ServerConnectInfo {
            server_ip: match server_connect_info.as_ref() {
                Some(server_connect_info) => {
                    ServerConnectInfo::deserialize(&server_connect_info.value)
                        .unwrap()
                        .server_ip
                }
                None => "".into(),
            },
            port: &info.port,
        },
        None => {
            ServerConnectInfo::deserialize(&server_connect_info.as_ref().unwrap().value).unwrap()
        }
    };

    let ip = get_ip_address();

    if ip.is_empty() {
        server_sender.send_status(SenderStatus::Disconnected).await;
        return Ok(());
    }

    match connect_info_data.server_ip {
        "" => {
            server_sender.write().await.is_try_connect = true;

            let (server_ip, ws_stream) = ScanManager::new(connect_info_data.port).run().await;
            tokio::spawn(async move {
                if let Err(error) =
                    handle_websocket(db, server_sender.clone(), options, server_ip, ws_stream).await
                {
                    log_error!("Error handling websocket: {:?}", error);
                    server_sender.write().await.is_try_connect = false;
                }
            });
        }
        _server_ip => {
            tokio::spawn(wrap_get_internal_websocket(
                db.clone(),
                server_sender.clone(),
                _server_ip.into(),
                options.clone(),
            ));
        }
    };

    Ok(())
}

pub fn get_ip_address() -> String {
    let socket = UdpSocket::bind("0.0.0.0:0");
    let socket = match socket {
        Ok(socket) => socket,
        Err(_) => return "".into(),
    };
    match socket.connect("8.8.8.8:80") {
        Ok(_) => {}
        Err(_) => return "".into(),
    };
    let addr = match socket.local_addr() {
        Ok(addr) => addr,
        Err(_) => return "".into(),
    };
    addr.ip().to_string()
}
