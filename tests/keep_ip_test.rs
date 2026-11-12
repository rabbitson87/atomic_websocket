//! What a client that keeps its server address (`use_keep_ip`) does when the
//! connection goes wrong. Two ways it used to end up offline for good:
//!
//! **A server that stops answering and does not hang up.** The other
//! reconnection tests kill the server, which closes the socket: the client's
//! reader sees the end of the stream and the connection winds itself down.
//! That is the easy case. The one that left tablets offline for an evening is
//! a socket that stays open and silent — the tablet's screen went off and its
//! network with it, or the server dropped the client from its list and kept
//! the socket. Nothing errors, nothing ends, and the connection goes on
//! holding the single-flight flag every reconnect path checks first. The
//! checker used to report the connection dead and leave it standing, so the
//! client never dialed again.
//!
//! **A server that is away for longer than one attempt.** The first dial that
//! failed cleared the saved address, kept or not. After that the client had
//! nowhere to dial, and came back only if discovery was on and found the
//! server, or the application handed it the address again.

#![cfg(all(feature = "native-db", feature = "bebop"))]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use atomic_websocket::connection_store::{ConnectionStore, NativeDbConnectionStore};
use atomic_websocket::external::native_db::{Builder, Models};
use atomic_websocket::server_sender::{
    get_internal_connect, ClientOptions, SenderStatus, ServerSender, ServerSenderTrait,
};
use atomic_websocket::types::{RwServerSender, DB};
use atomic_websocket::{AtomicWebsocket, AtomicWebsocketType};
use atomic_websocket::external::tokio_tungstenite::tungstenite::{Bytes, Message};
use tokio::sync::mpsc::Receiver;
use tokio::sync::{Mutex, RwLock};

use common::{find_available_port, simulate_long_downtime, wait_for_status, TestServer};

/// Seconds between the checker's looks. Dead is four of these without a
/// message.
const RETRY_SECONDS: u64 = 2;

fn make_store() -> Arc<dyn ConnectionStore> {
    let mut models = Models::new();
    models
        .define::<atomic_websocket::Settings>()
        .expect("Settings model");
    let models: &'static Models = Box::leak(Box::new(models));
    let db: DB = Arc::new(Mutex::new(
        Builder::new().create_in_memory(models).expect("in-memory db"),
    ));
    Arc::new(NativeDbConnectionStore::new(db))
}

fn options(port: u16, kind: AtomicWebsocketType, use_keep_ip: bool) -> ClientOptions {
    ClientOptions {
        use_ping: true,
        url: format!("ws://127.0.0.1:{port}"),
        retry_seconds: RETRY_SECONDS,
        use_keep_ip,
        connect_timeout_seconds: 3,
        atomic_websocket_type: kind,
        ..Default::default()
    }
}

async fn make_sender(store: &Arc<dyn ConnectionStore>, options: &ClientOptions) -> RwServerSender {
    let mut sender: RwServerSender = Arc::new(RwLock::new(ServerSender::new(
        store.clone(),
        String::new(),
        options.clone(),
    )));
    sender.regist(sender.clone()).await;
    sender
}

/// A client that dials the URL in its options — the POS talking to a server
/// it was given the address of.
async fn connect_external(
    port: u16,
    use_keep_ip: bool,
) -> (RwServerSender, Receiver<SenderStatus>) {
    connect_external_with(options(port, AtomicWebsocketType::External, use_keep_ip)).await
}

async fn connect_external_with(options: ClientOptions) -> (RwServerSender, Receiver<SenderStatus>) {
    let store = make_store();
    let sender = make_sender(&store, &options).await;
    let client =
        AtomicWebsocket::get_outer_client_with_server_sender(store.clone(), options, sender.clone())
            .await;
    let status = client.get_status_receiver().await.expect("status receiver");
    client.get_outer_connect(store).await.expect("connect");
    // The checker it spawned holds what it needs; the handle is not wanted.
    std::mem::forget(client);
    (sender, status)
}

