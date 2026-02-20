//! Integration tests for reconnection after prolonged server downtime.
//!
//! Tests verify that clients automatically reconnect when a server dies
//! for extended periods (1+ hours) and comes back up.
//!
//! Strategy:
//! - External type: Manipulates `server_received_times` to simulate long downtime
//!   (external checker uses retry_seconds*4 threshold)
//! - Uses short `retry_seconds` (2s) so loop checker fires frequently

mod common;

use std::sync::Arc;
use std::time::Duration;

use atomic_websocket::server_sender::{
    ClientOptions, SenderStatus, ServerSender, ServerSenderTrait,
};
use atomic_websocket::types::RwServerSender;
use atomic_websocket::{AtomicWebsocket, AtomicWebsocketType};
use tokio::sync::{Mutex, RwLock};

use common::{find_available_port, simulate_long_downtime, wait_for_status, TestServer};

#[cfg(not(feature = "native-db"))]
use atomic_websocket::types::InMemoryStorage;

// ============================================================================
// DB helpers
// ============================================================================

#[cfg(not(feature = "native-db"))]
fn create_test_db() -> atomic_websocket::types::DB {
    Arc::new(Mutex::new(InMemoryStorage::new()))
}

#[cfg(feature = "native-db")]
fn create_test_db() -> atomic_websocket::types::DB {
    use atomic_websocket::external::native_db::{Builder, Models};
    let mut models = Models::new();
    models
        .define::<atomic_websocket::Settings>()
        .expect("Failed to define Settings model");
    // Leak models to get 'static lifetime required by Database<'a>.
    // Acceptable in tests — each test creates a small Models instance.
    let models: &'static Models = Box::leak(Box::new(models));
    let db = Builder::new()
        .create_in_memory(models)
        .expect("Failed to create in-memory native-db");
    Arc::new(Mutex::new(db))
}

// ============================================================================
// Client setup helpers (avoids naming private AtomicClient type)
// ============================================================================

/// Creates an external client, initiates connection, returns (server_sender, status_rx, db).
async fn setup_external_client(
    port: u16,
    retry_seconds: u64,
    use_keep_ip: bool,
) -> (
    RwServerSender,
    tokio::sync::mpsc::Receiver<SenderStatus>,
    atomic_websocket::types::DB,
) {
    let db = create_test_db();
    let options = ClientOptions {
        use_ping: true,
        url: format!("127.0.0.1:{}", port),
        retry_seconds,
        use_keep_ip,
        connect_timeout_seconds: 3,
        atomic_websocket_type: AtomicWebsocketType::External,
        #[cfg(feature = "rustls")]
        use_tls: false,
    };

    let mut server_sender: RwServerSender = Arc::new(RwLock::new(ServerSender::new(
        db.clone(),
        options.url.clone(),
        options.clone(),
    )));
    server_sender.regist(server_sender.clone()).await;

    let client = AtomicWebsocket::get_outer_client_with_server_sender(
        db.clone(),
        options,
        server_sender.clone(),
    )
    .await;

    let status_rx = client.get_status_receiver().await;

    // Initiate connection
    client.get_outer_connect(db.clone()).await.unwrap();

    // The client's ping loop checker is already spawned and holds Arc refs,
    // so it stays alive. We forget the client struct to avoid drop issues.
    std::mem::forget(client);

    (server_sender, status_rx, db)
}

/// Creates an internal client without initiating connection.
/// Returns (server_sender, status_rx, db).
async fn setup_internal_client_no_connect(
    port: u16,
    retry_seconds: u64,
) -> (
    RwServerSender,
    tokio::sync::mpsc::Receiver<SenderStatus>,
    atomic_websocket::types::DB,
) {
    let db = create_test_db();
    let options = ClientOptions {
        use_ping: true,
        url: format!("127.0.0.1:{}", port),
        retry_seconds,
        use_keep_ip: false,
        connect_timeout_seconds: 3,
        atomic_websocket_type: AtomicWebsocketType::Internal,
        #[cfg(feature = "rustls")]
        use_tls: false,
    };

    let mut server_sender: RwServerSender = Arc::new(RwLock::new(ServerSender::new(
        db.clone(),
        String::new(),
        options.clone(),
    )));
    server_sender.regist(server_sender.clone()).await;

    let client = AtomicWebsocket::get_internal_client_with_server_sender(
        db.clone(),
        options,
        server_sender.clone(),
    )
    .await;

    let status_rx = client.get_status_receiver().await;
    std::mem::forget(client);

    (server_sender, status_rx, db)
}

