use std::error::Error;
use std::{sync::Arc, time::Duration};

use crate::generated::schema::{SaveKey, ServerConnectInfo};
use crate::helpers::get_outer_websocket::wrap_get_outer_websocket;
use crate::helpers::{
    common::{get_setting_by_key, make_ping_message},
    get_internal_websocket::{get_id, wrap_get_internal_websocket},
    server_sender::{SenderStatus, ServerSender, ServerSenderTrait},
    traits::{
        date_time::now,
        ip::{get_ip, GetIp},
        StringUtil,
    },
};
use crate::{dev_print, Settings};
use bebop::Record;
use native_db::Database;

use tokio::sync::watch::Receiver;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct ClientOptions {
    pub use_ping: bool,
    pub url: String,
    #[cfg(feature = "native_tls")]
    pub use_tls: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            use_ping: true,
            url: "".into(),
            #[cfg(feature = "native_tls")]
            use_tls: true,
        }
    }
}

pub struct AtomicClient {
    pub server_sender: Arc<RwLock<ServerSender>>,
    pub options: ClientOptions,
}

impl AtomicClient {
    pub async fn internal_initialize(&self, db: Arc<RwLock<Database<'static>>>) {
        self.regist_id(db.clone()).await;
        tokio::spawn(internal_ping_loop_cheker(
            self.server_sender.clone(),
            self.options.clone(),
        ));
    }

    pub async fn outer_initialize(&self, db: Arc<RwLock<Database<'static>>>) {
        self.regist_id(db.clone()).await;
        tokio::spawn(outer_ping_loop_cheker(
            self.server_sender.clone(),
            self.options.clone(),
        ));
    }

    pub async fn get_outer_connect(
        &self,
        db: Arc<RwLock<Database<'static>>>,
    ) -> Result<(), Box<dyn Error>> {
        get_outer_connect(db, self.server_sender.clone(), self.options.clone()).await
    }

    pub async fn get_internal_connect(
        &self,
        input: Option<ServerConnectInfo<'_>>,
        db: Arc<RwLock<Database<'static>>>,
    ) -> Result<(), Box<dyn Error>> {
        get_internal_connect(input, db, self.server_sender.clone(), self.options.clone()).await
    }

    pub async fn regist_id(&self, db: Arc<RwLock<Database<'static>>>) {
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
        if id.is_none() {
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

async fn internal_ping_loop_cheker(
    server_sender: Arc<RwLock<ServerSender>>,
    options: ClientOptions,
) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let mut server_sender_clone = server_sender.write().await;
        if server_sender_clone.server_send_times + 90 < now().timestamp()
            || server_sender_clone.server_ip.is_empty()
        {
            server_sender_clone.change_ip("".into());

            server_sender.send_status(SenderStatus::Disconnected);

            let server_connect_info = match get_setting_by_key(
                server_sender_clone.db.clone(),
                format!("{:?}", SaveKey::ServerConnectInfo),
            )
            .await
            {
                Ok(server_connect_info) => server_connect_info,
                Err(error) => {
                    log::error!("Failed to get server_connect_info {error:?}");
                    None
                }
            };
            if let Some(server_connect_info) = server_connect_info {
                let mut info = ServerConnectInfo::deserialize(&server_connect_info.value).unwrap();
                info.server_ip = "".into();
                let mut value = Vec::new();
                info.serialize(&mut value).unwrap();
                let db = server_sender_clone.db.read().await;
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
            drop(server_sender_clone);
            let server_sender_clone = server_sender.clone();
            let server_sender_clone2 = server_sender.clone();
            let options_clone = options.clone();
            tokio::spawn(async move {
                let server_sender_clone = server_sender_clone.read().await;
                let db = server_sender_clone.db.clone();
                drop(server_sender_clone);
                let _ = get_internal_connect(None, db, server_sender_clone2, options_clone).await;
                true
            });
        } else if server_sender_clone.server_send_times + 30 < now().timestamp() {
            log::debug!(
                "send: {:?}, current: {:?}",
                server_sender_clone.server_send_times,
                now().timestamp()
            );
            log::debug!("Try ping from loop checker");
            let id: String = get_id(server_sender_clone.db.clone()).await;
            server_sender_clone.send(make_ping_message(&id)).await;
            drop(server_sender_clone);
        }
        log::debug!("loop server checker finish");
    }
}

async fn outer_ping_loop_cheker(server_sender: Arc<RwLock<ServerSender>>, options: ClientOptions) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let mut server_sender_clone = server_sender.write().await;
        if server_sender_clone.server_send_times + 90 < now().timestamp()
            || server_sender_clone.server_ip.is_empty()
        {
            server_sender_clone.change_ip("".into());

            server_sender.send_status(SenderStatus::Disconnected);

            let server_sender_clone = server_sender.clone();
            let server_sender_clone2 = server_sender.clone();
            let options_clone = options.clone();
            tokio::spawn(async move {
                let server_sender_clone = server_sender_clone.read().await;
                let db = server_sender_clone.db.clone();
                drop(server_sender_clone);
                let _ = get_outer_connect(db, server_sender_clone2, options_clone).await;
                true
            });
        } else if server_sender_clone.server_send_times + 30 < now().timestamp() {
            log::debug!(
                "send: {:?}, current: {:?}",
                server_sender_clone.server_send_times,
                now().timestamp()
            );
            log::debug!("Try ping from loop checker");
            let id: String = get_id(server_sender_clone.db.clone()).await;
            server_sender_clone.send(make_ping_message(&id)).await;
            drop(server_sender_clone);
        }
        log::debug!("loop server checker finish");
    }
}

