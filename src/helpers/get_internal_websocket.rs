//! Internal WebSocket connection handling for atomic_websocket clients.
//!
//! This module provides functionality for establishing and maintaining WebSocket
//! connections to servers, including connection setup, message handling, and
//! automatic reconnection logic.

use std::sync::{atomic::AtomicBool, Arc};

use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{sleep, timeout},
};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

#[cfg(feature = "bebop")]
use crate::generated::schema::Category;
#[cfg(feature = "bebop")]
use crate::helpers::common::get_data_schema;
use crate::{
    helpers::{
        common::{make_disconnect_message, make_ping_message},
        connection_store::ConnectionStore,
        server_sender::{register_connection, ConnectionTraffic, SenderStatus, ServerSenderTrait},
        traits::atomic::FlagAtomic,
    },
    log_debug, log_error, Settings,
};

use super::{
    internal_client::ClientOptions,
    types::{save_key, RwServerSender, DB},
};

/// How long a connection that is being ended gets to tell the server so.
///
/// Short on purpose: the connections that get ended are mostly the ones whose
/// socket has gone quiet, and the goodbye is a courtesy to a server that is
/// still there.
pub(crate) const STOP_NOTICE: Duration = Duration::from_millis(250);

/// How long a connection that has ended gets to flush what it already wrote.
pub(crate) const FINAL_FLUSH: Duration = Duration::from_secs(1);

/// Guard that owns the `is_try_connect` single-flight lock for the lifetime of
/// one connection attempt, and resets it to `false` on drop.
///
/// Must be acquired via [`TryConnectGuard::try_acquire`] *before* the
/// handshake (`connect_async`) starts, not after it succeeds — otherwise a
/// concurrent reconnect trigger (the periodic ping-loop checker, an app-level
/// `get_internal_connect`/`get_outer_connect` call, or `ServerSender::send`'s
/// exhausted-retry reconnect) can slip through and start its own connection
/// attempt during the handshake window.
///
/// Also ensures the flag is always reset even if the connection handler
/// panics or exits early, preventing permanent connection lockout.
pub(crate) struct TryConnectGuard {
    server_sender: RwServerSender,
    disarmed: bool,
}

impl TryConnectGuard {
    /// Atomically claims the single-flight guard for a new connection
    /// attempt: `false` -> `true` under one write lock. Returns `None` if a
    /// connection attempt is already in progress.
    pub(crate) async fn try_acquire(server_sender: RwServerSender) -> Option<Self> {
        let mut guard = server_sender.write().await;
        if guard.is_try_connect {
            return None;
        }
        guard.is_try_connect = true;
        drop(guard);
        Some(Self {
            server_sender,
            disarmed: false,
        })
    }

    /// Disarm the guard to prevent redundant reset on normal exit.
    pub(crate) fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for TryConnectGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            let server_sender = self.server_sender.clone();
            tokio::spawn(async move {
                server_sender.write().await.is_try_connect = false;
            });
        }
    }
}

/// Wrapper function for establishing an internal WebSocket connection.
///
/// Handles errors from the connection attempt and logs them appropriately.
///
/// # Arguments
///
/// * `connection_store` - Persistence for connection-identity state
/// * `server_sender` - Server sender for message handling
/// * `server_ip` - Server address to connect to
/// * `options` - Client connection options
///
/// # Returns
///
/// `true` if the connection was successfully established, `false` otherwise
pub async fn wrap_get_internal_websocket(
    connection_store: Arc<dyn ConnectionStore>,
    server_sender: RwServerSender,
    server_ip: String,
    options: ClientOptions,
) -> bool {
    match get_internal_websocket(connection_store, server_sender, server_ip, options).await {
        Ok(_) => true,
        Err(e) => {
            log_error!("Error getting websocket: {:?}", e);
            false
        }
    }
}

