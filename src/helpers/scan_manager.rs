use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures_util::SinkExt;
use tokio_tungstenite::{connect_async, tungstenite};

use crate::log_debug;
use crate::server_sender::get_ip_address;

use super::common::make_disconnect_message;
use super::traits::StringUtil;

pub struct ScanManager {
    quick_scan_ips: Vec<String>, // ConnectionRefused였던 IP들
    slow_scan_ips: Vec<String>,  // 그 외 IP들
    last_scan_times: HashMap<String, Instant>,
    quick_interval: Duration, // 2초
    slow_interval: Duration,  // 30초
}

impl ScanManager {
    pub fn new(port: &str) -> Self {
        let mut quick_scan_ips = Vec::new();
        let ip = get_ip_address();
        let ips = ip.split('.').collect::<Vec<&str>>();

        for sub_ip in 1..255 {
            let ip = format!("ws://{}.{}.{}.{}:{}", ips[0], ips[1], ips[2], sub_ip, port);
            quick_scan_ips.push(ip);
        }

        Self {
            quick_scan_ips,
            slow_scan_ips: Vec::new(),
            last_scan_times: HashMap::new(),
            quick_interval: Duration::from_secs(2),
            slow_interval: Duration::from_secs(30),
        }
    }

    fn update_status(&mut self, ip: &str, status: &ConnectionStatus) {
        let now = Instant::now();
        self.last_scan_times.insert(ip.into(), now);

        match status {
            ConnectionStatus::PortClosed => {
                if !self.quick_scan_ips.contains(&ip.into()) {
                    self.quick_scan_ips.push(ip.into());
                    self.slow_scan_ips.retain(|x| x != ip);
                }
            }
            _ => {
                if !self.slow_scan_ips.contains(&ip.into()) {
                    self.slow_scan_ips.push(ip.into());
                    self.quick_scan_ips.retain(|x| x != ip);
                }
            }
        }
    }

    fn should_scan(&self, ip: &str) -> bool {
        let last_scan = self.last_scan_times.get(ip);
        let now = Instant::now();

        if let Some(last) = last_scan {
            let interval = if self.quick_scan_ips.contains(&ip.into()) {
                self.quick_interval
            } else {
                self.slow_interval
            };
            now.duration_since(*last) >= interval
        } else {
            true // 처음 스캔하는 IP는 바로 스캔
        }
    }

    async fn scan_network(&mut self) -> Option<String> {
        let scan_list: Vec<String> = self
            .quick_scan_ips
            .iter()
            .chain(self.slow_scan_ips.iter())
            .filter(|ip| self.should_scan(ip))
            .cloned()
            .collect();

        let (tx, mut rx) = tokio::sync::mpsc::channel(255);
        for server_ip in scan_list {
            let tx = tx.clone();
            tokio::spawn(async move {
                let status = check_connection(server_ip.copy_string()).await;
                log_debug!("ip: {}, {:?}", server_ip, status);
                if !tx.is_closed() {
                    tx.send((server_ip, status)).await.unwrap();
                }
            });
        }
        drop(tx);

        while let Some((server_ip, status)) = rx.recv().await {
            self.update_status(&server_ip, &status);
            if status == ConnectionStatus::Success {
                rx.close();
                return Some(server_ip);
            }
        }
        None
    }

    pub async fn run(&mut self) -> String {
        loop {
            if let Some(server_ip) = self.scan_network().await {
                return server_ip;
            }
        }
    }
}

async fn check_connection(server_ip: String) -> ConnectionStatus {
    match connect_async(&server_ip).await {
        Ok((mut ws_stream, _)) => {
            ws_stream
                .send(make_disconnect_message(&server_ip))
                .await
                .unwrap();
            ws_stream.flush().await.unwrap();
            // 연결 성공
            ConnectionStatus::Success
        }
        Err(e) => {
            match e {
                tungstenite::Error::Io(e) => match e.kind() {
                    std::io::ErrorKind::TimedOut => {
                        // 타임아웃 - 호스트는 존재할 수 있지만 응답 없음
                        ConnectionStatus::Timeout
                    }
                    std::io::ErrorKind::ConnectionRefused => {
                        // 포트는 닫혔지만 호스트는 존재
                        ConnectionStatus::PortClosed
                    }
                    std::io::ErrorKind::ConnectionReset => {
                        // 연결이 리셋됨 - 방화벽이나 보안 설정 가능성
                        ConnectionStatus::Reset
                    }
                    _ => {
                        // 그 외 에러 (호스트가 없거나 네트워크 문제)
                        ConnectionStatus::Failed
                    }
                },
                _ => ConnectionStatus::Failed,
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ConnectionStatus {
    Success,
    Timeout,
    PortClosed,
    Reset,
    Failed,
}
