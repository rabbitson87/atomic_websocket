//! Integration tests for client-server communication.
//!
//! These tests verify actual WebSocket connections between server and clients.

mod common;

use std::sync::Arc;
use std::time::Duration;

use atomic_websocket::client_sender::{ClientSenders, ClientSendersTrait, ServerOptions};
use atomic_websocket::AtomicWebsocket;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::{Bytes, Message};

use common::{
    create_test_rw_client_senders, default_timeout, find_available_port, medium_delay, short_delay,
    with_timeout,
};

// ============================================================================
// Server Connection Tests
// ============================================================================

#[tokio::test]
async fn test_server_starts_and_listens() {
    let port = find_available_port().await;
    let addr = format!("127.0.0.1:{}", port);

    let options = ServerOptions::default();
    let _server = AtomicWebsocket::get_internal_server(addr.clone(), options).await;

    short_delay().await;

    let result = with_timeout(default_timeout(), tokio::net::TcpStream::connect(&addr)).await;

    assert!(result.is_ok(), "Should be able to connect to server");
    assert!(result.unwrap().is_ok(), "TCP connection should succeed");
}

#[tokio::test]
async fn test_websocket_connection_upgrade() {
    let port = find_available_port().await;
    let addr = format!("127.0.0.1:{}", port);
    let ws_url = format!("ws://127.0.0.1:{}", port);

    let options = ServerOptions::default();
    let _server = AtomicWebsocket::get_internal_server(addr, options).await;

    short_delay().await;

    let result = with_timeout(default_timeout(), tokio_tungstenite::connect_async(&ws_url)).await;

    assert!(result.is_ok(), "Should complete within timeout");
    assert!(
        result.unwrap().is_ok(),
        "WebSocket connection should succeed"
    );
}

#[tokio::test]
async fn test_multiple_clients_connect() {
    let port = find_available_port().await;
    let addr = format!("127.0.0.1:{}", port);
    let ws_url = format!("ws://127.0.0.1:{}", port);

    let client_senders = create_test_rw_client_senders();
    let options = ServerOptions {
        use_ping: false,
        proxy_ping: -1,
        ..Default::default()
    };

    let _server = AtomicWebsocket::get_internal_server_with_client_senders(
        addr,
        options,
        client_senders.clone(),
    )
    .await;

    short_delay().await;

    let mut connections = Vec::new();
    for _ in 0..3 {
        let result = tokio_tungstenite::connect_async(&ws_url).await;
        assert!(result.is_ok(), "Client should connect successfully");
        connections.push(result.unwrap());
    }

    medium_delay().await;

    assert_eq!(connections.len(), 3);
}

#[tokio::test]
async fn test_server_with_custom_client_senders() {
    let port = find_available_port().await;
    let addr = format!("127.0.0.1:{}", port);

    let client_senders = create_test_rw_client_senders();
    let options = ServerOptions::default();

    let server = AtomicWebsocket::get_internal_server_with_client_senders(
        addr.clone(),
        options,
        client_senders.clone(),
    )
    .await;

    short_delay().await;

    assert_eq!(server.client_senders.read().await.len(), 0);
}

// ============================================================================
// ClientSenders Tests
// ============================================================================

#[tokio::test]
async fn test_client_senders_add_remove() {
    let senders = ClientSenders::new();

    assert!(senders.is_empty());
    assert_eq!(senders.len(), 0);

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    senders.add("peer1", tx).await;

    assert!(!senders.is_empty());
    assert_eq!(senders.len(), 1);
    assert!(senders.is_active("peer1"));

    senders.remove("peer1");
    assert!(senders.is_empty());
    assert!(!senders.is_active("peer1"));
}

#[tokio::test]
async fn test_client_senders_send_message() {
    let senders = ClientSenders::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);

    senders.add("peer1", tx).await;

    let msg = Message::Binary(Bytes::from_static(b"test message"));
    let result = senders.send("peer1", msg).await;

    assert!(result, "Send should succeed");

    let received = rx.recv().await;
    assert!(received.is_some(), "Should receive message");
}

#[tokio::test]
async fn test_client_senders_send_to_nonexistent() {
    let senders = ClientSenders::new();

    let msg = Message::Binary(Bytes::from_static(b"test"));
    let result = senders.send("nonexistent", msg).await;

    assert!(!result, "Send to nonexistent peer should fail");
}

#[tokio::test]
async fn test_client_senders_send_all_broadcast() {
    let senders: Arc<RwLock<ClientSenders>> = Arc::new(RwLock::new(ClientSenders::new()));

    let (tx1, mut rx1) = tokio::sync::mpsc::channel(8);
    let (tx2, mut rx2) = tokio::sync::mpsc::channel(8);

    senders.add("peer1", tx1).await;
    senders.add("peer2", tx2).await;

    let msg = Message::Binary(Bytes::from_static(b"broadcast"));
    senders.send_all(msg).await;

    let received1 = with_timeout(Duration::from_millis(100), rx1.recv()).await;
    let received2 = with_timeout(Duration::from_millis(100), rx2.recv()).await;

    assert!(received1.is_ok() && received1.unwrap().is_some());
    assert!(received2.is_ok() && received2.unwrap().is_some());
}

#[tokio::test]
async fn test_handle_message_receiver() {
    let mut senders = ClientSenders::new();
    let mut rx = senders.get_handle_message_receiver();

    senders
        .send_handle_message(vec![1, 2, 3], "test_peer")
        .await;

    let result = with_timeout(Duration::from_millis(100), rx.recv()).await;
    assert!(result.is_ok());

    let (data, peer) = result.unwrap().unwrap();
    assert_eq!(data, vec![1, 2, 3]);
    assert_eq!(peer, "test_peer");
}