/// Establishes a WebSocket connection to an internal server.
///
/// Attempts to connect to the specified server with a timeout, then hands off
/// the connection to the WebSocket handler if successful.
///
/// # Arguments
///
/// * `connection_store` - Persistence for connection-identity state
/// * `server_sender` - Server sender for message handling
/// * `server_ip` - Server address to connect to
/// * `options` - Client connection options
///
/// # Returns
///
/// A Result indicating whether the connection process completed successfully
pub async fn get_internal_websocket(
    connection_store: Arc<dyn ConnectionStore>,
    server_sender: RwServerSender,
    server_ip: String,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
    // Claim the single-flight guard *before* dialing, not after the handshake
    // succeeds — otherwise a concurrent reconnect trigger can start its own
    // connect_async during this window. If another attempt already owns it,
    // there's nothing for this call to do.
    let Some(connect_guard) = TryConnectGuard::try_acquire(server_sender.clone()).await else {
        return Ok(());
    };

    log_debug!("Connecting to {}", server_ip);
    match timeout(
        Duration::from_secs(options.connect_timeout_seconds),
        connect_async(&server_ip),
    )
    .await
    {
        Ok(Ok((ws_stream, _))) => {
            if let Err(err) = handle_websocket(
                connection_store,
                server_sender.clone(),
                options,
                server_ip.clone(),
                ws_stream,
                connect_guard,
            )
            .await
            {
                log_error!("Error handling websocket: {:?}", err);
            }
        }
        Err(e) => {
            // connect_guard drops here, resetting is_try_connect automatically.
            if worth_logging(dial_failed(&server_sender, &options, &server_ip).await) {
                log_error!("Error connecting to {}: {:?}", server_ip, e);
            }
        }
        Ok(Err(e)) => {
            if worth_logging(dial_failed(&server_sender, &options, &server_ip).await) {
                log_error!("Error connecting to {}: {:?}", server_ip, e);
            }
        }
    }
    log_debug!("Connection session ended for {}", server_ip);
    Ok(())
}

/// Whether the server has said anything within two retry periods — the
/// measure `is_valid_server_ip` uses for a connection that is alive.
async fn heard_recently(server_sender: &RwServerSender, retry_seconds: u64) -> bool {
    let window = Duration::from_secs(retry_seconds.max(1) * 2);
    server_sender
        .read()
        .await
        .heard_ago()
        .is_some_and(|ago| ago < window)
}

/// What a connection does when this end closes it: writes what `send` had
/// already accepted, then tells the server goodbye — as far as the socket
/// takes them within [`STOP_NOTICE`].
///
/// The queue first, because a caller that sends its last message and then
/// disconnects has been told the message was accepted. Not getting it all
/// out in time is not an error: the socket is closed right after either way.
async fn wind_down<S>(
    ostream: &mut S,
    rx: &mut mpsc::Receiver<tokio_tungstenite::tungstenite::Message>,
    server_ip: &str,
) where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message> + Unpin,
{
    let _ = timeout(STOP_NOTICE, async {
        while let Ok(message) = rx.try_recv() {
            if ostream.send(message).await.is_err() {
                return;
            }
        }
        let _ = ostream.send(make_disconnect_message(server_ip)).await;
    })
    .await;
}

/// What a dial that did not get through does to the address it dialed.
///
/// A client that does not keep its address forgets it, in memory and in the
/// store, and is back to discovery or to being told where the server is.
///
/// A client that keeps its address (`use_keep_ip`) keeps it here too. It used
/// to lose it like the other: one attempt while the server was restarting —
/// or was out of reach for a few seconds — and the saved address was gone,
/// leaving a subnet scan, or nothing at all, as the way back. The failure is
/// counted instead, so that discovery can be brought in beside the dials when
/// the address keeps failing (see `get_internal_connect`).
///
/// Returns how many dials of the kept address have now failed in a row; zero
/// for a client that does not keep one.
async fn dial_failed(
    server_sender: &RwServerSender,
    options: &ClientOptions,
    server_ip: &str,
) -> u32 {
    if !options.use_keep_ip {
        server_sender.remove_ip_if_valid_server_ip(server_ip).await;
        return 0;
    }
    let mut guard = server_sender.write().await;
    // Whatever connection was registered before this dial is over; only the
    // address stays.
    guard.drop_connection();
    guard.failed_dials = guard.failed_dials.saturating_add(1);
    guard.failed_dials
}

/// How many failed dials apart the kept address is written to the log again.
const LOG_EVERY_FAILED_DIALS: u32 = 20;

/// Whether this failed dial is worth a line in the log.
///
/// A client that forgets its address fails once per address and always was
/// logged. One that keeps it fails again on every attempt for as long as the
/// server is away — every few seconds, all night, each line carrying the
/// server's address. The first failure says what happened; after that, one
/// line in [`LOG_EVERY_FAILED_DIALS`] says it is still happening.
fn worth_logging(failed_dials: u32) -> bool {
    failed_dials <= 1 || failed_dials % LOG_EVERY_FAILED_DIALS == 0
}