/// A client that dials the address in its store — a tablet or a kitchen
/// screen with the POS address saved.
async fn connect_internal(
    port: u16,
    use_keep_ip: bool,
) -> (RwServerSender, Receiver<SenderStatus>, Arc<dyn ConnectionStore>) {
    let store = make_store();
    let options = options(port, AtomicWebsocketType::Internal, use_keep_ip);
    let sender = make_sender(&store, &options).await;
    let client = AtomicWebsocket::get_internal_client_with_server_sender(
        store.clone(),
        options,
        sender.clone(),
    )
    .await;
    let status = client.get_status_receiver().await.expect("status receiver");
    store
        .set_server_connect_info(&format!("ws://127.0.0.1:{port}"), &port.to_string())
        .await;
    client
        .get_internal_connect(None, store.clone())
        .await
        .expect("connect");
    std::mem::forget(client);
    (sender, status, store)
}

/// The server goes silent on the connection it has, and the client is made
/// to see an hour without a message instead of waiting for one.
async fn go_silent(server: &TestServer, sender: &RwServerSender) {
    server.go_silent();
    // A pong already on its way would put a fresh stamp over the old one.
    tokio::time::sleep(Duration::from_millis(300)).await;
    simulate_long_downtime(sender, 3700).await;
}

async fn connected(status: &mut Receiver<SenderStatus>, within: Duration) -> bool {
    wait_for_status(status, SenderStatus::Connected, within).await
}

