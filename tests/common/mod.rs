//! Common test utilities for atomic_websocket integration tests.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::error::Elapsed;
use tokio_tungstenite::tungstenite::{Bytes, Message};

use atomic_websocket::client_sender::ClientSenders;
use atomic_websocket::server_sender::SenderStatus;
use atomic_websocket::types::{RwClientSenders, RwServerSender};

/// Finds an available port for testing.
pub async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind to random port");
    listener.local_addr().unwrap().port()
}

/// Runs a future with a timeout.
pub async fn with_timeout<T, F: std::future::Future<Output = T>>(
    duration: Duration,
    future: F,
) -> Result<T, Elapsed> {
    tokio::time::timeout(duration, future).await
}

/// Creates a test RwClientSenders instance.
pub fn create_test_rw_client_senders() -> RwClientSenders {
    Arc::new(RwLock::new(ClientSenders::new()))
}

/// Waits for a short duration (useful for async operations to complete).
#[allow(dead_code)]
pub async fn short_delay() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// Waits for a medium duration.
#[allow(dead_code)]
pub async fn medium_delay() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// Default timeout duration for tests.
pub fn default_timeout() -> Duration {
    Duration::from_secs(5)
}

// ============================================================================
// TestServer: Controllable WebSocket server for reconnection tests
// ============================================================================

/// Category constants matching bebop schema.
const CATEGORY_PING: u16 = 10000;
const CATEGORY_PONG: u16 = 10001;
const CATEGORY_DISCONNECT: u16 = 10003;

/// A controllable WebSocket test server with explicit lifecycle management.
///
/// Handles the atomic_websocket protocol (ping/pong with bebop categories)
/// and can be shut down to simulate server crashes.
pub struct TestServer {
    pub port: u16,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    handle: JoinHandle<()>,
}

impl TestServer {
    /// Starts a new test server on the specified port.
    ///
    /// Uses SO_REUSEADDR to allow rapid port reuse after shutdown.
    pub async fn start(port: u16) -> Self {
        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_reuseaddr(true).unwrap();
        socket.bind(addr).unwrap();
        let listener = socket.listen(128).unwrap();

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let connection_handles: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let handles_clone = connection_handles.clone();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        // Abort all active connection handlers
                        let handles = handles_clone.lock().await;
                        for h in handles.iter() {
                            h.abort();
                        }
                        break;
                    }
                    result = listener.accept() => {
                        if let Ok((stream, _)) = result {
                            let h = tokio::spawn(handle_test_connection(stream));
                            handles_clone.lock().await.push(h);
                        }
                    }
                }
            }
        });

        // Wait for server to be ready
        tokio::time::sleep(Duration::from_millis(50)).await;

        Self {
            port,
            shutdown_tx: Some(shutdown_tx),
            handle,
        }
    }

    /// Shuts down the server, aborting all connections.
    ///
    /// Simulates a server crash by immediately dropping all connections.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.handle.await;
        // Brief delay to allow OS to release the port
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Handles a single test WebSocket connection.
///
/// Responds to Ping messages with Pong and echoes other messages.
async fn handle_test_connection(stream: tokio::net::TcpStream) {
    let Ok(ws_stream) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut write, mut read) = ws_stream.split();

    while let Some(Ok(msg)) = read.next().await {
        let data = msg.into_data();
        if data.len() >= 2 {
            let category = data[0] as u16 + (data[1] as u16) * 256;

            if category == CATEGORY_PING {
                // Respond with Pong
                let pong_bytes: Vec<u8> =
                    vec![(CATEGORY_PONG % 256) as u8, (CATEGORY_PONG / 256) as u8];
                let pong = Message::Binary(Bytes::from(pong_bytes));
                if write.send(pong).await.is_err() {
                    break;
                }
                continue;
            }

            if category == CATEGORY_DISCONNECT {
                break;
            }
        }
        // Echo other messages
        if write
            .send(Message::Binary(Bytes::from(data.to_vec())))
            .await
            .is_err()
        {
            break;
        }
    }
}

// ============================================================================
// Reconnection test helpers
// ============================================================================

/// Waits for a specific SenderStatus on the status channel.
///
/// Skips non-matching statuses until the expected one arrives or timeout.
/// Returns true if the expected status was received.
#[allow(dead_code)]
pub async fn wait_for_status(
    rx: &mut tokio::sync::mpsc::Receiver<SenderStatus>,
    expected: SenderStatus,
    timeout_duration: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout_duration;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(status)) if status == expected => return true,
            Ok(Some(_)) => continue,
            Ok(None) => return false,
            Err(_) => return false,
        }
    }
}

/// Simulates long server downtime by manipulating server_received_times.
///
/// Sets `server_received_times` to `now - seconds_ago`, making the ping
/// loop checker think no messages were received for that duration.
#[allow(dead_code)]
pub async fn simulate_long_downtime(server_sender: &RwServerSender, seconds_ago: i64) {
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    server_sender.write().await.server_received_times = now_ts - seconds_ago;
}