/// Handles an established WebSocket connection.
///
/// Sets up bidirectional message handling between the client and server,
/// including automatic ping/pong for connection health monitoring.
///
/// # Arguments
///
/// * `connection_store` - Persistence for connection-identity state
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options
/// * `server_ip` - Server address connected to
/// * `ws_stream` - Established WebSocket stream
///
/// # Returns
///
/// A Result indicating whether the connection handling completed successfully
///
/// `connect_guard` must already hold the `is_try_connect` single-flight lock
/// (acquired by the caller before the handshake started); this function owns
/// it for the life of the connection and disarms it on normal exit.
#[cfg(feature = "bebop")]
pub async fn handle_websocket(
    connection_store: Arc<dyn ConnectionStore>,
    server_sender: RwServerSender,
    options: ClientOptions,
    server_ip: String,
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut connect_guard: TryConnectGuard,
) -> tokio_tungstenite::tungstenite::Result<()> {
    let (mut ostream, mut istream) = ws_stream.split();
    log_debug!("Connected to {} for web socket", server_ip);

    let (sx, mut rx) = mpsc::channel(options.per_connection_buffer_size);
    let id = connection_store.get_client_id().await;

    // The server learns who this client is from the first frame, and closes
    // a connection that starts with anything else. The ping is queued before
    // the connection is registered: until then nothing else can reach this
    // queue, so nothing the application sends gets in ahead of it.
    let use_ping = options.use_ping;
    let traffic = Arc::new(ConnectionTraffic::default());
    if use_ping {
        log_debug!("Client send message: {:?}", make_ping_message(&id));
        traffic.ping_queued();
        if sx.send(make_ping_message(&id)).await.is_ok() {
            server_sender.read().await.metrics.inc_messages_sent();
        }
    }
    let (stop, connection) =
        register_connection(&server_sender, sx.clone(), &server_ip, traffic.clone()).await;

    let mut is_first = true;
    if !use_ping {
        is_first = false;
        server_sender.write_received_times().await;
        server_sender.send_status(SenderStatus::Connected).await;
    }

    let retry_seconds = options.retry_seconds;
    let server_sender_clone = server_sender.clone();
    let server_ip_clone = server_ip.clone();
    let (stream_end_tx, mut stream_end_rx) = tokio::sync::oneshot::channel::<()>();

    // Spawn a task to handle incoming messages
    let mut reader = tokio::spawn(async move {
        let server_ip = server_ip_clone;
        let server_sender = server_sender_clone;
        let is_wait_ping = Arc::new(AtomicBool::new(false));

        while let Some(Ok(message)) = istream.next().await {
            server_sender.write_received_times().await;
            // Only a binary frame is one of ours: a websocket ping, pong,
            // close or text frame whose payload happens to begin like one of
            // our messages is not. It still says the server is there.
            if !matches!(message, tokio_tungstenite::tungstenite::Message::Binary(_)) {
                continue;
            }
            let value = message.into_data();
            let data = match get_data_schema(&value) {
                Ok(data) => data,
                Err(e) => {
                    log_error!("Error getting data schema: {:?}", e);
                    continue;
                }
            };

            // Counted before anything is told about the connection, so that
            // what is read after `Connected` already includes this pong.
            let is_pong = data.category == Category::Pong as u16;
            if is_pong {
                traffic.pong_received();
            }
            if is_first {
                is_first = false;
                server_sender.send_status(SenderStatus::Connected).await;
            }
            let id = id.clone();
            log_debug!("Client receive message: {:?}", data);
            if is_pong {
                if !is_wait_ping.is_true() {
                    is_wait_ping.set_bool(true);
                    let server_sender_clone = server_sender.clone();
                    let is_wait_ping_clone = is_wait_ping.clone();
                    // Schedule the next ping after receiving a pong. On this
                    // connection only: if it has been replaced by then, the
                    // new one has pings of its own, and its first frame must
                    // be its own.
                    tokio::spawn(async move {
                        sleep(Duration::from_secs(retry_seconds)).await;
                        is_wait_ping_clone.set_bool(false);
                        server_sender_clone
                            .send_to(make_ping_message(&id), connection)
                            .await;
                    });
                }
                continue;
            } else if data.category == Category::Disconnect as u16 {
                // Parse server IP from URL format (e.g., "ws://192.168.1.1:8080")
                let peer = server_ip
                    .split("://")
                    .nth(1)
                    .and_then(|s| s.split(':').next())
                    .unwrap_or(&server_ip);
                let _ = sx.send(make_disconnect_message(peer)).await;
                break;
            }
            server_sender.send_handle_message(data).await;
        }
        // Notify writer that the read stream has ended (server disconnected)
        let _ = stream_end_tx.send(());
    });

    // Handle outgoing messages, also watching for reader stream end and for
    // this end giving the connection up.
    let mut parked = false;
    'writer: loop {
        tokio::select! {
            // In this order: once this end has given the connection up,
            // nothing new is started.
            biased;
            _ = stop.cancelled() => {
                wind_down(&mut ostream, &mut rx, &server_ip).await;
                break;
            }
            msg = rx.recv() => {
                match msg {
                    Some(message) => {
                        // A socket that has stopped taking writes parks this
                        // await for as long as the OS allows, and nothing
                        // queued behind it is ever looked at. The stop signal
                        // has to reach the connection here too — but only a
                        // write that is really parked is given up: one that
                        // is merely in flight gets the same short notice the
                        // goodbye does.
                        let write = ostream.send(message.clone());
                        tokio::pin!(write);
                        let sent = tokio::select! {
                            biased;
                            sent = &mut write => sent,
                            _ = stop.cancelled() => match timeout(STOP_NOTICE, &mut write).await {
                                Ok(sent) => sent,
                                Err(_) => {
                                    parked = true;
                                    break 'writer;
                                }
                            },
                        };
                        match sent {
                            Ok(_) => {
                                let data = message.into_data();
                                let data = match get_data_schema(&data) {
                                    Ok(data) => data,
                                    Err(e) => {
                                        log_error!("Error getting data schema: {:?}", e);
                                        rx.close();
                                        break;
                                    }
                                };
                                log_debug!("Send message: {:?}", data);
                                if data.category == Category::Disconnect as u16 {
                                    rx.close();
                                    break;
                                }
                            }
                            Err(e) => {
                                log_error!("Error sending message: {:?}", e);
                                break;
                            }
                        }
                    }
                    None => break,
                }
            }
            _ = &mut stream_end_rx => {
                // Reader stream ended — connection is dead
                break;
            }
        }
    }
    log_debug!("WebSocket closed");
    // The writer is done, so the connection is over — and the reader has to
    // go with it. When this end was the one that gave up (the checker dropped
    // a connection that had gone silent, or the application disconnected) the
    // reader is still parked on a socket nothing may ever arrive on; left
    // alone it keeps that socket open for as long as the other side does, and
    // stamps this client "heard from the server" if anything ever does arrive.
    if parked {
        // A socket that would not even take the write that was in flight:
        // there is nothing to flush that a flush could get out, and nobody
        // on the other side to wait for.
        reader.abort();
    } else {
        let _ = timeout(FINAL_FLUSH, ostream.flush()).await;
        // A server that has read the goodbye hangs up, and the reader ends
        // with that. It is given a moment to: closing first, with what the
        // server sent still unread, makes the OS reset the connection, and a
        // reset can take the last messages this end wrote with it.
        //
        // Only worth the moment on a connection that is talking. One that
        // has gone quiet has nothing unread to reset over — and it is the
        // kind that gets ended by whoever cannot spare a quarter of a second
        // (the checker, an application's supervisor after a wake).
        let hung_up = heard_recently(&server_sender, options.retry_seconds).await
            && timeout(STOP_NOTICE, &mut reader).await.is_ok();
        if !hung_up {
            reader.abort();
        }
    }
    // Normal exit: reset flag directly and disarm guard
    server_sender.write().await.is_try_connect = false;
    connect_guard.disarm();
    Ok(())
}