/// Waits until `count` dials have failed in a row, and says whether they did.
async fn dials_fail(sender: &RwServerSender, count: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if sender.read().await.failed_dials >= count {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Waits until no connection attempt holds the single-flight flag, and says
/// whether that happened.
async fn flag_is_released(sender: &RwServerSender, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if sender.is_need_connect().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

async fn open_connections_become(server: &TestServer, expected: usize, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if server.open_connection_count() == expected {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// The case from the field, in the configuration the tablets and kitchen
/// screens run: saved address, `use_keep_ip`. The server is there and
/// accepting; only the connection the client holds is dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_keeps_its_address_dials_again_when_the_server_goes_silent() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;
    let (sender, mut status, store) = connect_internal(port, true).await;
    assert!(connected(&mut status, Duration::from_secs(10)).await, "first connection");
    let saved = store.get_server_connect_info().await;

    go_silent(&server, &sender).await;

    assert!(
        wait_for_status(&mut status, SenderStatus::Disconnected, Duration::from_secs(10)).await,
        "the silence should be reported"
    );
    assert!(
        connected(&mut status, Duration::from_secs(15)).await,
        "the dead connection was left standing, so nothing dialed again"
    );
    assert_eq!(server.accepted_connection_count(), 2, "one new connection, not several");
    assert_eq!(
        store.get_server_connect_info().await,
        saved,
        "the saved address must survive the dead connection"
    );
    assert!(
        saved.is_some_and(|(address, _)| !address.is_empty()),
        "and it has to have been an address in the first place"
    );
}

/// The same for a client that dials a URL from its options.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_url_client_that_keeps_its_address_dials_again_when_the_server_goes_silent() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;
    let (sender, mut status) = connect_external(port, true).await;
    assert!(connected(&mut status, Duration::from_secs(10)).await, "first connection");
    let address = sender.read().await.server_ip.clone();

    go_silent(&server, &sender).await;

    assert!(
        wait_for_status(&mut status, SenderStatus::Disconnected, Duration::from_secs(10)).await,
        "the silence should be reported"
    );
    assert!(
        connected(&mut status, Duration::from_secs(15)).await,
        "the dead connection was left standing, so nothing dialed again"
    );
    assert_eq!(server.accepted_connection_count(), 2, "one new connection, not several");
    assert_eq!(sender.read().await.server_ip, address, "the address is kept");
}

/// Dialing again is not the whole of it: the dead socket has to be closed.
/// Left open it is a second connection the server still counts for this
/// client, and its reader would go on stamping "a message arrived" on the
/// client if the old socket ever delivered one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dead_socket_is_closed_not_just_abandoned() {
    for use_keep_ip in [true, false] {
        let port = find_available_port().await;
        let server = TestServer::start(port).await;
        let (sender, mut status) = connect_external(port, use_keep_ip).await;
        assert!(connected(&mut status, Duration::from_secs(10)).await, "first connection");
        assert_eq!(server.open_connection_count(), 1);

        go_silent(&server, &sender).await;

        assert!(
            connected(&mut status, Duration::from_secs(20)).await,
            "use_keep_ip={use_keep_ip}: should be connected again"
        );
        assert!(
            open_connections_become(&server, 1, Duration::from_secs(5)).await,
            "use_keep_ip={use_keep_ip}: the silent socket is still open on the server \
             ({} open)",
            server.open_connection_count()
        );
    }
}

/// The checker spawns the dial in the same breath as the teardown, and the
/// old connection needs a moment to let go of the single-flight flag. A dial
/// that arrives first is turned away, and the next one is a whole backoff
/// step later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_new_dial_does_not_wait_for_the_next_look() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;
    let (sender, mut status, _store) = connect_internal(port, true).await;
    assert!(connected(&mut status, Duration::from_secs(10)).await, "first connection");

    go_silent(&server, &sender).await;

    assert!(
        wait_for_status(&mut status, SenderStatus::Disconnected, Duration::from_secs(10)).await,
        "the silence should be reported"
    );
    let reported = Instant::now();
    assert!(connected(&mut status, Duration::from_secs(15)).await, "second connection");
    // The look after the one that found it dead is two steps on: 4 s.
    assert!(
        reported.elapsed() < Duration::from_secs(RETRY_SECONDS * 2 - 1),
        "connected {:?} after the silence was reported — that is the next look, not this one",
        reported.elapsed()
    );
}

/// The peer has stopped acknowledging and the application went on sending:
/// the socket's buffers are full, the connection's writer is parked in a
/// write that will not come back, and its queue is full behind it. A
/// connection in that state cannot be told to end by anything it would have
/// to read from its queue or write to its socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_whose_socket_takes_no_more_writes_can_still_be_ended() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;
    let (sender, mut status) = connect_external(port, true).await;
    assert!(connected(&mut status, Duration::from_secs(10)).await, "first connection");

    // Fill the socket: megabyte after megabyte to a server that has stopped
    // reading, until a send no longer gets through the client's own queue.
    server.go_deaf();
    let megabyte = Message::Binary(Bytes::from(vec![0u8; 1 << 20]));
    let mut parked = false;
    for _ in 0..512 {
        let sent = tokio::time::timeout(
            Duration::from_millis(500),
            sender.send(megabyte.clone()),
        )
        .await;
        if sent.is_err() {
            parked = true;
            break;
        }
    }
    assert!(parked, "512 MB went out to a server that reads nothing; the test proves nothing");
    assert!(!sender.is_need_connect().await, "the connection is still there");

    let started = Instant::now();
    sender.write().await.drop_connection();

    assert!(
        flag_is_released(&sender, Duration::from_secs(3)).await,
        "the connection is parked in its write and never saw that it was dropped"
    );
    // Given up after the write's grace, and closed without a flush that
    // cannot succeed or a wait for a hang-up that will not come. Either
    // costs a second or more.
    assert!(
        started.elapsed() < Duration::from_millis(800),
        "released after {:?}: a parked connection was still waited on",
        started.elapsed()
    );
}

/// Ending a connection is not the same as cutting it off. A caller that
/// sends its last messages and disconnects in the same breath was told they
/// were accepted: on a socket that still works they go out, and the goodbye
/// after them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_was_accepted_is_sent_before_a_healthy_connection_is_closed() {
    for round in 0..10 {
        let port = find_available_port().await;
        let server = TestServer::start(port).await;
        let (sender, mut status) = connect_external(port, false).await;
        assert!(connected(&mut status, Duration::from_secs(10)).await, "first connection");

        // Category 1: neither a ping nor a goodbye to the test server.
        for n in 0..8u8 {
            sender.send(Message::Binary(Bytes::from(vec![1, 0, n]))).await;
        }
        sender.remove_ip().await;

        assert!(
            open_connections_become(&server, 0, Duration::from_secs(3)).await,
            "round {round}: the connection was not closed"
        );
        assert_eq!(server.message_count(), 8, "round {round}: messages that were accepted");
        assert_eq!(server.goodbye_count(), 1, "round {round}: the goodbye");
        server.shutdown().await;
    }
}

