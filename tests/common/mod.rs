//! Common test utilities for atomic_websocket integration tests.
//!
//! Each integration test binary compiles this module separately and uses only
//! the part of it that it needs, so `dead_code` fires on whatever the others
//! use. The warning carries no information here.
#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
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
    Arc::new(ClientSenders::new())
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
    accepted_connections: Arc<AtomicUsize>,
    open_connections: Arc<AtomicUsize>,
    silent_below: Arc<AtomicUsize>,
    deaf_below: Arc<AtomicUsize>,
    messages: Arc<AtomicUsize>,
    goodbyes: Arc<AtomicUsize>,
}

impl TestServer {
    /// Starts a new test server on the specified port.
    ///
    /// Uses SO_REUSEADDR to allow rapid port reuse after shutdown.
    pub async fn start(port: u16) -> Self {
        Self::start_at(format!("127.0.0.1:{}", port).parse().unwrap()).await
    }

    /// Starts a test server on every interface, so that it can be reached at
    /// this machine's address on the local network — which is where a subnet
    /// scan looks.
    #[allow(dead_code)]
    pub async fn start_on_all_interfaces(port: u16) -> Self {
        Self::start_at(format!("0.0.0.0:{}", port).parse().unwrap()).await
    }

    async fn start_at(addr: std::net::SocketAddr) -> Self {
        let port = addr.port();
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_reuseaddr(true).unwrap();
        socket.bind(addr).unwrap();
        let listener = socket.listen(128).unwrap();

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let connection_handles: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let handles_clone = connection_handles.clone();
        let accepted_connections = Arc::new(AtomicUsize::new(0));
        let accepted_connections_clone = accepted_connections.clone();
        let open_connections = Arc::new(AtomicUsize::new(0));
        let open_connections_clone = open_connections.clone();
        let silent_below = Arc::new(AtomicUsize::new(0));
        let silent_below_clone = silent_below.clone();
        let deaf_below = Arc::new(AtomicUsize::new(0));
        let deaf_below_clone = deaf_below.clone();
        let messages = Arc::new(AtomicUsize::new(0));
        let messages_clone = messages.clone();
        let goodbyes = Arc::new(AtomicUsize::new(0));
        let goodbyes_clone = goodbyes.clone();

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
                            let index = accepted_connections_clone.fetch_add(1, Ordering::SeqCst);
                            let open = open_connections_clone.clone();
                            let silent_below = silent_below_clone.clone();
                            let deaf_below = deaf_below_clone.clone();
                            let counts = (messages_clone.clone(), goodbyes_clone.clone());
                            open.fetch_add(1, Ordering::SeqCst);
                            let h = tokio::spawn(async move {
                                handle_test_connection(
                                    stream,
                                    index,
                                    silent_below,
                                    deaf_below,
                                    counts,
                                )
                                .await;
                                open.fetch_sub(1, Ordering::SeqCst);
                            });
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
            accepted_connections,
            open_connections,
            silent_below,
            deaf_below,
            messages,
            goodbyes,
        }
    }

    /// Messages received so far that were neither a ping nor a goodbye.
    #[allow(dead_code)]
    pub fn message_count(&self) -> usize {
        self.messages.load(Ordering::SeqCst)
    }

    /// Disconnect messages received so far.
    #[allow(dead_code)]
    pub fn goodbye_count(&self) -> usize {
        self.goodbyes.load(Ordering::SeqCst)
    }

    /// Every connection accepted so far stops being read, after the message
    /// it is waiting for. The socket stays open; what the client writes
    /// piles up in the buffers between the two until they are full, and the
    /// client's next write does not come back.
    ///
    /// This is a peer that has stopped acknowledging while the application
    /// keeps sending — a device that left the network with an order on its
    /// way.
    #[allow(dead_code)]
    pub fn go_deaf(&self) {
        self.deaf_below
            .store(self.accepted_connections.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    /// Every connection accepted so far stops answering and stays open: it
    /// still reads what the client sends, answers nothing, and does not
    /// close — not even when asked to. Connections accepted afterwards are
    /// served as usual.
    ///
    /// This is a server that has forgotten a client without hanging up on
    /// it, and from the client's side it is also what a network that went
    /// away looks like: writes go through, nothing comes back, and the
    /// socket reports no error.
    #[allow(dead_code)]
    pub fn go_silent(&self) {
        self.silent_below
            .store(self.accepted_connections.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    /// Number of connections whose socket is still open on the server side.
    #[allow(dead_code)]
    pub fn open_connection_count(&self) -> usize {
        self.open_connections.load(Ordering::SeqCst)
    }

    /// Number of TCP connections the server has accepted so far.
    ///
    /// Used by single-flight regression tests to verify a reconnect race
    /// resulted in exactly one dial reaching the server, not a duplicate.
    #[allow(dead_code)]
    pub fn accepted_connection_count(&self) -> usize {
        self.accepted_connections.load(Ordering::SeqCst)
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
/// Responds to Ping messages with Pong and echoes other messages — until the
/// server has been told to go silent on this connection (see
/// [`TestServer::go_silent`]), after which it only reads.
async fn handle_test_connection(
    stream: tokio::net::TcpStream,
    index: usize,
    silent_below: Arc<AtomicUsize>,
    deaf_below: Arc<AtomicUsize>,
    (messages, goodbyes): (Arc<AtomicUsize>, Arc<AtomicUsize>),
) {
    let Ok(ws_stream) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut write, mut read) = ws_stream.split();

    while let Some(Ok(msg)) = read.next().await {
        if index < deaf_below.load(Ordering::SeqCst) {
            // Holds the socket open and never reads from it again.
            std::future::pending::<()>().await;
        }
        if index < silent_below.load(Ordering::SeqCst) {
            continue;
        }
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
                goodbyes.fetch_add(1, Ordering::SeqCst);
                break;
            }
        }
        messages.fetch_add(1, Ordering::SeqCst);
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

/// Simulates long server downtime by moving the client's clocks back.
///
/// Sets `server_received_times` to `now - seconds_ago`, making the ping
/// loop checker think no messages were received for that duration — and
/// `connected_at` with it. The checker counts a connection's silence from
/// the later of the two, so that a connection made a moment ago is not
/// judged by its predecessor's last message; a connection that has been
/// silent for an hour was also made at least an hour ago, and a test that
/// moved only the one stamp would be describing a connection a few seconds
/// old.
#[allow(dead_code)]
pub async fn simulate_long_downtime(server_sender: &RwServerSender, seconds_ago: i64) {
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut guard = server_sender.write().await;
    guard.server_received_times = now_ts - seconds_ago;
    guard.connected_at = guard.connected_at.min(now_ts - seconds_ago);
}