// ============================================================================
// Group A: External Type - Basic Reconnection Tests
// ============================================================================

/// A1: Single external client reconnects after server restart.
///
/// Verifies the full cycle: Connected → Disconnected → Connected.
#[tokio::test]
async fn test_single_external_client_reconnects_after_server_restart() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;

    let (server_sender, mut status_rx, _db) = setup_external_client(port, 2, false).await;

    // Wait for Connected
    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(10)
        )
        .await,
        "Client should connect to server"
    );

    // Kill the server
    server.shutdown().await;

    // Wait briefly for handle_websocket to detect the broken connection via failed ping
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Simulate 1+ hour downtime
    simulate_long_downtime(&server_sender, 3700).await;

    // Wait for Disconnected status
    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Disconnected,
            Duration::from_secs(10)
        )
        .await,
        "Client should detect server disconnection"
    );

    // Restart server on same port
    let _server2 = TestServer::start(port).await;

    // Wait for reconnection
    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(15)
        )
        .await,
        "Client should reconnect after server restart"
    );

    // Verify connection state is healthy after reconnection.
    // Note: is_try_connect stays true during an active connection
    // (it only resets when the connection task exits/fails).
    let sender = server_sender.read().await;
    assert!(
        sender.server_received_times > 0,
        "server_received_times should be updated after reconnection"
    );
}

/// A2: Multiple external clients all reconnect after server restart.
#[tokio::test]
async fn test_multiple_external_clients_reconnect_after_server_restart() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;

    let client_count = 3;
    let mut status_rxs = Vec::new();
    let mut server_senders = Vec::new();

    for _ in 0..client_count {
        let (ss, rx, _db) = setup_external_client(port, 2, false).await;
        status_rxs.push(rx);
        server_senders.push(ss);
    }

    // Verify all connected
    for (i, rx) in status_rxs.iter_mut().enumerate() {
        assert!(
            wait_for_status(rx, SenderStatus::Connected, Duration::from_secs(10)).await,
            "Client {} should connect",
            i
        );
    }

    // Kill server
    server.shutdown().await;

    // Simulate long downtime for all clients
    tokio::time::sleep(Duration::from_secs(3)).await;
    for ss in &server_senders {
        simulate_long_downtime(ss, 3700).await;
    }

    // Verify all detect disconnection
    for (i, rx) in status_rxs.iter_mut().enumerate() {
        assert!(
            wait_for_status(rx, SenderStatus::Disconnected, Duration::from_secs(10)).await,
            "Client {} should detect disconnection",
            i
        );
    }

    // Restart server
    let _server2 = TestServer::start(port).await;

    // Verify all reconnect
    for (i, rx) in status_rxs.iter_mut().enumerate() {
        assert!(
            wait_for_status(rx, SenderStatus::Connected, Duration::from_secs(15)).await,
            "Client {} should reconnect",
            i
        );
    }
}

// ============================================================================
// Group B: Edge Cases & Robustness
// ============================================================================

/// B1: is_try_connect resets properly when reconnection fails (server still down).
#[tokio::test]
async fn test_is_try_connect_resets_on_failed_reconnect() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;

    let (server_sender, mut status_rx, _db) = setup_external_client(port, 2, false).await;

    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(10)
        )
        .await,
        "Should connect initially"
    );

    // Kill server (do NOT restart)
    server.shutdown().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    simulate_long_downtime(&server_sender, 3700).await;

    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Disconnected,
            Duration::from_secs(10)
        )
        .await,
        "Should detect disconnection"
    );

    // Wait for a reconnection attempt to fail (server still down)
    tokio::time::sleep(Duration::from_secs(5)).await;

    assert!(
        !server_sender.read().await.is_try_connect,
        "is_try_connect should reset to false after failed reconnection"
    );

    // Now start the server - should eventually reconnect
    let _server2 = TestServer::start(port).await;
    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(15)
        )
        .await,
        "Should reconnect once server is available again"
    );
}

