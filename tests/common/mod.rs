//! Common test utilities for atomic_websocket integration tests.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio::time::error::Elapsed;

use atomic_websocket::client_sender::ClientSenders;
use atomic_websocket::types::RwClientSenders;

/// Finds an available port for testing.
pub async fn find_available_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
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
pub async fn short_delay() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// Waits for a medium duration.
pub async fn medium_delay() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// Default timeout duration for tests.
pub fn default_timeout() -> Duration {
    Duration::from_secs(5)
}