/// A send that was waiting for room in the queue of a connection that gets
/// dropped fails when the queue closes. That connection's end has already
/// been dealt with; reporting it again would put `Disconnected` after the
/// replacement's `Connected`, and an application that mirrors the status
/// would show Offline over a working connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_that_was_waiting_on_a_dropped_connection_does_not_report_it_again() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;
    let (sender, mut status) = connect_external(port, true).await;
    assert!(connected(&mut status, Duration::from_secs(10)).await, "first connection");

    // Park the writer and fill the queue behind it.
    server.go_deaf();
    let megabyte = Message::Binary(Bytes::from(vec![0u8; 1 << 20]));
    let mut parked = false;
    for _ in 0..512 {
        if tokio::time::timeout(Duration::from_millis(500), sender.send(megabyte.clone()))
            .await
            .is_err()
        {
            parked = true;
            break;
        }
    }
    assert!(parked, "512 MB went out to a server that reads nothing; the test proves nothing");

    // Two sends that wait for room, as the library's own next ping does.
    for _ in 0..2 {
        let waiting = sender.clone();
        let message = megabyte.clone();
        tokio::spawn(async move { waiting.send(message).await });
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The checker finds the link dead and replaces the connection.
    simulate_long_downtime(&sender, 3700).await;
    assert!(
        connected(&mut status, Duration::from_secs(20)).await,
        "the connection was not replaced"
    );

    // From here on `Disconnected` would be a lie.
    let until = Instant::now() + Duration::from_secs(4);
    while let Ok(Some(next)) = tokio::time::timeout_at(until.into(), status.recv()).await {
        assert_ne!(
            next,
            SenderStatus::Disconnected,
            "a connection that had already been dropped was reported again"
        );
    }
}

/// A connection that has gone quiet has nothing unread, so there is nothing
/// to wait for when it is closed — and it is the kind that gets closed by
/// whoever cannot spare a quarter of a second: the checker, an application's
/// supervisor after its device woke up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_connection_is_released_without_waiting_for_the_server_to_hang_up() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;
    let (sender, mut status) = connect_external(port, true).await;
    assert!(connected(&mut status, Duration::from_secs(10)).await, "first connection");

    go_silent(&server, &sender).await;
    let started = Instant::now();
    sender.remove_ip().await;

    assert!(
        flag_is_released(&sender, Duration::from_secs(3)).await,
        "the connection was not released"
    );
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "released after {:?}: it waited for a hang-up that a silent server never sends",
        started.elapsed()
    );
}

/// A client that does not ping hears nothing on a healthy link that has
/// nothing to say. Its silence is reported, as it always was, and the link is
/// left alone: taking it down would recycle a working connection every few
/// retry periods.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_does_not_ping_keeps_its_quiet_link() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;
    let (sender, mut status) = connect_external_with(ClientOptions {
        use_ping: false,
        ..options(port, AtomicWebsocketType::External, true)
    })
    .await;
    assert!(connected(&mut status, Duration::from_secs(10)).await, "first connection");

    simulate_long_downtime(&sender, 3700).await;

    // Three looks of the checker at the least.
    tokio::time::sleep(Duration::from_secs(RETRY_SECONDS * 4)).await;
    assert_eq!(server.accepted_connection_count(), 1, "the link was recycled");
    assert_eq!(server.open_connection_count(), 1, "the link was closed");
    assert!(!sender.is_need_connect().await, "the connection is still held");
}