pub async fn get_outer_connect(
    db: Arc<RwLock<Database<'static>>>,
    server_sender: Arc<RwLock<ServerSender>>,
    options: ClientOptions,
) -> Result<(), Box<dyn Error>> {
    if server_sender.is_valid_server_ip().await {
        server_sender.send_status(SenderStatus::Connected);
        return Ok(());
    }
    let server_connect_info =
        get_setting_by_key(db.clone(), format!("{:?}", SaveKey::ServerConnectInfo)).await?;
    log::debug!("server_connect_info: {:?}", server_connect_info);

    if !options.url.is_empty() {
        server_sender
            .clone()
            .write()
            .await
            .change_ip(options.url.clone());
    }

    if options.url.is_empty() && !server_sender.is_valid_server_ip().await {
        server_sender.send_status(SenderStatus::Disconnected);
        return Ok(());
    }

    tokio::spawn(wrap_get_outer_websocket(db, server_sender, options));
    Ok(())
}

pub async fn get_internal_connect(
    input: Option<ServerConnectInfo<'_>>,
    db: Arc<RwLock<Database<'static>>>,
    server_sender: Arc<RwLock<ServerSender>>,
    options: ClientOptions,
) -> Result<(), Box<dyn Error>> {
    if server_sender.is_valid_server_ip().await {
        server_sender.send_status(SenderStatus::Connected);
        return Ok(());
    }
    let server_connect_info =
        get_setting_by_key(db.clone(), format!("{:?}", SaveKey::ServerConnectInfo)).await?;
    log::debug!("server_connect_info: {:?}", server_connect_info);

    if input.is_some() && server_connect_info.is_none() {
        let input = input.as_ref().unwrap();
        let db_clone = db.read().await;
        let writer = db_clone.rw_transaction()?;
        let mut value = Vec::new();
        ServerConnectInfo {
            current_ip: &input.current_ip,
            broadcast_ip: &input.broadcast_ip,
            gateway_ip: &input.gateway_ip,
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
        server_sender.send_status(SenderStatus::Disconnected);
        return Ok(());
    }

    let connect_info_data = match input.as_ref() {
        Some(info) => ServerConnectInfo {
            current_ip: &info.current_ip,
            broadcast_ip: &info.broadcast_ip,
            gateway_ip: &info.gateway_ip,
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

    if connect_info_data.current_ip.is_empty()
        || connect_info_data.broadcast_ip.is_empty()
        || connect_info_data.gateway_ip.is_empty()
    {
        server_sender.send_status(SenderStatus::Disconnected);
        return Ok(());
    }

    let mut info = connect_info_data.get_ip_range();
    let (exclude_ips, broadcast_ip, _) = info.get_data();

    match connect_info_data.server_ip {
        "" => {
            let mut ip = 1_u8;
            while ip <= broadcast_ip {
                if !exclude_ips.contains(&ip) {
                    info.set_server_ip(ip);
                    let server_url = info.get_full_server_ip();
                    let db_clone = db.clone();
                    let server_sender_clone = server_sender.clone();

                    tokio::spawn(wrap_get_internal_websocket(
                        db_clone,
                        server_sender_clone,
                        server_url.copy_string(),
                        options.clone(),
                    ));
                }
                if ip == 255 {
                    break;
                }
                ip += 1;
            }
        }
        _server_ip => {
            let ip = get_ip(_server_ip);
            info.set_server_ip(ip);
            let server_url = info.get_full_server_ip();
            let db_clone = db.clone();
            let server_sender_clone = server_sender.clone();
            tokio::spawn(wrap_get_internal_websocket(
                db_clone,
                server_sender_clone,
                server_url.copy_string(),
                options.clone(),
            ));
        }
    }
    Ok(())
}
