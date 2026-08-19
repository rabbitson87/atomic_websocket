//! Guards the "a wedged peer cannot stall the client table" invariant.
//!
//! `ClientSenders::add` replaces an existing connection, and on the way it
//! notifies the old one to disconnect. It used to do that *while holding the
//! DashMap shard guard*, awaiting on a bounded channel whose only drain was the
//! old connection's writer task.
//!
//! That is exactly backwards for the case the branch exists to serve. A tablet
//! walks out of WiFi range and comes back: the old socket is half-open, its
//! writer is blocked in `send`, the 8-slot channel fills, and the notice never
//! lands until the OS gives up on the socket minutes later. The shard lock was
//! held for all of it — and dashmap's RwLock is task-fair, so the writers that
//! queued behind it (`insert`, `remove`, `check_client_send_time`) in turn
//! blocked every later reader (`peers`, `len`, `send`). One returning tablet
//! stopped broadcasts for the whole floor.
//!
//! These tests need neither a POS nor a tablet: a channel nobody drains is a
//! faithful stand-in for a socket nobody is reading.

use std::sync::Arc;
use std::time::Duration;

use atomic_websocket::client_sender::ClientSenders;
use atomic_websocket::external::tokio_tungstenite::tungstenite::{Bytes, Message};
use tokio::sync::mpsc;

/// Matches `ServerOptions::default().per_connection_buffer_size`.
const CHANNEL_CAPACITY: usize = 8;

/// Registers `peer` and fills its channel to capacity, holding the receiver so
/// nothing is ever drained. This is the wedged connection.
async fn wedged_peer(senders: &ClientSenders, peer: &str) -> mpsc::Receiver<Message> {
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    senders.add(peer, tx.clone()).await;

    for _ in 0..CHANNEL_CAPACITY {
        tx.try_send(Message::Binary(Bytes::from_static(b"backlog")))
            .expect("channel should accept up to its capacity");
    }
    assert_eq!(tx.capacity(), 0, "the channel must be full for this test");

    rx
}

/// The direct regression: replacing a wedged peer must return promptly.
///
/// Before the fix this awaited until the old channel gained capacity, which for
/// a half-open socket means an OS TCP timeout — minutes, not milliseconds.
#[tokio::test]
async fn replacing_a_wedged_peer_does_not_block() {
    let senders = ClientSenders::new();
    let _old_rx = wedged_peer(&senders, "tablet-7").await;

    let (new_tx, mut new_rx) = mpsc::channel(CHANNEL_CAPACITY);

    tokio::time::timeout(Duration::from_secs(1), senders.add("tablet-7", new_tx))
        .await
        .expect("add() must not wait on a connection that cannot accept messages");

    // And the replacement is the one actually installed: a send for this peer
    // has to reach the new connection, not the abandoned one.
    assert!(
        senders
            .send("tablet-7", Message::Binary(Bytes::from_static(b"hello")))
            .await
    );
    assert!(
        new_rx.try_recv().is_ok(),
        "the new connection should have received the message"
    );
}

/// The consequence that actually cost the floor: while one peer is being
/// replaced, the client table must stay usable for everyone else.
///
/// `remove` on the same key takes the write side of the same shard, so this is
/// a guaranteed conflict rather than a shard-collision coin flip. Before the
/// fix it queued behind the guard `add` was holding across its await, and every
/// reader — `peers()`, and so every broadcast — queued behind *it*.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_being_replaced_does_not_freeze_the_client_table() {
    let senders = Arc::new(ClientSenders::new());
    let _old_rx = wedged_peer(&senders, "tablet-7").await;

    // 47 other tablets, minding their own business.
    let mut others = Vec::new();
    for n in 0..47 {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        senders.add(&format!("tablet-other-{n}"), tx).await;
        others.push(rx);
    }

    let replacing = {
        let senders = Arc::clone(&senders);
        tokio::spawn(async move {
            let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
            senders.add("tablet-7", tx).await;
        })
    };

    // Let the replacement get as far as it is going to get.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A write on the contended shard, off the async runtime so a genuine
    // deadlock shows up as a timeout instead of hanging the test harness.
    let contended = {
        let senders = Arc::clone(&senders);
        tokio::task::spawn_blocking(move || senders.remove("tablet-7"))
    };
    tokio::time::timeout(Duration::from_secs(1), contended)
        .await
        .expect("the client table must not be locked out by a peer being replaced")
        .expect("remove task should not panic");

    // Readers stay live too — this is the call every broadcast starts with.
    tokio::time::timeout(Duration::from_secs(1), async { senders.peers() })
        .await
        .expect("peers() must not queue behind a wedged replacement");

    tokio::time::timeout(Duration::from_secs(2), replacing)
        .await
        .expect("the replacement itself must finish")
        .expect("replacement task should not panic");
}

/// Reconnecting the same peer over and over must not inflate the active-connection
/// gauge. `add` used to increment it on replacement as well as on first sight, so
/// the number climbed all day and never came down.
#[tokio::test]
async fn reconnecting_does_not_inflate_the_active_connection_count() {
    let senders = ClientSenders::new();

    let mut keep_alive = Vec::new();
    for _ in 0..5 {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        senders.add("tablet-7", tx).await;
        keep_alive.push(rx);
    }

    let snapshot = senders.metrics.snapshot();
    assert_eq!(
        senders.len(),
        1,
        "five reconnects of one tablet are still one entry"
    );
    assert_eq!(
        snapshot.connections_active, 1,
        "…and still one active connection"
    );
    assert_eq!(
        snapshot.connections_total, 5,
        "but all five reconnects are counted in the total"
    );
}