// ============================================================================
// A server that is away for longer than one attempt
// ============================================================================

/// The POS restarts and takes a minute. The client dials the address it has
/// — and gets nothing — more than once before the server is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_address_survives_dials_that_fail() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;
    let (sender, mut status, store) = connect_internal(port, true).await;
    assert!(connected(&mut status, Duration::from_secs(10)).await, "first connection");
    let saved = store.get_server_connect_info().await;

    server.shutdown().await;
    simulate_long_downtime(&sender, 3700).await;

    assert!(
        dials_fail(&sender, 2, Duration::from_secs(20)).await,
        "expected the client to go on dialing while the server is away"
    );
    assert_eq!(
        store.get_server_connect_info().await,
        saved,
        "a dial that failed must not cost a client its kept address"
    );

    // Nothing hands the address back: this is the library on its own.
    let _server = TestServer::start(port).await;
    assert!(
        connected(&mut status, Duration::from_secs(25)).await,
        "with the address gone there is nothing left to dial"
    );
    assert_eq!(sender.read().await.failed_dials, 0, "a connection starts the count over");
}

/// Without `use_keep_ip` the address goes with the first failed dial, as it
/// always has: such a client is meant to find its server again, not to
/// remember it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_does_not_keep_its_address_forgets_it_when_a_dial_fails() {
    let port = find_available_port().await;
    let server = TestServer::start(port).await;
    let (sender, mut status, store) = connect_internal(port, false).await;
    assert!(connected(&mut status, Duration::from_secs(10)).await, "first connection");

    server.shutdown().await;
    simulate_long_downtime(&sender, 3700).await;

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let address = store
            .get_server_connect_info()
            .await
            .map(|(address, _)| address)
            .unwrap_or_default();
        if address.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "the address is still stored: {address}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(sender.read().await.failed_dials, 0, "nothing to count without a kept address");
}

/// The kept address has stopped being where the server is — the POS got a
/// new address from the router. With discovery on, a scan is started beside
/// the dials once they have failed three times, and what it finds becomes
/// the kept address.
///
/// Ignored by default: it scans this machine's subnet for real, needs a
/// network to do it on, and takes as long as the scan needs to reach this
/// machine's own address. Run it with
/// `cargo test --test keep_ip_test -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "scans the local subnet"]
async fn discovery_finds_a_server_that_moved() {
    let port = find_available_port().await;
    let server = TestServer::start_on_all_interfaces(port).await;

    let store = make_store();
    let options = ClientOptions {
        use_scan_discovery: true,
        connect_timeout_seconds: 1,
        ..options(port, AtomicWebsocketType::Internal, true)
    };
    let sender = make_sender(&store, &options).await;
    let mut status = sender.get_status_receiver().await.expect("status receiver");

    // Where the server used to be: an address nothing answers on
    // (192.0.2.0/24 is reserved for documentation).
    let moved_from = format!("ws://192.0.2.1:{port}");
    store
        .set_server_connect_info(&moved_from, &port.to_string())
        .await;

    // What an application's own retry does: ask for a connection again and
    // again. The fourth request is the one that starts the scan.
    for attempt in 1..=4 {
        get_internal_connect(None, store.clone(), sender.clone(), options.clone())
            .await
            .expect("connect request");
        if attempt < 4 {
            assert!(
                dials_fail(&sender, attempt, Duration::from_secs(5)).await,
                "dial {attempt} to the old address should have failed"
            );
            // The count goes up a moment before the failed dial lets go of
            // the flag, and a request made in that moment is turned away.
            assert!(flag_is_released(&sender, Duration::from_secs(2)).await);
        }
    }

    assert!(
        connected(&mut status, Duration::from_secs(90)).await,
        "the scan should have found the server on this machine's own address"
    );
    let (found, _) = store.get_server_connect_info().await.expect("stored address");
    assert_ne!(found, moved_from, "the address that was found replaces the old one");
    assert!(found.ends_with(&format!(":{port}")), "found {found}");
    assert!(server.accepted_connection_count() >= 1);
}