/// B2: Client survives multiple server death/restart cycles.
#[tokio::test]
async fn test_multiple_reconnect_cycles() {
    let port = find_available_port().await;

    let (server_sender, mut status_rx, _db) = setup_external_client(port, 2, false).await;

    for cycle in 0..3 {
        let server = TestServer::start(port).await;

        // Wait for Connected
        assert!(
            wait_for_status(
                &mut status_rx,
                SenderStatus::Connected,
                Duration::from_secs(15)
            )
            .await,
            "Cycle {}: should connect",
            cycle
        );

        // Kill server
        server.shutdown().await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        simulate_long_downtime(&server_sender, 3700).await;

        // Wait for Disconnected
        assert!(
            wait_for_status(
                &mut status_rx,
                SenderStatus::Disconnected,
                Duration::from_secs(10)
            )
            .await,
            "Cycle {}: should detect disconnection",
            cycle
        );
    }

    // Final reconnection
    let _server_final = TestServer::start(port).await;
    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(15)
        )
        .await,
        "Should reconnect after final restart"
    );
}

/// B3: use_keep_ip = false clears server_ip on disconnect.
#[tokio::test]
async fn test_use_keep_ip_false_clears_server_ip() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;

    let (server_sender, mut status_rx, _db) = setup_external_client(port, 2, false).await;

    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(10)
        )
        .await,
        "Should connect"
    );

    assert!(
        !server_sender.read().await.server_ip.is_empty(),
        "server_ip should be set after connection"
    );

    // Kill server and simulate downtime
    server.shutdown().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    simulate_long_downtime(&server_sender, 3700).await;

    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Disconnected,
            Duration::from_secs(10)
        )
        .await,
        "Should detect disconnection"
    );

    // With use_keep_ip: false, server_ip should be cleared
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        server_sender.read().await.server_ip.is_empty(),
        "server_ip should be cleared with use_keep_ip: false"
    );

    // Should still reconnect (uses options.url)
    let _server2 = TestServer::start(port).await;
    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(15)
        )
        .await,
        "Should reconnect even after server_ip was cleared"
    );
}

/// B4: Internal ping loop checker does NOT fire false disconnect when
/// server_received_times == 0 (never connected).
///
/// The internal checker has a `server_received_times > 0` guard.
#[tokio::test]
async fn test_no_false_disconnect_when_never_connected_internal() {
    let port = find_available_port().await;
    // Do NOT start a server

    let (_server_sender, mut status_rx, _db) = setup_internal_client_no_connect(port, 2).await;

    // Internal loop checker: `server_received_times > 0` guard prevents false disconnect.
    // Wait for several ticks (retry_seconds = 2).
    let got_disconnect = wait_for_status(
        &mut status_rx,
        SenderStatus::Disconnected,
        Duration::from_secs(6),
    )
    .await;

    assert!(
        !got_disconnect,
        "Internal checker should NOT fire false disconnect when server_received_times == 0"
    );
}

/// B5: Status channel (capacity 8) does not block the loop checker.
#[tokio::test]
async fn test_channel_capacity_not_blocking_loop_checker() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;

    let (server_sender, mut status_rx, _db) = setup_external_client(port, 2, false).await;

    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(10)
        )
        .await,
        "Should connect"
    );

    // Kill server
    server.shutdown().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    simulate_long_downtime(&server_sender, 3700).await;

    // Do NOT drain status channel - let it fill up (capacity 8).
    // try_send is non-blocking so loop checker should not stall.
    tokio::time::sleep(Duration::from_secs(8)).await;

    // Drain accumulated statuses
    let mut disconnect_count = 0;
    while let Ok(Some(status)) =
        tokio::time::timeout(Duration::from_millis(100), status_rx.recv()).await
    {
        if status == SenderStatus::Disconnected {
            disconnect_count += 1;
        }
    }

    assert!(
        disconnect_count > 0,
        "Should have received at least one Disconnected status"
    );

    // Start server - reconnection should still work
    let _server2 = TestServer::start(port).await;
    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(15)
        )
        .await,
        "Loop checker should still be functional after channel pressure"
    );
}

