use std::{collections::HashMap, sync::Arc};

use tokio::{net::TcpStream, sync::RwLock};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::helpers::scan_manager::{ConnectionState, WebSocketStatus};

use super::StringUtil;

pub trait ConnectionManager {
    async fn start_connection(&self, server_ip: &str);
    async fn end_connection(
        &self,
        server_ip: &str,
        status: (
            WebSocketStatus,
            Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        ),
    );
    async fn get_connected_ip(
        &self,
    ) -> Option<(String, WebSocketStream<MaybeTlsStream<TcpStream>>)>;
    async fn is_connected(&self) -> bool;
}

impl ConnectionManager for Arc<RwLock<HashMap<String, ConnectionState>>> {
    async fn start_connection(&self, server_ip: &str) {
        self.write().await.insert(
            server_ip.into(),
            ConnectionState {
                status: WebSocketStatus::Connecting,
                is_connecting: true,
                ws_stream: None,
            },
        );
    }

    async fn end_connection(
        &self,
        server_ip: &str,
        status: (
            WebSocketStatus,
            Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        ),
    ) {
        if let Some(state) = self.write().await.get_mut(server_ip) {
            state.status = status.0;
            state.is_connecting = false;
            state.ws_stream = status.1;
        }
    }

    async fn get_connected_ip(
        &self,
    ) -> Option<(String, WebSocketStream<MaybeTlsStream<TcpStream>>)> {
        for (server_ip, state) in self.write().await.iter_mut() {
            if state.status == WebSocketStatus::Connected {
                return Some((
                    server_ip.copy_string(),
                    state.ws_stream.take().expect("ws_stream already taken"),
                ));
            }
        }
        None
    }

    async fn is_connected(&self) -> bool {
        self.read()
            .await
            .iter()
            .any(|(_, state)| state.status == WebSocketStatus::Connected)
    }
}
