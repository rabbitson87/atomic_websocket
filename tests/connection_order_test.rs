//! What goes out on a connection, and on which one.
//!
//! **The first frame is the ping.** The server learns who a client is from
//! its first frame and closes a connection that starts with anything else.
//! The client used to register a new connection and only then queue the
//! ping, so an application sending at that moment — a kitchen screen with
//! messages waiting for the link to come back does exactly that — got its
//! message out first and had the connection closed under it.
//!
//! **`send_to` sends on one connection.** `send` goes out on whatever
//! connection is registered when it runs and does not say whether any took
//! the message. An application that has to know what reached which
//! connection (to send again on the next one what the last one may have
//! lost) needs both.

#![cfg(all(feature = "native-db", feature = "bebop"))]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use atomic_websocket::common::make_atomic_message;
use atomic_websocket::connection_store::{ConnectionStore, NativeDbConnectionStore};
use atomic_websocket::external::native_db::{Builder, Models};
use atomic_websocket::external::tokio_tungstenite::tungstenite::{Bytes, Message};
use atomic_websocket::server_sender::{
    get_internal_connect, ClientOptions, SenderStatus, ServerSender, ServerSenderTrait,
};
use atomic_websocket::types::{RwServerSender, DB};
use atomic_websocket::{AtomicWebsocket, AtomicWebsocketType};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::Receiver;
use tokio::sync::{Mutex, RwLock};

use common::{find_available_port, wait_for_status};

const PING: u16 = 10000;
const PONG: u16 = 10001;
const APP: u16 = 307;

/// The categories each connection received, in order, one list per
/// connection in the order they were accepted.
type Seen = Arc<std::sync::Mutex<Vec<Vec<u16>>>>;

/// A server that answers pings and writes down everything it is sent.
async fn recording_server(port: u16) -> Seen {
    recording_server_with(port, Arc::new(AtomicBool::new(true))).await
}

/// The same, answering pings only while `answering` is set: a server that
/// still reads but has stopped saying so.
async fn recording_server_with(port: u16, answering: Arc<AtomicBool>) -> Seen {
    let seen: Seen = Arc::default();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind");
    let record = seen.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let record = record.clone();
            let answering = answering.clone();
            tokio::spawn(async move {
                let Ok(ws) = atomic_websocket::external::tokio_tungstenite::accept_async(stream).await
                else {
                    return;
                };
                let index = {
                    let mut all = record.lock().unwrap();
                    all.push(Vec::new());
                    all.len() - 1
                };
                let (mut write, mut read) = ws.split();
                while let Some(Ok(message)) = read.next().await {
                    let data = message.into_data();
                    if data.len() < 2 {
                        continue;
                    }
                    let category = u16::from_le_bytes([data[0], data[1]]);
                    record.lock().unwrap()[index].push(category);
                    if category == PING && answering.load(Ordering::SeqCst) {
                        let pong = Message::Binary(Bytes::from(PONG.to_le_bytes().to_vec()));
                        if write.send(pong).await.is_err() {
                            break;
                        }
                    }
                }
            });
        }
    });
    seen
}

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

/// A client and what it needs to dial again.
struct Client {
    sender: RwServerSender,
    store: Arc<dyn ConnectionStore>,
    options: ClientOptions,
}

