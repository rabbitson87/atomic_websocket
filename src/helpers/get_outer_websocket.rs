use super::types::{RwServerSender, DB};
use crate::{helpers::get_internal_websocket::handle_websocket, log_error};

use crate::{helpers::traits::StringUtil, server_sender::ClientOptions};
use std::time::Duration;
use tokio::time::timeout;

use crate::log_debug;

pub async fn wrap_get_outer_websocket(
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
) {
    match get_outer_websocket(db, server_sender, options).await {
        Ok(_) => (),
        Err(e) => {
            log_error!("Error getting websocket: {:?}", e);
        }
    }
}

#[cfg(feature = "rustls")]
pub async fn get_outer_websocket(
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
    use rustls::{ClientConfig, RootCertStore};
    use std::sync::Arc;
    use tokio_tungstenite::{connect_async_tls_with_config, Connector};

    let server_ip = format!("wss://{}", &options.url);

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
                server_ip.copy_string(),
                ws_stream,
            )
            .await?;
        }
        _ => {}
    }
    log_debug!("Failed to server connect to {}", server_ip);

    Ok(())
}

#[cfg(not(feature = "rustls"))]
pub async fn get_outer_websocket(
    db: DB,
    server_sender: RwServerSender,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
    use tokio_tungstenite::connect_async;

    let server_ip = format!("ws://{}", &options.url);
    log_debug!("Connecting to WebSocket server: {:?}", &server_ip);
    match timeout(
        Duration::from_secs(options.connect_timeout_seconds),
        connect_async(&server_ip),
    )
    .await
    {
        Ok(Ok((ws_stream, _))) => {
            handle_websocket(
                db,
                server_sender.clone(),
                options,
                server_ip.copy_string(),
                ws_stream,
            )
            .await?
        }
        _ => {}
    }
    log_debug!("Failed to server connect to {}", server_ip);

    Ok(())
}
