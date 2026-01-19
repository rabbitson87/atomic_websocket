//! Connection state management for WebSocket connections.
//!
//! This module provides a trait and implementation for managing the state
//! of multiple WebSocket connections during network scanning.

use std::sync::Arc;

use dashmap::DashMap;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::helpers::scan_manager::{ConnectionState, WebSocketStatus};

/// Trait for managing WebSocket connection states during network scanning.
pub trait ConnectionManager {
    /// Marks a connection attempt as started for the given server IP.
    fn start_connection(&self, server_ip: &str);

    /// Updates the connection state after a connection attempt completes.
    fn end_connection(
        &self,
        server_ip: &str,
        status: (
            WebSocketStatus,
            Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        ),
    );

    /// Finds and takes the WebSocket stream for a connected server.
    fn get_connected_ip(&self) -> Option<(String, WebSocketStream<MaybeTlsStream<TcpStream>>)>;

    /// Checks if any connection is in the Connected state.
    fn is_connected(&self) -> bool;
}

impl ConnectionManager for Arc<DashMap<String, ConnectionState>> {
    fn start_connection(&self, server_ip: &str) {
        self.insert(
            server_ip.into(),
            ConnectionState {
                status: WebSocketStatus::Connecting,
                is_connecting: true,
                ws_stream: None,
            },
        );
    }

    fn end_connection(
        &self,
        server_ip: &str,
        status: (
            WebSocketStatus,
            Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        ),
    ) {
        if let Some(mut state) = self.get_mut(server_ip) {
            state.status = status.0;
            state.is_connecting = false;
            state.ws_stream = status.1;
        }
    }

    fn get_connected_ip(&self) -> Option<(String, WebSocketStream<MaybeTlsStream<TcpStream>>)> {
        // Find the connected IP that has a ws_stream
        let connected_ip = self
            .iter()
            .find(|entry| entry.status == WebSocketStatus::Connected && entry.ws_stream.is_some())
            .map(|entry| entry.key().clone());

        // Take the ws_stream if we found a connected IP
        if let Some(ip) = connected_ip {
            if let Some(mut state) = self.get_mut(&ip) {
                if let Some(ws_stream) = state.ws_stream.take() {
                    return Some((ip, ws_stream));
                }
            }
        }

        None
    }

    fn is_connected(&self) -> bool {
        self.iter()
            .any(|entry| entry.status == WebSocketStatus::Connected && entry.ws_stream.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_states() -> Arc<DashMap<String, ConnectionState>> {
        Arc::new(DashMap::new())
    }

    #[test]
    fn test_connection_manager_start_connection() {
        let states = create_test_states();

        states.start_connection("192.168.1.1:9000");

        let state = states.get("192.168.1.1:9000").unwrap();
        assert!(state.is_connecting);
        assert_eq!(state.status, WebSocketStatus::Connecting);
        assert!(state.ws_stream.is_none());
    }

    #[test]
    fn test_connection_manager_start_multiple_connections() {
        let states = create_test_states();

        states.start_connection("192.168.1.1:9000");
        states.start_connection("192.168.1.2:9000");
        states.start_connection("192.168.1.3:9000");

        assert_eq!(states.len(), 3);
        assert!(states.contains_key("192.168.1.1:9000"));
        assert!(states.contains_key("192.168.1.2:9000"));
        assert!(states.contains_key("192.168.1.3:9000"));
    }

    #[test]
    fn test_connection_manager_end_connection_timeout() {
        let states = create_test_states();

        states.start_connection("192.168.1.1:9000");
        states.end_connection("192.168.1.1:9000", (WebSocketStatus::Timeout, None));

        let state = states.get("192.168.1.1:9000").unwrap();
        assert!(!state.is_connecting);
        assert_eq!(state.status, WebSocketStatus::Timeout);
        assert!(state.ws_stream.is_none());
    }

    #[test]
    fn test_connection_manager_end_connection_refused() {
        let states = create_test_states();

        states.start_connection("192.168.1.1:9000");
        states.end_connection(
            "192.168.1.1:9000",
            (WebSocketStatus::ConnectionRefused, None),
        );

        let state = states.get("192.168.1.1:9000").unwrap();
        assert!(!state.is_connecting);
        assert_eq!(state.status, WebSocketStatus::ConnectionRefused);
    }

    #[test]
    fn test_connection_manager_is_connected_empty() {
        let states = create_test_states();
        assert!(!states.is_connected());
    }

    #[test]
    fn test_connection_manager_is_connected_no_stream() {
        let states = create_test_states();

        states.start_connection("192.168.1.1:9000");
        // End connection as Connected but with no stream
        states.end_connection("192.168.1.1:9000", (WebSocketStatus::Connected, None));

        // Should return false because ws_stream is None
        assert!(!states.is_connected());
    }

    #[test]
    fn test_connection_manager_is_connected_not_connected_status() {
        let states = create_test_states();

        states.start_connection("192.168.1.1:9000");
        states.end_connection("192.168.1.1:9000", (WebSocketStatus::Timeout, None));

        assert!(!states.is_connected());
    }

    #[test]
    fn test_connection_manager_get_connected_ip_empty() {
        let states = create_test_states();
        assert!(states.get_connected_ip().is_none());
    }

    #[test]
    fn test_connection_manager_get_connected_ip_no_stream() {
        let states = create_test_states();

        states.start_connection("192.168.1.1:9000");
        states.end_connection("192.168.1.1:9000", (WebSocketStatus::Connected, None));

        // Should return None because ws_stream is None
        assert!(states.get_connected_ip().is_none());
    }

    #[test]
    fn test_websocket_status_equality() {
        assert_eq!(WebSocketStatus::Connecting, WebSocketStatus::Connecting);
        assert_eq!(WebSocketStatus::Connected, WebSocketStatus::Connected);
        assert_eq!(
            WebSocketStatus::ConnectionRefused,
            WebSocketStatus::ConnectionRefused
        );
        assert_eq!(WebSocketStatus::Timeout, WebSocketStatus::Timeout);

        assert_ne!(WebSocketStatus::Connecting, WebSocketStatus::Connected);
        assert_ne!(
            WebSocketStatus::Connected,
            WebSocketStatus::ConnectionRefused
        );
        assert_ne!(WebSocketStatus::ConnectionRefused, WebSocketStatus::Timeout);
    }

    #[test]
    fn test_connection_state_initial() {
        let state = ConnectionState {
            status: WebSocketStatus::Connecting,
            is_connecting: true,
            ws_stream: None,
        };

        assert_eq!(state.status, WebSocketStatus::Connecting);
        assert!(state.is_connecting);
        assert!(state.ws_stream.is_none());
    }
}
