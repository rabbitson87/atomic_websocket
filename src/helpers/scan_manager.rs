//! Local network scanning and server discovery functionality.
//!
//! This module provides mechanisms to automatically discover WebSocket servers
//! on the local network by scanning IP addresses in the same subnet.
//!
//! The scanner uses parallel connection attempts with a configurable concurrency
//! limit (Semaphore) to balance speed and resource usage.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::time::{timeout, MissedTickBehavior};
use tokio_tungstenite::{connect_async, tungstenite, MaybeTlsStream, WebSocketStream};

use crate::helpers::traits::connection_state::ConnectionManager;
use crate::log_debug;
use crate::server_sender::get_ip_address;

/// Default maximum concurrent connection attempts during network scanning.
const DEFAULT_MAX_CONCURRENT_CONNECTIONS: usize = 50;

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
/// to discover available WebSocket servers. Uses a semaphore to limit concurrent
/// connection attempts for better resource management.
pub struct ScanManager {
    /// List of IP addresses to scan
    scan_ips: Vec<String>,
    /// Tracks connection state for each IP address
    connection_states: Arc<DashMap<String, ConnectionState>>,
    /// Semaphore to limit concurrent connection attempts
    semaphore: Arc<Semaphore>,
    /// Flag to stop scanning when connected
    stop_flag: Arc<AtomicBool>,
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
    /// A new ScanManager instance with default concurrency limit (50)
    pub fn new(port: &str) -> Self {
        Self::with_concurrency(port, DEFAULT_MAX_CONCURRENT_CONNECTIONS)
    }

    /// Creates a new ScanManager with a custom concurrency limit.
    ///
    /// # Arguments
    ///
    /// * `port` - The port number to scan for WebSocket servers
    /// * `max_concurrent` - Maximum number of concurrent connection attempts
    ///
    /// # Returns
    ///
    /// A new ScanManager instance
    pub fn with_concurrency(port: &str, max_concurrent: usize) -> Self {
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
            connection_states: Arc::new(DashMap::new()),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            stop_flag: Arc::new(AtomicBool::new(false)),
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
    fn is_connecting_allowed(&self, server_ip: &str) -> bool {
        // Check if stop flag is set (connection already established)
        if self.stop_flag.load(Ordering::Acquire) {
            return false;
        }
        // Check if already attempting to connect
        if let Some(state) = self.connection_states.get(server_ip) {
            if state.is_connecting {
                return false;
            }
        }
        // Check if already connected to any server
        if self.connection_states.is_connected() {
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
    fn get_scannable_ips(&self) -> Vec<String> {
        self.scan_ips
            .iter()
            .filter(|server_ip| {
                if let Some(state) = self.connection_states.get(*server_ip) {
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
    /// Spawns connection attempts for each IP address in parallel, limited by the
    /// configured semaphore to prevent resource exhaustion.
    async fn scan_network(&mut self) {
        // Early exit if stop flag is set
        if self.stop_flag.load(Ordering::Acquire) {
            return;
        }

        let scan_list: Vec<String> = self.get_scannable_ips();

        for server_ip in scan_list {
            if !self.is_connecting_allowed(&server_ip) {
                continue;
            }

            let connection_states = self.connection_states.clone();
            let semaphore = self.semaphore.clone();
            let stop_flag = self.stop_flag.clone();

            tokio::spawn(async move {
                // Check stop flag before acquiring semaphore
                if stop_flag.load(Ordering::Acquire) {
                    return;
                }

                // Acquire semaphore permit before attempting connection
                let _permit = semaphore.acquire().await;

                // Check stop flag again after acquiring permit
                if stop_flag.load(Ordering::Acquire) {
                    return;
                }

                connection_states.start_connection(&server_ip);
                let status = check_connection(server_ip.clone()).await;
                log_debug!("server_ip: {}, {:?}", server_ip, status);

                // Set stop flag if connected successfully
                if status.0 == WebSocketStatus::Connected {
                    stop_flag.store(true, Ordering::Release);
                }

                connection_states.end_connection(&server_ip, status);
                // Permit is automatically released when _permit is dropped
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
            if let Some(state) = self.connection_states.get_connected_ip() {
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