impl Client {
    /// Drops the connection and dials the kept address again, the way an
    /// application's supervisor does, and returns the new connection's
    /// number.
    async fn replace_connection(&self) -> u64 {
        let before = connection_number(&self.sender).await;
        {
            // A dial while the server still counts as heard from is taken
            // for "already connected"; a supervisor redials a link that has
            // gone quiet, so the test makes it quiet.
            let mut guard = self.sender.write().await;
            guard.drop_connection();
            guard.server_received_times -= 60;
        }
        for _ in 0..200 {
            if self.sender.is_need_connect().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        get_internal_connect(None, self.store.clone(), self.sender.clone(), self.options.clone())
            .await
            .expect("dial again");
        next_connection(&self.sender, before).await
    }
}

/// A client that keeps its address and dials it, like a kitchen screen.
async fn connect(port: u16) -> (Client, Receiver<SenderStatus>) {
    connect_with(port, make_store()).await
}

async fn connect_with(port: u16, store: Arc<dyn ConnectionStore>) -> (Client, Receiver<SenderStatus>) {
    let options = ClientOptions {
        use_ping: true,
        url: format!("ws://127.0.0.1:{port}"),
        retry_seconds: 2,
        use_keep_ip: true,
        connect_timeout_seconds: 3,
        atomic_websocket_type: AtomicWebsocketType::Internal,
        ..Default::default()
    };
    let mut sender: RwServerSender = Arc::new(RwLock::new(ServerSender::new(
        store.clone(),
        String::new(),
        options.clone(),
    )));
    sender.regist(sender.clone()).await;
    let client = AtomicWebsocket::get_internal_client_with_server_sender(
        store.clone(),
        options.clone(),
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
    (Client { sender, store, options }, status)
}

fn app_message() -> Message {
    make_atomic_message(APP, vec![1, 2, 3])
}

async fn connection_number(sender: &RwServerSender) -> u64 {
    sender.read().await.connection()
}

/// Waits for a connection numbered above `after`, and returns its number.
async fn next_connection(sender: &RwServerSender, after: u64) -> u64 {
    for _ in 0..200 {
        let now = connection_number(sender).await;
        if now > after {
            return now;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no connection after {after}");
}

#[tokio::test]
async fn the_first_frame_of_every_connection_is_the_ping_even_with_the_application_sending() {
    let port = find_available_port().await;
    let seen = recording_server(port).await;

    // An application with a backlog: it sends the moment anything is
    // registered, and keeps sending.
    let (client, mut status) = connect(port).await;
    let stop = Arc::new(AtomicBool::new(false));
    let flood = {
        let sender = client.sender.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                sender.send(app_message()).await;
                tokio::task::yield_now().await;
            }
        })
    };
    assert!(wait_for_status(&mut status, SenderStatus::Connected, Duration::from_secs(5)).await);

    // And again on new connections, with the flood still running.
    for _ in 0..5 {
        client.replace_connection().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop.store(true, Ordering::Relaxed);
    let _ = flood.await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let seen = seen.lock().unwrap().clone();
    assert!(seen.len() >= 6, "six connections at least, got {}", seen.len());
    for (index, frames) in seen.iter().enumerate() {
        assert_eq!(
            frames.first(),
            Some(&PING),
            "connection {index} began with {:?}",
            frames.iter().take(3).collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn send_to_goes_out_on_its_connection_and_says_so() {
    let port = find_available_port().await;
    let seen = recording_server(port).await;
    let (client, mut status) = connect(port).await;
    let sender = &client.sender;
    assert!(wait_for_status(&mut status, SenderStatus::Connected, Duration::from_secs(5)).await);

    let first = connection_number(sender).await;
    assert!(first >= 1);
    assert!(sender.send_to(app_message(), first).await, "the current connection takes it");

    // The connection is replaced. A message for the old one goes nowhere and
    // says so; the new one is a different number.
    sender.write().await.drop_connection();
    assert!(
        !sender.send_to(app_message(), first).await,
        "a dropped connection takes nothing"
    );
    let second = client.replace_connection().await;
    assert_ne!(second, first);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !sender.send_to(app_message(), first).await,
        "nor does the new connection take what was meant for the old one"
    );
    assert!(sender.send_to(app_message(), second).await);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let seen = seen.lock().unwrap().clone();
    let apps = |frames: &Vec<u16>| frames.iter().filter(|&&c| c == APP).count();
    assert_eq!(apps(&seen[0]), 1, "the old connection got its one message: {:?}", seen[0]);
    let last = seen.last().unwrap();
    assert_eq!(apps(last), 1, "the new one only its own: {last:?}");
}

#[tokio::test]
async fn connection_numbers_are_never_reused() {
    let port = find_available_port().await;
    let _seen = recording_server(port).await;
    let (client, mut status) = connect(port).await;
    assert!(wait_for_status(&mut status, SenderStatus::Connected, Duration::from_secs(5)).await);

    // Replaced several times, quickly: their times may be the same second,
    // their numbers are not.
    let mut numbers = vec![connection_number(&client.sender).await];
    for _ in 0..3 {
        numbers.push(client.replace_connection().await);
    }
    let mut unique = numbers.clone();
    unique.dedup();
    assert_eq!(unique, numbers);
    assert!(numbers.windows(2).all(|pair| pair[0] < pair[1]), "{numbers:?}");
}

/// Sends a message on the current connection and reads its delivery mark,
/// both against the same connection.
async fn send_marked(sender: &RwServerSender) -> (u64, u64) {
    let connection = connection_number(sender).await;
    assert!(sender.send_to(app_message(), connection).await);
    let guard = sender.read().await;
    let mark = guard.delivery_mark(connection).expect("still that connection");
    (connection, mark)
}

async fn delivered_within(sender: &RwServerSender, (connection, mark): (u64, u64), within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        if sender.read().await.delivered(connection, mark) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// For a message nothing answers (a notice), the pong to a ping queued
/// after it is the only sign the server read it.
#[tokio::test]
async fn a_message_is_delivered_once_a_ping_queued_after_it_is_answered() {
    let port = find_available_port().await;
    let _seen = recording_server(port).await;
    let (client, mut status) = connect(port).await;
    assert!(wait_for_status(&mut status, SenderStatus::Connected, Duration::from_secs(5)).await);

    let sent = send_marked(&client.sender).await;
    assert!(
        !client.sender.read().await.delivered(sent.0, sent.1),
        "no ping queued after it has been answered yet"
    );
    // Pings go every `retry_seconds` (2) after each pong.
    assert!(delivered_within(&client.sender, sent, Duration::from_secs(6)).await);
}

/// A server that reads but no longer answers, or a connection that died
/// with the message in it, never confirms it.
#[tokio::test]
async fn a_message_is_not_delivered_without_an_answer_to_a_later_ping() {
    let port = find_available_port().await;
    let answering = Arc::new(AtomicBool::new(true));
    let _seen = recording_server_with(port, answering.clone()).await;
    let (client, mut status) = connect(port).await;
    assert!(wait_for_status(&mut status, SenderStatus::Connected, Duration::from_secs(5)).await);

    answering.store(false, Ordering::SeqCst);
    let sent = send_marked(&client.sender).await;
    assert!(!delivered_within(&client.sender, sent, Duration::from_secs(5)).await);
}

/// Once its connection has been replaced, a message it took is not
/// confirmed by anything the new connection hears.
#[tokio::test]
async fn a_replaced_connection_confirms_nothing() {
    let port = find_available_port().await;
    let answering = Arc::new(AtomicBool::new(true));
    let _seen = recording_server_with(port, answering.clone()).await;
    let (client, mut status) = connect(port).await;
    assert!(wait_for_status(&mut status, SenderStatus::Connected, Duration::from_secs(5)).await);

    answering.store(false, Ordering::SeqCst);
    let sent = send_marked(&client.sender).await;
    answering.store(true, Ordering::SeqCst);
    let replaced = client.replace_connection().await;
    assert!(replaced > sent.0);
    assert!(!delivered_within(&client.sender, sent, Duration::from_secs(5)).await);
}

/// The library's own server answers a ping on the connection it came in on.
/// It used to send the pong to whichever connection had the client's id: the
/// last pings of a connection that was being replaced were answered on its
/// replacement, where a client counting its pongs counted answers to pings
/// it never sent there, and took a message for read that the server had
/// not read.
#[tokio::test]
async fn the_server_answers_a_ping_on_the_connection_it_came_in_on() {
    use atomic_websocket::client_sender::ServerOptions;
    use atomic_websocket::external::bebop::Record;
    use atomic_websocket::schema::{Category, Ping};

    let make_ping_message = |peer: &str| {
        let mut datas = Vec::new();
        Ping { peer, activations: 0 }.serialize(&mut datas).unwrap();
        make_atomic_message(Category::Ping as u16, datas)
    };

    let port = find_available_port().await;
    let _server = AtomicWebsocket::get_internal_server(
        format!("127.0.0.1:{port}"),
        ServerOptions::default(),
    )
    .await
    .expect("server");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The old connection: registered under the client's id first.
    let store = make_store();
    store.ensure_client_id().await;
    let id = store.get_client_id().await;
    let (mut old, _) = atomic_websocket::external::tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}"
    ))
    .await
    .expect("old connection");
    old.send(make_ping_message(&id)).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), old.next()).await;
    assert!(
        matches!(&first, Ok(Some(Ok(Message::Binary(b)))) if b.len() >= 2 && u16::from_le_bytes([b[0], b[1]]) == PONG),
        "the old connection's first ping is answered on it: {first:?}"
    );