// ============================================================================
// Group C: Previously Discovered Issues - Now Fixed
// ============================================================================

/// C1: External checker does NOT fire false disconnect when never connected.
///
/// The external checker now has the same `server_received_times > 0` guard
/// as the internal checker. When server_received_times == 0 (never connected),
/// no spurious Disconnected status should be emitted.
#[tokio::test]
async fn test_external_no_false_disconnect_when_never_connected() {
    let port = find_available_port().await;
    // Do NOT start a server

    let db = create_test_db();
    let options = ClientOptions {
        use_ping: true,
        url: format!("127.0.0.1:{}", port),
        retry_seconds: 2,
        use_keep_ip: false,
        connect_timeout_seconds: 3,
        atomic_websocket_type: AtomicWebsocketType::External,
        #[cfg(feature = "rustls")]
        use_tls: false,
    };

    let mut server_sender: RwServerSender = Arc::new(RwLock::new(ServerSender::new(
        db.clone(),
        options.url.clone(),
        options.clone(),
    )));
    server_sender.regist(server_sender.clone()).await;

    let client = AtomicWebsocket::get_outer_client_with_server_sender(
        db.clone(),
        options,
        server_sender.clone(),
    )
    .await;

    let mut status_rx = client.get_status_receiver().await;
    std::mem::forget(client);

    // With the zero guard, checker should NOT fire Disconnected
    // when server_received_times == 0
    let got_disconnect = wait_for_status(
        &mut status_rx,
        SenderStatus::Disconnected,
        Duration::from_secs(5),
    )
    .await;

    assert!(
        !got_disconnect,
        "External checker should NOT fire false disconnect when server_received_times == 0"
    );
}

/// C2: External checker uses retry_seconds-based thresholds.
///
/// `outer_ping_loop_cheker` now uses `retry_seconds * 4` for disconnect detection
/// and `retry_seconds * 2` for ping, consistent with the internal checker.
/// With retry_seconds=2: disconnect threshold = 8s, ping threshold = 4s.
#[tokio::test]
async fn test_external_uses_retry_seconds_thresholds() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;

    // retry_seconds=2 → disconnect threshold = 2*4 = 8s
    let (server_sender, mut status_rx, _db) = setup_external_client(port, 2, false).await;

    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(10)
        )
        .await,
        "Should connect"
    );

    // Kill server
    server.shutdown().await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Simulate downtime > retry_seconds*4 (8s) → should trigger disconnect
    simulate_long_downtime(&server_sender, 10).await;
    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Disconnected,
            Duration::from_secs(5)
        )
        .await,
        "Should detect disconnection after retry_seconds*4 simulated downtime"
    );
}

// ============================================================================
// Group D: use_keep_ip=true Reconnection (the critical fix)
// ============================================================================

/// D1: External client with use_keep_ip=true reconnects after server restart.
///
/// This was the original reported bug: with use_keep_ip=true, server_ip is
/// never cleared after disconnect, so is_need_connect() would return false
/// (same IP comparison), silently blocking reconnection forever.
#[tokio::test]
async fn test_use_keep_ip_true_reconnects_after_server_restart() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;

    // use_keep_ip = true — the bug scenario
    let (server_sender, mut status_rx, _db) = setup_external_client(port, 2, true).await;

    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(10)
        )
        .await,
        "Should connect initially"
    );

    // Verify server_ip is set
    let stored_ip = server_sender.read().await.server_ip.clone();
    assert!(!stored_ip.is_empty(), "server_ip should be set");

    // Kill server and simulate long downtime
    server.shutdown().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    simulate_long_downtime(&server_sender, 3700).await;

    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Disconnected,
            Duration::from_secs(10)
        )
        .await,
        "Should detect disconnection"
    );

    // With use_keep_ip=true, server_ip should NOT be cleared
    assert_eq!(
        server_sender.read().await.server_ip,
        stored_ip,
        "server_ip should be preserved with use_keep_ip=true"
    );

    // Restart server — reconnection must succeed even with same IP
    let _server2 = TestServer::start(port).await;
    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(15)
        )
        .await,
        "Client with use_keep_ip=true should reconnect to same server"
    );
}

