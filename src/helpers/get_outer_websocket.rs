//! External WebSocket connection handling for atomic_websocket clients.
//!
//! This module provides functionality for establishing and maintaining WebSocket connections
//! to external servers, with optional TLS support through the rustls feature.

use std::sync::Arc;

use super::types::RwServerSender;
use crate::{
    helpers::{
        connection_store::ConnectionStore,
        get_internal_websocket::{handle_websocket, TryConnectGuard},
    },
    log_error,
};

use crate::server_sender::ClientOptions;
use std::time::Duration;
use tokio::time::timeout;

use crate::log_debug;

/// Resolves the URL this client should dial.
///
/// A URL that names its own scheme is used exactly as written — that is the
/// supported way to choose between plaintext and TLS, and it is why there is no
/// `use_tls` option: the address already says which one it is.
///
/// A scheme-less address is completed from the build, `wss://` with the
/// `rustls` feature and `ws://` without it. That default exists for
/// convenience, but relying on it means the same configuration speaks a
/// different protocol depending on how the binary was compiled, and the symptom
/// — a TLS handshake error against a plaintext server — points at certificates
/// rather than at the address. Callers should name the scheme.
pub(crate) fn outer_ws_url(url: &str) -> String {
    if url.starts_with("ws://") || url.starts_with("wss://") {
        return url.to_owned();
    }
    #[cfg(feature = "rustls")]
    {
        format!("wss://{url}")
    }
    #[cfg(not(feature = "rustls"))]
    {
        format!("ws://{url}")
    }
}

/// Wrapper function for establishing an external WebSocket connection.
///
/// Handles errors from the connection attempt and logs them appropriately.
///
/// # Arguments
///
/// * `connection_store` - Persistence for connection-identity state
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options, including the URL to connect to
pub async fn wrap_get_outer_websocket(
    connection_store: Arc<dyn ConnectionStore>,
    server_sender: RwServerSender,
    options: ClientOptions,
) {
    match get_outer_websocket(connection_store, server_sender.clone(), options).await {
        Ok(_) => (),
        Err(e) => {
            // is_try_connect is owned by TryConnectGuard inside
            // get_outer_websocket/handle_websocket and resets itself on
            // drop — no manual reset needed (and doing one here risks
            // clobbering a different, already-in-flight attempt).
            log_error!("Error getting websocket: {:?}", e);
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
/// * `connection_store` - Persistence for connection-identity state
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options, including the URL to connect to
///
/// # Returns
///
/// A Result indicating whether the connection process completed successfully
#[cfg(feature = "rustls")]
pub async fn get_outer_websocket(
    connection_store: Arc<dyn ConnectionStore>,
    server_sender: RwServerSender,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
    use rustls::{ClientConfig, RootCertStore};
    use tokio_tungstenite::{connect_async_tls_with_config, Connector};

    // Claim the single-flight guard before dialing (see `TryConnectGuard`).
    let Some(connect_guard) = TryConnectGuard::try_acquire(server_sender.clone()).await else {
        return Ok(());
    };

    let server_ip = outer_ws_url(&options.url);

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
                connection_store,
                server_sender.clone(),
                options,
                server_ip.clone(),
                ws_stream,
                connect_guard,
            )
            .await?;
        }
        Ok(Err(e)) => {
            // connect_guard drops here, resetting is_try_connect automatically.
            log_debug!("Failed to connect to {}: {:?}", server_ip, e);
        }
        Err(_) => {
            log_debug!("Connection timeout to {}", server_ip);
        }
    }

    Ok(())
}

/// Establishes a WebSocket connection to an external server without TLS.
///
/// This implementation is used when the `rustls` feature is not enabled,
/// providing basic WebSocket connections (ws://).
///
/// # Arguments
///
/// * `connection_store` - Persistence for connection-identity state
/// * `server_sender` - Server sender for message handling
/// * `options` - Client connection options, including the URL to connect to
///
/// # Returns
///
/// A Result indicating whether the connection process completed successfully
#[cfg(not(feature = "rustls"))]
pub async fn get_outer_websocket(
    connection_store: Arc<dyn ConnectionStore>,
    server_sender: RwServerSender,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
    use tokio_tungstenite::connect_async;

    // Claim the single-flight guard before dialing (see `TryConnectGuard`).
    let Some(connect_guard) = TryConnectGuard::try_acquire(server_sender.clone()).await else {
        return Ok(());
    };

    let server_ip = outer_ws_url(&options.url);
    log_debug!("Connecting to WebSocket server: {:?}", &server_ip);
    if let Ok(Ok((ws_stream, _))) = timeout(
        Duration::from_secs(options.connect_timeout_seconds),
        connect_async(&server_ip),
    )
    .await
    {
        handle_websocket(
            connection_store,
            server_sender.clone(),
            options,
            server_ip.clone(),
            ws_stream,
            connect_guard,
        )
        .await?
    }
    // If the connect attempt failed/timed out, connect_guard drops here,
    // resetting is_try_connect automatically.
    log_debug!("Failed to server connect to {}", server_ip);

    Ok(())
}

#[cfg(test)]
mod outer_ws_url_tests {
    use super::outer_ws_url;

    #[test]
    fn an_explicit_scheme_is_never_overridden() {
        // Both directions, under either feature: what the caller wrote stands.
        assert_eq!(outer_ws_url("ws://10.0.0.5:9000"), "ws://10.0.0.5:9000");
        assert_eq!(
            outer_ws_url("wss://license.jkpos365.com"),
            "wss://license.jkpos365.com"
        );
    }

    #[cfg(feature = "rustls")]
    #[test]
    fn a_scheme_less_address_defaults_to_tls_under_rustls() {
        assert_eq!(outer_ws_url("10.0.0.5:9000"), "wss://10.0.0.5:9000");
    }

    #[cfg(not(feature = "rustls"))]
    #[test]
    fn a_scheme_less_address_defaults_to_plaintext_without_rustls() {
        assert_eq!(outer_ws_url("10.0.0.5:9000"), "ws://10.0.0.5:9000");
    }
}