    // Its replacement takes the id over.
    let (client, mut status) = connect_with(port, store).await;
    assert!(wait_for_status(&mut status, SenderStatus::Connected, Duration::from_secs(5)).await);
    let sent = send_marked(&client.sender).await;

    // The old connection's last pings are still read by the server.
    old.send(make_ping_message(&id)).await.unwrap();
    old.send(make_ping_message(&id)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The client's own next ping is not due before `retry_seconds` (2 s)
    // after its first pong, so nothing it sent can have confirmed this yet.
    assert!(
        !client.sender.read().await.delivered(sent.0, sent.1),
        "a pong for another connection's ping was counted on this one"
    );
}

/// A client that only pings stays in the server's table. The pong is what
/// marks it heard from, and answering on the connection itself must not
/// skip that: the client would be let go after `client_timeout_seconds`
/// while its connection was fine, and nothing sent to it would arrive.
#[tokio::test]
async fn a_client_that_only_pings_stays_registered_with_the_server() {
    use atomic_websocket::client_sender::{ClientSenders, ClientSendersTrait, ServerOptions};

    let port = find_available_port().await;
    let client_senders = Arc::new(ClientSenders::new());
    let options = ServerOptions {
        client_timeout_seconds: 4,
        ..Default::default()
    };
    let _server = AtomicWebsocket::get_internal_server_with_client_senders(
        format!("127.0.0.1:{port}"),
        options,
        client_senders.clone(),
    )
    .await
    .expect("server");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (client, mut status) = connect(port).await;
    assert!(wait_for_status(&mut status, SenderStatus::Connected, Duration::from_secs(5)).await);
    let id = client.store.get_client_id().await;

    // Past the timeout, with nothing but pings (every 2 s) on the link.
    tokio::time::sleep(Duration::from_secs(5)).await;
    client_senders.check_client_send_time();
    assert!(client_senders.is_active(&id).await, "dropped while it was pinging");
}

/// A client the server has let go (it was silent too long) gets no pong on
/// the connection it still has: it hears nothing, goes quiet and dials
/// again, which is how it gets back into the table. Answered there, it
/// would stay connected and never be sent anything again.
#[tokio::test]
async fn a_client_the_server_let_go_is_not_answered_on_its_old_connection() {
    use atomic_websocket::client_sender::{ClientSenders, ServerOptions};
    use atomic_websocket::external::bebop::Record;
    use atomic_websocket::schema::{Category, Ping};

    let port = find_available_port().await;
    let client_senders = Arc::new(ClientSenders::new());
    let _server = AtomicWebsocket::get_internal_server_with_client_senders(
        format!("127.0.0.1:{port}"),
        ServerOptions::default(),
        client_senders.clone(),
    )
    .await
    .expect("server");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let id = "let-go-peer";
    let ping = || {
        let mut datas = Vec::new();
        Ping { peer: id, activations: 0 }.serialize(&mut datas).unwrap();
        make_atomic_message(Category::Ping as u16, datas)
    };
    let (mut ws, _) = atomic_websocket::external::tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}"
    ))
    .await
    .expect("connection");
    ws.send(ping()).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
    assert!(
        matches!(&first, Ok(Some(Ok(Message::Binary(b)))) if b.len() >= 2 && u16::from_le_bytes([b[0], b[1]]) == PONG),
        "registered and answered: {first:?}"
    );

    client_senders.remove(id);
    ws.send(ping()).await.unwrap();
    let answered = tokio::time::timeout(Duration::from_millis(700), ws.next()).await;
    assert!(answered.is_err(), "answered after being let go: {answered:?}");
}

