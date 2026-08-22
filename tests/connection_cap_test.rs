//! The server used to accept without limit.
//!
//! Every accepted socket spawned a task, so anything opening connections
//! faster than they closed — a reconnect storm, a scanner, a bug in a client
//! — grew until the process died. These check that the cap holds, that it
//! releases, and that it is not so eager that ordinary use trips it.

use std::sync::Arc;
use std::time::Duration;

use atomic_websocket::client_sender::{ClientSenders, ServerOptions};
use atomic_websocket::AtomicWebsocket;
use tokio::net::TcpStream;

/// Ephemeral port, so the tests do not fight each other or a real server.
async fn free_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

async fn server_with_cap(cap: usize) -> u16 {
    let port = free_port().await;
    let senders = Arc::new(ClientSenders::new());
    let options = ServerOptions {
        max_connections: cap,
        ..Default::default()
    };
    let _ = AtomicWebsocket::get_internal_server_with_client_senders(
        format!("127.0.0.1:{port}"),
        options,
        senders,
    )
    .await;
    // The listener is bound inside the spawn above.
    tokio::time::sleep(Duration::from_millis(200)).await;
    port
}

/// Connections past the cap are closed rather than queued, and the ones
/// inside it are unaffected.
#[tokio::test]
async fn connections_past_the_cap_are_refused() {
    let port = server_with_cap(2).await;
    let addr = format!("127.0.0.1:{port}");

    let a = TcpStream::connect(&addr).await.expect("first fits");
    let b = TcpStream::connect(&addr).await.expect("second fits");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The third is accepted by the OS backlog and then closed by us, so the
    // connect succeeds and the read returns EOF.
    let third = TcpStream::connect(&addr).await.expect("connect itself works");
    let mut buf = [0u8; 1];
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        use tokio::io::AsyncReadExt;
        let mut third = third;
        third.read(&mut buf).await
    })
    .await
    .expect("the refused connection must be closed, not left hanging");

    assert!(
        matches!(closed, Ok(0)),
        "expected EOF on the refused connection, got {closed:?}"
    );

    drop(a);
    drop(b);
}

/// The same third connection is left alone when the cap is not in the way.
///
/// Without this the refusal test proves only that something closed the
/// socket — a handshake timeout would look identical. Same shape, same
/// timings, one number different.
#[tokio::test]
async fn a_third_connection_is_fine_when_the_cap_is_high() {
    let port = server_with_cap(64).await;
    let addr = format!("127.0.0.1:{port}");

    let _a = TcpStream::connect(&addr).await.unwrap();
    let _b = TcpStream::connect(&addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let third = TcpStream::connect(&addr).await.unwrap();
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(3), async {
        use tokio::io::AsyncReadExt;
        let mut third = third;
        third.read(&mut buf).await
    })
    .await;

    assert!(
        read.is_err(),
        "under the cap the connection should stay open, but it ended: {read:?}"
    );
}

/// A closed connection gives its slot back, or a store would stop accepting
/// tablets after the cap had been reached once over the day.
#[tokio::test]
async fn a_closed_connection_frees_its_slot() {
    let port = server_with_cap(1).await;
    let addr = format!("127.0.0.1:{port}");

    let first = TcpStream::connect(&addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(first);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let second = TcpStream::connect(&addr).await.unwrap();
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_millis(600), async {
        use tokio::io::AsyncReadExt;
        let mut second = second;
        second.read(&mut buf).await
    })
    .await;

    assert!(
        read.is_err(),
        "the slot should have been freed, so this connection stays open: {read:?}"
    );
}

/// The default is generous. A store settles around fifty connections and
/// must never meet this.
#[tokio::test]
async fn the_default_cap_is_far_above_a_real_store() {
    assert!(
        ServerOptions::default().max_connections >= 256,
        "a default that a real deployment can reach is a default that will be hit during service"
    );
}
