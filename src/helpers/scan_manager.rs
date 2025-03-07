//! Local network scanning and server discovery functionality.
//!
//! This module provides mechanisms to automatically discover WebSocket servers
//! on the local network by scanning IP addresses in the same subnet.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::time::{timeout, MissedTickBehavior};
use tokio_tungstenite::{connect_async, tungstenite, MaybeTlsStream, WebSocketStream};

use crate::helpers::traits::connection_state::ConnectionManager;
use crate::log_debug;
use crate::server_sender::get_ip_address;

use super::traits::StringUtil;

/// Represents the state of a WebSocket connection during scanning.
pub struct ConnectionState {
    /// Current status of the connection
    pub status: WebSocketStatus,
    /// Whether a connection attempt is in progress
    pub is_connecting: bool,
    /// The actual WebSocket stream if connected
    pub ws_stream: Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
}

/// Manages the scanning process to discover servers on the local network.
///
/// Automatically attempts to connect to all IP addresses on the same subnet
/// to discover available WebSocket servers.
pub struct ScanManager {
    /// List of IP addresses to scan
    scan_ips: Vec<String>,
    /// Tracks connection state for each IP address
    connection_states: Arc<RwLock<HashMap<String, ConnectionState>>>,
}

impl ScanManager {
    /// Creates a new ScanManager for a specific port.
    ///
    /// Generates a list of IP addresses to scan based on the local network.
    /// This requires internet connectivity to determine the local IP address.
    ///
    /// # Arguments
    ///
    /// * `port` - The port number to scan for WebSocket servers
    ///
    /// # Returns
    ///
    /// A new ScanManager instance
    pub fn new(port: &str) -> Self {
        let mut scan_ips = Vec::new();
        let ip = get_ip_address();
        let ips = ip.split('.').collect::<Vec<&str>>();

        // Generate IP addresses for the entire subnet (1-254)
        for sub_ip in 1..255 {
            let ip = format!("ws://{}.{}.{}.{}:{}", ips[0], ips[1], ips[2], sub_ip, port);
            scan_ips.push(ip);
        }

        Self {
            scan_ips,
            connection_states: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Determines if a connection attempt to a specific IP is allowed.
    ///
    /// Prevents duplicate connection attempts and stops scanning once
    /// a server is found.
    ///
    /// # Arguments
    ///
    /// * `server_ip` - The IP address to check
    ///
    /// # Returns
    ///
    /// `true` if a connection attempt is allowed, `false` otherwise
    async fn is_connecting_allowed(&self, server_ip: &str) -> bool {
        // Check if already attempting to connect
        if let Some(state) = self.connection_states.read().await.get(server_ip) {
            if state.is_connecting {
                return false;
            }
        }
        // Check if already connected to any server
        if self.connection_states.is_connected().await {
            return false;
        }
        true
    }

    /// Gets a list of IP addresses that can be scanned.
    ///
    /// Filters out IP addresses that are already being processed.
    ///
    /// # Returns
    ///
    /// Vector of scannable IP addresses
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

    /// Scans the network for WebSocket servers.
    ///
    /// Spawns connection attempts for each IP address in parallel.
    async fn scan_network(&mut self) {
        let scan_list: Vec<String> = self.get_scannable_ips().await;

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

    /// Runs the scanning process until a server is found.
    ///
    /// Periodically scans the network until a WebSocket server is discovered.
    ///
    /// # Returns
    ///
    /// A tuple containing the server IP address and the established WebSocket stream
    pub async fn run(&mut self) -> (String, WebSocketStream<MaybeTlsStream<TcpStream>>) {
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now(), Duration::from_secs(2));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            interval.tick().await;
            if let Some(state) = self.connection_states.get_connected_ip().await {
                return state;
            }
            self.scan_network().await;
        }
    }
}

/// Attempts to establish a WebSocket connection to a specific IP address.
///
/// # Arguments
///
/// * `server_ip` - The WebSocket server address to connect to
///
/// # Returns
///
/// A tuple containing the connection status and the WebSocket stream if successful
async fn check_connection(
    server_ip: String,
) -> (
    WebSocketStatus,
    Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
) {
    match timeout(Duration::from_secs(10), connect_async(&server_ip)).await {
        Ok(result) => match result {
            Ok((ws_stream, _)) => {
                // Connection successful
                (WebSocketStatus::Connected, Some(ws_stream))
            }

            Err(e) => {
                match e {
                    tungstenite::Error::Io(e) => match e.kind() {
                        std::io::ErrorKind::ConnectionRefused => {
                            // Port is closed but host exists
                            (WebSocketStatus::ConnectionRefused, None)
                        }
                        _ => {
                            // Other errors (no host or network issue)
                            (WebSocketStatus::Timeout, None)
                        }
                    },
                    _ => (WebSocketStatus::Timeout, None),
                }
            }
        },
        Err(_) => {
            // Timeout
            (WebSocketStatus::Timeout, None)
        }
    }
}

/// Possible states of a WebSocket connection during scanning.
#[derive(Debug, PartialEq, Eq)]
pub enum WebSocketStatus {
    /// Connection attempt in progress
    Connecting,
    /// Successfully connected
    Connected,
    /// Connection was refused (host exists but port is closed)
    ConnectionRefused,
    /// Connection timed out (no host or network issue)
    Timeout,
}
