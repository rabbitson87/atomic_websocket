//! Integration tests for connection and reconnection logic.
//!
//! These tests verify connection timeout, reconnection scenarios, and client options.

mod common;

use std::time::Duration;

use atomic_websocket::client_sender::ServerOptions;
use atomic_websocket::server_sender::ClientOptions;
use atomic_websocket::AtomicWebsocket;

use common::{default_timeout, find_available_port, with_timeout};

// ============================================================================
// Connection Timeout Tests
// ============================================================================

#[tokio::test]
async fn test_connection_timeout_to_unreachable() {
    // Try to connect to RFC 5737 TEST-NET address (not routable)
    let result = with_timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect("192.0.2.1:9999"),
    )
    .await;

    assert!(
        result.is_err() || result.unwrap().is_err(),
        "Connection to non-existent server should fail"
    );
}

#[tokio::test]
async fn test_tcp_connection_basic() {
    let port = find_available_port().await;
    let addr = format!("127.0.0.1:{}", port);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();

    let accept_task = tokio::spawn(async move { listener.accept().await.is_ok() });

    tokio::time::sleep(Duration::from_millis(10)).await;

    let connect_result = tokio::net::TcpStream::connect(&addr).await;
    assert!(connect_result.is_ok());

    let accept_result = accept_task.await;
    assert!(accept_result.is_ok() && accept_result.unwrap());
}

// ============================================================================
// Reconnection Scenario Tests
// ============================================================================

#[tokio::test]
async fn test_server_accepts_connection() {
    let port = find_available_port().await;
    let addr = format!("127.0.0.1:{}", port);
    let ws_url = format!("ws://127.0.0.1:{}", port);

    let options = ServerOptions::default();
    let _server = AtomicWebsocket::get_internal_server(addr.clone(), options).await;

    tokio::time::sleep(Duration::from_millis(50)).await;

    let result = with_timeout(default_timeout(), tokio_tungstenite::connect_async(&ws_url)).await;

    assert!(result.is_ok() && result.unwrap().is_ok());
}

#[tokio::test]
async fn test_multiple_reconnections() {
    let port = find_available_port().await;
    let addr = format!("127.0.0.1:{}", port);
    let ws_url = format!("ws://127.0.0.1:{}", port);

    let options = ServerOptions::default();
    let _server = AtomicWebsocket::get_internal_server(addr.clone(), options).await;

    tokio::time::sleep(Duration::from_millis(50)).await;

    // First connection
    let result1 = tokio_tungstenite::connect_async(&ws_url).await;
    assert!(result1.is_ok(), "First connection should succeed");

    // Second connection (simulating reconnect)
    let result2 = tokio_tungstenite::connect_async(&ws_url).await;
    assert!(result2.is_ok(), "Second connection should succeed");
}

// ============================================================================
// Options Tests
// ============================================================================

#[test]
fn test_server_options_default() {
    let options = ServerOptions::default();
    assert!(options.use_ping);
    assert_eq!(options.proxy_ping, -1);
}

#[test]
fn test_client_options_default() {
    let options = ClientOptions::default();
    assert!(options.use_ping);
    assert_eq!(options.url, "");
    assert_eq!(options.retry_seconds, 30);
    assert!(!options.use_keep_ip);
    assert_eq!(options.connect_timeout_seconds, 3);
}

#[test]
fn test_client_options_clone() {
    let options = ClientOptions {
        use_ping: false,
        url: "test-url".to_string(),
        retry_seconds: 15,
        use_keep_ip: true,
        connect_timeout_seconds: 5,
        atomic_websocket_type: atomic_websocket::AtomicWebsocketType::External,
        #[cfg(feature = "rustls")]
        use_tls: true,
    };

    let cloned = options.clone();
    assert!(!cloned.use_ping);
    assert_eq!(cloned.url, "test-url");
    assert_eq!(cloned.retry_seconds, 15);
    assert!(cloned.use_keep_ip);
}
