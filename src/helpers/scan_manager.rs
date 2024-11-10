use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::SinkExt;
use tokio::sync::RwLock;
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::{connect_async, tungstenite};

use crate::helpers::traits::connection_state::ConnectionManager;
use crate::log_debug;
use crate::server_sender::get_ip_address;

use super::common::make_disconnect_message;
use super::traits::StringUtil;

pub struct ConnectionState {
    pub status: WebSocketStatus,
    pub is_connecting: bool,
}

pub struct ScanManager {
    scan_ips: Vec<String>, // ConnectionRefused였던 IP들
    connection_states: Arc<RwLock<HashMap<String, ConnectionState>>>,
}

impl ScanManager {
    pub fn new(port: &str) -> Self {
        let mut scan_ips = Vec::new();
        let ip = get_ip_address();
        let ips = ip.split('.').collect::<Vec<&str>>();

        for sub_ip in 1..255 {
            let ip = format!("ws://{}.{}.{}.{}:{}", ips[0], ips[1], ips[2], sub_ip, port);
            scan_ips.push(ip);
        }

        Self {
            scan_ips,
            connection_states: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn is_connecting_allowed(&self, server_ip: &str) -> bool {
        // 이미 연결 시도 중인지 확인
        if let Some(state) = self.connection_states.read().await.get(server_ip) {
            if state.is_connecting {
                return false;
            }
        }
        // 연결된 상태인지 확인
        if self.connection_states.get_connected_ip().await.is_some() {
            return false;
        }
        true
    }

    async fn get_scannable_ips(&self) -> Vec<String> {
        let states = self.connection_states.read().await;

        self.scan_ips
            .iter()
            .filter(|server_ip| {
                if let Some(state) = states.get(*server_ip) {
                    if state.is_connecting {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect()
    }

    async fn scan_network(&mut self) {
        let scan_list: Vec<String> = self.get_scannable_ips().await;
        println!("scan_list: {:?}", scan_list);

        for server_ip in scan_list {
            if !self.is_connecting_allowed(&server_ip).await {
                continue;
            }

            let connection_states = self.connection_states.clone();
            let server_ip = server_ip.clone();
            tokio::spawn(async move {
                connection_states.start_connection(&server_ip).await;
                let status = check_connection(server_ip.copy_string()).await;
                log_debug!("server_ip: {}, {:?}", server_ip, status);
                connection_states.end_connection(&server_ip, status).await;
            });
        }
    }

    pub async fn run(&mut self) -> String {
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now(), Duration::from_secs(2));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            interval.tick().await;
            if let Some(server_ip) = self.connection_states.get_connected_ip().await {
                return server_ip;
            }
            self.scan_network().await;
        }
    }
}

async fn check_connection(server_ip: String) -> WebSocketStatus {
    match connect_async(&server_ip).await {
        Ok((mut ws_stream, _)) => {
            ws_stream
                .send(make_disconnect_message(&server_ip))
                .await
                .unwrap();
            ws_stream.flush().await.unwrap();
            // 연결 성공
            WebSocketStatus::Connected
        }
        Err(e) => {
            match e {
                tungstenite::Error::Io(e) => match e.kind() {
                    std::io::ErrorKind::ConnectionRefused => {
                        // 포트는 닫혔지만 호스트는 존재
                        WebSocketStatus::ConnectionRefused
                    }
                    _ => {
                        // 그 외 에러 (호스트가 없거나 네트워크 문제)
                        WebSocketStatus::Timeout
                    }
                },
                _ => WebSocketStatus::Timeout,
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum WebSocketStatus {
    Connecting,
    Connected,
    ConnectionRefused,
    Timeout,
}
