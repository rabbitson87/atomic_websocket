//! Tests for fixed-IP behavior (P2/P3):
//!
//! * Auto-discovery is OFF by default — when the server IP is unknown the client
//!   reports `Disconnected` immediately (the app keeps running) instead of
//!   launching an open-ended subnet scan.
//! * A known fixed server IP connects directly, without depending on
//!   `get_ip_address()` / internet reachability.

#![cfg(all(feature = "native-db", feature = "bebop"))]

mod common;

use std::sync::Arc;
use std::time::Duration;

use atomic_websocket::common::set_setting;
use atomic_websocket::external::bebop::Record;
use atomic_websocket::external::native_db::{Builder, Models};
use atomic_websocket::schema::ServerConnectInfo;
use atomic_websocket::server_sender::{
    get_internal_connect, ClientOptions, SenderStatus, ServerSender, ServerSenderTrait,
};
use atomic_websocket::types::{save_key, RwServerSender, DB};
use atomic_websocket::Settings;
use tempfile::NamedTempFile;
use tokio::sync::{Mutex, RwLock};

use common::{wait_for_status, TestServer};

/// Builds a fresh on-disk native-db. Keep the returned temp file alive.
fn make_native_db() -> (NamedTempFile, DB) {
    let mut models = Models::new();
    models.define::<Settings>().unwrap();
    let models: &'static Models = Box::leak(Box::new(models));

    let temp = NamedTempFile::new().unwrap();
    let db = Builder::new().create(models, temp.path()).unwrap();
    (temp, Arc::new(Mutex::new(db)))
}

async fn make_server_sender(db: DB, options: ClientOptions) -> RwServerSender {
    let mut ss: RwServerSender = Arc::new(RwLock::new(ServerSender::new(
        db,
        "".into(),
        options,
    )));
    let clone = ss.clone();
    ServerSenderTrait::regist(&mut ss, clone).await;
    ss
}

/// Persists a fixed server IP into the DB exactly as the library would.
async fn store_server_ip(db: &DB, server_ip: &str, port: &str) {
    let mut buf = Vec::new();
    ServerConnectInfo { server_ip, port }
        .serialize(&mut buf)
        .unwrap();
    set_setting(
        db.clone(),
        Settings {
            key: save_key::SERVER_CONNECT_INFO.to_owned(),
            value: buf,
        },
    )
    .await
    .unwrap();
}

/// With discovery disabled (the default) and no known IP, the client must NOT
/// scan: it returns promptly and reports `Disconnected` so the app keeps running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_auto_scan_when_discovery_disabled() {
    let (_temp, db) = make_native_db();
    let options = ClientOptions::default(); // use_scan_discovery == false
    let ss = make_server_sender(db.clone(), options.clone()).await;

    let mut rx = ss.get_status_receiver().await.expect("status receiver");

    let port = "9000".to_string();
    // Must return quickly — a regression to auto-scan would hang here.
    let call = get_internal_connect(
        Some(ServerConnectInfo {
            server_ip: "",
            port: &port,
        }),
        db.clone(),
        ss.clone(),
        options.clone(),
    );
    tokio::time::timeout(Duration::from_secs(3), call)
        .await
        .expect("get_internal_connect hung — auto-scan regression?")
        .expect("get_internal_connect returned Err");

    // Should report Disconnected, never Connecting (no scan was started).
    let got_disconnected = wait_for_status(
        &mut rx,
        SenderStatus::Disconnected,
        Duration::from_secs(2),
    )
    .await;
    assert!(
        got_disconnected,
        "expected Disconnected when discovery is disabled and no IP is known"
    );
}

/// A known fixed server IP connects directly (no scan, no internet dependency).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_fixed_ip_connects() {
    let port = common::find_available_port().await;
    let server = TestServer::start(port).await;

    let (_temp, db) = make_native_db();
    let options = ClientOptions::default();
    let ss = make_server_sender(db.clone(), options.clone()).await;

    // Pre-store the fixed server IP, as a deployed tablet would have saved it.
    let url = format!("ws://127.0.0.1:{port}");
    let port_str = port.to_string();
    store_server_ip(&db, &url, &port_str).await;

    let mut rx = ss.get_status_receiver().await.expect("status receiver");

    get_internal_connect(
        Some(ServerConnectInfo {
            server_ip: "",
            port: &port_str,
        }),
        db.clone(),
        ss.clone(),
        options.clone(),
    )
    .await
    .expect("get_internal_connect returned Err");

    let connected =
        wait_for_status(&mut rx, SenderStatus::Connected, Duration::from_secs(5)).await;
    assert!(connected, "direct fixed-IP connect should reach Connected");

    server.shutdown().await;
}

/// Regression: the scan-discovery path must be single-flight.
///
/// Before this guard, `is_try_connect` only became true once a connection was
/// established, so while `ScanManager` was still searching (forever, when no
/// server exists) every repeated `get_internal_connect` call passed the
/// re-entrancy guard and started ANOTHER unbounded scan. Each scan held a
/// subnet's worth of in-flight sockets, so a retry/registration loop leaked
/// SYN_SENT sockets until the machine's ephemeral ports were exhausted.
///
/// This asserts the flag-level invariant without touching the network: while a
/// scan is already in progress (`is_scanning == true`), a second call returns
/// immediately instead of entering the scan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_discovery_is_single_flight() {
    let (_temp, db) = make_native_db();
    let mut options = ClientOptions::default();
    options.use_scan_discovery = true;
    // Long timeout: if the guard failed to engage, the call would enter the scan
    // and block far beyond the 3s assertion window below.
    options.scan_timeout_seconds = 60;
    let ss = make_server_sender(db.clone(), options.clone()).await;

    // Simulate a scan already in progress.
    ss.write().await.is_scanning = true;

    let port = "16250".to_string();
    let call = get_internal_connect(
        Some(ServerConnectInfo {
            server_ip: "",
            port: &port,
        }),
        db.clone(),
        ss.clone(),
        options.clone(),
    );

    // With single-flight the call returns immediately; without it, it would start
    // a second scan and block until the 60s timeout.
    let res = tokio::time::timeout(Duration::from_secs(3), call).await;
    assert!(
        res.is_ok(),
        "get_internal_connect started a second concurrent scan instead of single-flighting"
    );
    res.unwrap().expect("get_internal_connect returned Err");

    // The guard must not clear another scan's in-progress flag.
    assert!(
        ss.read().await.is_scanning,
        "single-flight guard wrongly cleared the in-progress scan flag"
    );
}