/// D2: Multiple clients with use_keep_ip=true all reconnect.
#[tokio::test]
async fn test_multiple_use_keep_ip_true_clients_reconnect() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;

    let client_count = 3;
    let mut status_rxs = Vec::new();
    let mut server_senders = Vec::new();

    for _ in 0..client_count {
        let (ss, rx, _db) = setup_external_client(port, 2, true).await;
        status_rxs.push(rx);
        server_senders.push(ss);
    }

    // Verify all connected
    for (i, rx) in status_rxs.iter_mut().enumerate() {
        assert!(
            wait_for_status(rx, SenderStatus::Connected, Duration::from_secs(10)).await,
            "Client {} should connect",
            i
        );
    }

    // Kill server, simulate long downtime
    server.shutdown().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    for ss in &server_senders {
        simulate_long_downtime(ss, 3700).await;
    }

    // Verify all detect disconnection
    for (i, rx) in status_rxs.iter_mut().enumerate() {
        assert!(
            wait_for_status(rx, SenderStatus::Disconnected, Duration::from_secs(10)).await,
            "Client {} should detect disconnection",
            i
        );
    }

    // Restart server
    let _server2 = TestServer::start(port).await;

    // All must reconnect (this was the bug — none would reconnect before fix)
    for (i, rx) in status_rxs.iter_mut().enumerate() {
        assert!(
            wait_for_status(rx, SenderStatus::Connected, Duration::from_secs(15)).await,
            "Client {} with use_keep_ip=true should reconnect",
            i
        );
    }
}

// ============================================================================
// Group E: Additional Scenarios for is_need_connect Fix Verification
// ============================================================================

/// E1: use_keep_ip=true survives 3 full death/restart cycles.
///
/// Verifies the fix holds up over repeated reconnections to the same server,
/// not just a single reconnection.
#[tokio::test]
async fn test_use_keep_ip_true_multiple_reconnect_cycles() {
    let port = find_available_port().await;

    let (server_sender, mut status_rx, _db) = setup_external_client(port, 2, true).await;

    for cycle in 0..3 {
        let server = TestServer::start(port).await;

        assert!(
            wait_for_status(
                &mut status_rx,
                SenderStatus::Connected,
                Duration::from_secs(15)
            )
            .await,
            "Cycle {}: should connect with use_keep_ip=true",
            cycle
        );

        // Verify server_ip is preserved throughout cycles
        assert!(
            !server_sender.read().await.server_ip.is_empty(),
            "Cycle {}: server_ip should be set",
            cycle
        );

        server.shutdown().await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        simulate_long_downtime(&server_sender, 3700).await;

        assert!(
            wait_for_status(
                &mut status_rx,
                SenderStatus::Disconnected,
                Duration::from_secs(10)
            )
            .await,
            "Cycle {}: should detect disconnection",
            cycle
        );

        // server_ip should remain set (use_keep_ip=true)
        assert!(
            !server_sender.read().await.server_ip.is_empty(),
            "Cycle {}: server_ip should be preserved after disconnect",
            cycle
        );
    }

    // Final reconnection
    let _server_final = TestServer::start(port).await;
    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(15)
        )
        .await,
        "Should reconnect after final restart with use_keep_ip=true"
    );
}

/// E2: is_try_connect prevents duplicate connections during active connection.
///
/// After fixing is_need_connect to not compare IPs, we verify the
/// is_try_connect flag still prevents duplicate handle_websocket setups.
#[tokio::test]
async fn test_is_try_connect_prevents_duplicate_during_active_connection() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;

    let (server_sender, mut status_rx, _db) = setup_external_client(port, 2, true).await;

    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(10)
        )
        .await,
        "Should connect"
    );

    // During active connection, is_try_connect should be true
    assert!(
        server_sender.read().await.is_try_connect,
        "is_try_connect should be true during active connection"
    );

    // Wait for several loop checker ticks (retry_seconds=2)
    // No additional Connected status should appear (no duplicate connections)
    tokio::time::sleep(Duration::from_secs(6)).await;

    // Drain any accumulated statuses — should NOT contain a second Connected
    let mut extra_connected_count = 0;
    while let Ok(Some(status)) =
        tokio::time::timeout(Duration::from_millis(100), status_rx.recv()).await
    {
        if status == SenderStatus::Connected {
            extra_connected_count += 1;
        }
    }

    assert_eq!(
        extra_connected_count, 0,
        "No duplicate Connected status should be sent during active connection"
    );

    // Connection should still work
    server.shutdown().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    simulate_long_downtime(&server_sender, 3700).await;

    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Disconnected,
            Duration::from_secs(10)
        )
        .await,
        "Should detect disconnection after test"
    );
}

