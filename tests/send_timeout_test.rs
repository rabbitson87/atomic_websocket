//! A peer that stops reading must not hold up everybody else.
//!
//! `send` used to await a bounded channel with no deadline. A full buffer is
//! drained by the connection's writer task, and that task is blocked writing
//! to the socket — so a tablet that walks out of WiFi range keeps its buffer
//! full until the OS gives up on the half-open socket, minutes later.
//! `send_all` joins across every peer, so for all of those minutes no
//! broadcast reached the rest of the floor.
//!
//! Every test here hangs forever without the timeout rather than failing, so
//! they are wrapped in a deadline of their own.

use std::sync::Arc;
use std::time::Duration;

use atomic_websocket::client_sender::{ClientSenders, ClientSendersTrait};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Longer than SEND_TIMEOUT (2s), short enough that a regression is a failing
/// test rather than a hung suite.
const DEADLINE: Duration = Duration::from_secs(8);

fn note() -> Message {
    Message::Text("x".into())
}

/// Fills a peer's buffer so the next send has nowhere to go, and keeps the
/// receiver alive — which is the case retrying could never fix, because a
/// live receiver means `send` waits rather than erroring.
async fn wedged_peer(senders: &Arc<ClientSenders>, peer: &str) -> mpsc::Receiver<Message> {
    let (tx, rx) = mpsc::channel(1);
    tx.send(note()).await.expect("the buffer starts empty");
    senders.add(peer, tx).await;
    rx
}

#[tokio::test]
async fn a_peer_that_stopped_reading_does_not_block_forever() {
    let senders = Arc::new(ClientSenders::new());
    let _rx = wedged_peer(&senders, "wedged").await;

    let started = std::time::Instant::now();
    let sent = tokio::time::timeout(DEADLINE, senders.send("wedged", note()))
        .await
        .expect("send must give up on its own rather than hang");

    assert!(!sent, "a peer that never drains is not a successful send");
    assert!(
        started.elapsed() < DEADLINE,
        "gave up after {:?}",
        started.elapsed()
    );
}

/// The one that matters for a dining room: one bad tablet, forty-seven good
/// ones, and a broadcast that has to reach them.
#[tokio::test]
async fn one_wedged_peer_does_not_hold_up_a_broadcast() {
    let senders: Arc<ClientSenders> = Arc::new(ClientSenders::new());
    let _wedged = wedged_peer(&senders, "wedged").await;

    let mut healthy = Vec::new();
    for i in 0..47 {
        let (tx, rx) = mpsc::channel(8);
        senders.add(&format!("peer{i}"), tx).await;
        healthy.push(rx);
    }

    tokio::time::timeout(DEADLINE, senders.send_all(note()))
        .await
        .expect("send_all must not be held open by one unreachable peer");

    for (i, rx) in healthy.iter_mut().enumerate() {
        assert!(
            rx.try_recv().is_ok(),
            "peer{i} should have got the broadcast"
        );
    }
}

/// Failing to send drops the peer, so the next broadcast does not pay for it
/// again. Without this a wedged tablet costs every broadcast the full timeout
/// for as long as it stays wedged.
#[tokio::test]
async fn a_peer_that_times_out_is_removed() {
    let senders: Arc<ClientSenders> = Arc::new(ClientSenders::new());
    let _rx = wedged_peer(&senders, "wedged").await;
    assert_eq!(senders.len(), 1);

    tokio::time::timeout(DEADLINE, senders.send("wedged", note()))
        .await
        .expect("send must give up on its own rather than hang");

    assert_eq!(
        senders.len(),
        0,
        "a peer that could not be sent to should have been dropped"
    );
}

/// A peer whose receiver is gone fails immediately — it should not spend the
/// timeout discovering what the channel already knows.
#[tokio::test]
async fn a_closed_peer_fails_at_once() {
    let senders = Arc::new(ClientSenders::new());
    let (tx, rx) = mpsc::channel(8);
    senders.add("gone", tx).await;
    drop(rx);

    let started = std::time::Instant::now();
    let sent = senders.send("gone", note()).await;

    assert!(!sent);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "a closed channel took {:?}; it should be immediate",
        started.elapsed()
    );
}
