use std::{collections::HashMap, sync::Arc};

use tokio::sync::RwLock;

use crate::helpers::scan_manager::{ConnectionState, WebSocketStatus};

use super::StringUtil;

pub trait ConnectionManager {
    async fn start_connection(&self, server_ip: &str);
    async fn end_connection(&self, server_ip: &str, status: WebSocketStatus);
    async fn get_connected_ip(&self) -> Option<String>;
}

impl ConnectionManager for Arc<RwLock<HashMap<String, ConnectionState>>> {
    async fn start_connection(&self, server_ip: &str) {
        self.write().await.insert(
            server_ip.into(),
            ConnectionState {
                status: WebSocketStatus::Connecting,
                is_connecting: true,
            },
        );
    }

    async fn end_connection(&self, server_ip: &str, status: WebSocketStatus) {
        if let Some(state) = self.write().await.get_mut(server_ip) {
            state.status = status;
            state.is_connecting = false;
        }
    }

    async fn get_connected_ip(&self) -> Option<String> {
        for (server_ip, state) in self.read().await.iter() {
            if state.status == WebSocketStatus::Connected {
                return Some(server_ip.copy_string());
            }
        }
        None
    }
}