/// E3: Mixed use_keep_ip clients on same server all reconnect.
///
/// Some clients use use_keep_ip=true, others use false.
/// Both types should reconnect successfully after server restart.
#[tokio::test]
async fn test_mixed_use_keep_ip_clients_all_reconnect() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;

    // 2 clients with use_keep_ip=true, 2 with false
    let (ss_keep1, mut rx_keep1, _) = setup_external_client(port, 2, true).await;
    let (ss_keep2, mut rx_keep2, _) = setup_external_client(port, 2, true).await;
    let (ss_nokeep1, mut rx_nokeep1, _) = setup_external_client(port, 2, false).await;
    let (ss_nokeep2, mut rx_nokeep2, _) = setup_external_client(port, 2, false).await;

    // All should connect
    for (label, rx) in [
        ("keep1", &mut rx_keep1),
        ("keep2", &mut rx_keep2),
        ("nokeep1", &mut rx_nokeep1),
        ("nokeep2", &mut rx_nokeep2),
    ] {
        assert!(
            wait_for_status(rx, SenderStatus::Connected, Duration::from_secs(10)).await,
            "{} should connect",
            label
        );
    }

    // Kill server, simulate long downtime
    server.shutdown().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    for ss in [&ss_keep1, &ss_keep2, &ss_nokeep1, &ss_nokeep2] {
        simulate_long_downtime(ss, 3700).await;
    }

    // All should detect disconnection
    for (label, rx) in [
        ("keep1", &mut rx_keep1),
        ("keep2", &mut rx_keep2),
        ("nokeep1", &mut rx_nokeep1),
        ("nokeep2", &mut rx_nokeep2),
    ] {
        assert!(
            wait_for_status(rx, SenderStatus::Disconnected, Duration::from_secs(10)).await,
            "{} should detect disconnection",
            label
        );
    }

    // Verify IP preservation behavior
    assert!(
        !ss_keep1.read().await.server_ip.is_empty(),
        "use_keep_ip=true should preserve server_ip"
    );
    // Wait briefly for remove_ip to process
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        ss_nokeep1.read().await.server_ip.is_empty(),
        "use_keep_ip=false should clear server_ip"
    );

    // Restart server — ALL should reconnect
    let _server2 = TestServer::start(port).await;

    for (label, rx) in [
        ("keep1", &mut rx_keep1),
        ("keep2", &mut rx_keep2),
        ("nokeep1", &mut rx_nokeep1),
        ("nokeep2", &mut rx_nokeep2),
    ] {
        assert!(
            wait_for_status(rx, SenderStatus::Connected, Duration::from_secs(15)).await,
            "{} should reconnect",
            label
        );
    }
}

/// E4: Rapid server restart — client handles immediate recovery.
///
/// Server dies and restarts almost immediately (no long downtime simulation).
/// Tests the natural disconnect detection via broken websocket + quick recovery.
#[tokio::test]
async fn test_rapid_server_restart_use_keep_ip_true() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;

    let (server_sender, mut status_rx, _db) = setup_external_client(port, 2, true).await;

    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(10)
        )
        .await,
        "Should connect initially"
    );

    // Kill and immediately restart
    server.shutdown().await;
    simulate_long_downtime(&server_sender, 3700).await;
    let _server2 = TestServer::start(port).await;

    // Should detect disconnect then reconnect
    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Disconnected,
            Duration::from_secs(10)
        )
        .await,
        "Should detect disconnection"
    );

    assert!(
        wait_for_status(
            &mut status_rx,
            SenderStatus::Connected,
            Duration::from_secs(15)
        )
        .await,
        "Should reconnect after rapid server restart"
    );
}
