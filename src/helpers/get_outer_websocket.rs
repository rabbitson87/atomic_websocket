//! External WebSocket connection handling for atomic_websocket clients.
//!
//! This module provides functionality for establishing and maintaining WebSocket connections
//! to external servers, with optional TLS support through the rustls feature.

use super::types::{RwServerSender, DB};
use crate::{helpers::get_internal_websocket::handle_websocket, log_error};

use crate::server_sender::ClientOptions;
use std::time::Duration;
use tokio::time::timeout;

use crate::log_debug;

/// Wrapper function for establishing an external WebSocket connection.
///
/// Handles errors from the connection attempt and logs them appropriately.
///
/// # Arguments
///
/// * `db` - Database instance for storing connection state
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options, including the URL to connect to
pub async fn wrap_get_outer_websocket(
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
) {
    match get_outer_websocket(db, server_sender.clone(), options).await {
        Ok(_) => (),
        Err(e) => {
            log_error!("Error getting websocket: {:?}", e);
            server_sender.write().await.is_try_connect = false;
        }
    }
}

/// Establishes a WebSocket connection to an external server with TLS support.
///
/// This implementation is used when the `rustls` feature is enabled,
/// providing secure WebSocket connections (wss://).
///
/// # Arguments
///
/// * `db` - Database instance for storing connection state
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options, including the URL to connect to
///
/// # Returns
///
/// A Result indicating whether the connection process completed successfully
#[cfg(feature = "rustls")]
pub async fn get_outer_websocket(
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
    use rustls::{ClientConfig, RootCertStore};
    use std::sync::Arc;
    use tokio_tungstenite::{connect_async_tls_with_config, Connector};

    // Format the URL with 'wss://' scheme for secure WebSockets
    let server_ip = format!("wss://{}", &options.url);

    // Configure TLS with root certificates from webpki-roots
    let root_store = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };
    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let connector = Connector::Rustls(Arc::new(config));

    log_debug!("Connecting to WebSocket server: {:?}", &server_ip);
    match timeout(
        Duration::from_secs(options.connect_timeout_seconds),
        connect_async_tls_with_config(&server_ip, None, false, Some(connector)),
    )
    .await
    {
        Ok(Ok((ws_stream, _))) => {
            handle_websocket(
                db,
                server_sender.clone(),
                options,
                server_ip.clone(),
                ws_stream,
            )
            .await?;
        }
        _ => {}
    }
    log_debug!("Failed to server connect to {}", server_ip);

    Ok(())
}

/// Establishes a WebSocket connection to an external server without TLS.
///
/// This implementation is used when the `rustls` feature is not enabled,
/// providing basic WebSocket connections (ws://).
///
/// # Arguments
///
/// * `db` - Database instance for storing connection state
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options, including the URL to connect to
///
/// # Returns
///
/// A Result indicating whether the connection process completed successfully
#[cfg(not(feature = "rustls"))]
pub async fn get_outer_websocket(
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
    use tokio_tungstenite::connect_async;

    // Format the URL with 'ws://' scheme for standard WebSockets
    let server_ip = format!("ws://{}", &options.url);
    log_debug!("Connecting to WebSocket server: {:?}", &server_ip);
    if let Ok(Ok((ws_stream, _))) = timeout(
        Duration::from_secs(options.connect_timeout_seconds),
        connect_async(&server_ip),
    )
    .await
    {
        handle_websocket(
            db,
            server_sender.clone(),
            options,
            server_ip.clone(),
            ws_stream,
        )
        .await?
    }
    log_debug!("Failed to server connect to {}", server_ip);

    Ok(())
}