/// Handles an established WebSocket connection (raw bytes version).
#[cfg(not(feature = "bebop"))]
pub async fn handle_websocket(
    connection_store: Arc<dyn ConnectionStore>,
    server_sender: RwServerSender,
    options: ClientOptions,
    server_ip: String,
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut connect_guard: TryConnectGuard,
) -> tokio_tungstenite::tungstenite::Result<()> {
    let _ = connection_store;
    let (mut ostream, mut istream) = ws_stream.split();
    log_debug!("Connected to {} for web socket", server_ip);

    let (sx, mut rx) = mpsc::channel(options.per_connection_buffer_size);
    let (stop, _connection) =
        register_connection(&server_sender, sx.clone(), &server_ip, Default::default()).await;

    // Without bebop there is no pong handshake, so emit Connected immediately
    // and set the initial received timestamp for the loop checker.
    server_sender.write_received_times().await;
    server_sender.send_status(SenderStatus::Connected).await;

    let server_sender_clone = server_sender.clone();
    let (stream_end_tx, mut stream_end_rx) = tokio::sync::oneshot::channel::<()>();

    // Spawn a task to handle incoming messages - pass raw bytes
    let mut reader = tokio::spawn(async move {
        let server_sender = server_sender_clone;

        while let Some(Ok(message)) = istream.next().await {
            server_sender.write_received_times().await;
            let value = message.into_data();
            server_sender.send_handle_message(value.to_vec()).await;
        }
        // Notify writer that the read stream has ended (server disconnected)
        let _ = stream_end_tx.send(());
    });

    // Handle outgoing messages, also watching for reader stream end and for
    // this end giving the connection up. Without bebop there is no message
    // the writer could recognise as "stop", so the signal is the only way.
    let mut parked = false;
    'writer: loop {
        tokio::select! {
            // In this order: once this end has given the connection up,
            // nothing new is started.
            biased;
            _ = stop.cancelled() => {
                wind_down(&mut ostream, &mut rx, &server_ip).await;
                break;
            }
            msg = rx.recv() => {
                match msg {
                    Some(message) => {
                        // See the bebop variant: a parked write must not hide
                        // the stop signal, and only a parked one is given up.
                        let write = ostream.send(message);
                        tokio::pin!(write);
                        let sent = tokio::select! {
                            biased;
                            sent = &mut write => sent,
                            _ = stop.cancelled() => match timeout(STOP_NOTICE, &mut write).await {
                                Ok(sent) => sent,
                                Err(_) => {
                                    parked = true;
                                    break 'writer;
                                }
                            },
                        };
                        if let Err(e) = sent {
                            log_error!("Error sending message: {:?}", e);
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = &mut stream_end_rx => {
                // Reader stream ended — connection is dead
                break;
            }
        }
    }
    log_debug!("WebSocket closed");
    // The writer is done, so the connection is over — and the reader has to
    // go with it. When this end was the one that gave up (the checker dropped
    // a connection that had gone silent, or the application disconnected) the
    // reader is still parked on a socket nothing may ever arrive on; left
    // alone it keeps that socket open for as long as the other side does, and
    // stamps this client "heard from the server" if anything ever does arrive.
    if parked {
        // A socket that would not even take the write that was in flight:
        // there is nothing to flush that a flush could get out, and nobody
        // on the other side to wait for.
        reader.abort();
    } else {
        let _ = timeout(FINAL_FLUSH, ostream.flush()).await;
        // A server that has read the goodbye hangs up, and the reader ends
        // with that. It is given a moment to: closing first, with what the
        // server sent still unread, makes the OS reset the connection, and a
        // reset can take the last messages this end wrote with it.
        //
        // Only worth the moment on a connection that is talking. One that
        // has gone quiet has nothing unread to reset over — and it is the
        // kind that gets ended by whoever cannot spare a quarter of a second
        // (the checker, an application's supervisor after a wake).
        let hung_up = heard_recently(&server_sender, options.retry_seconds).await
            && timeout(STOP_NOTICE, &mut reader).await.is_ok();
        if !hung_up {
            reader.abort();
        }
    }
    // Normal exit: reset flag directly and disarm guard
    server_sender.write().await.is_try_connect = false;
    connect_guard.disarm();
    Ok(())
}

/// Retrieves the client identifier from the database.
///
/// # Arguments
///
/// * `db` - Database instance
///
/// # Returns
///
/// The client identifier as a string, or empty string if not found
#[cfg(feature = "native-db")]
pub async fn get_id(db: DB) -> String {
    // Run the synchronous redb read on the blocking pool so it never stalls a
    // Tokio worker thread (see `common::flatten_join` for the rationale).
    tokio::task::spawn_blocking(move || {
        let db = db.blocking_lock();
        let Ok(reader) = db.r_transaction() else {
            return String::new();
        };

        let Ok(Some(data)) = reader.get().primary::<Settings>(save_key::CLIENT_ID) else {
            return String::new();
        };

        String::from_utf8(data.value).unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

/// Retrieves the client identifier from in-memory storage.
#[cfg(not(feature = "native-db"))]
pub async fn get_id(db: DB) -> String {
    let db = db.lock().await;
    db.get(save_key::CLIENT_ID)
        .map(|v| String::from_utf8(v.clone()).unwrap_or_default())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client that forgets its address reports zero and is logged every
    /// time, as it always was.
    #[test]
    fn a_failed_dial_without_a_kept_address_is_always_logged() {
        assert!(worth_logging(0));
    }

    /// The first failure says what happened; then one line in twenty, not
    /// one every few seconds for the whole night.
    #[test]
    fn a_kept_address_that_keeps_failing_is_logged_now_and_then() {
        let logged: Vec<u32> = (1..=60).filter(|n| worth_logging(*n)).collect();
        assert_eq!(logged, vec![1, 20, 40, 60]);
    }
}
