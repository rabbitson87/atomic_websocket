use std::sync::Arc;

use crate::{helpers::get_internal_websocket::handle_websocket, log_error};
use native_db::Database;
#[cfg(feature = "native_tls")]
use native_tls::TlsConnector;
use tokio::sync::RwLock;

#[cfg(not(feature = "native_tls"))]
use tokio_tungstenite::connect_async;
#[cfg(feature = "native_tls")]
use tokio_tungstenite::{connect_async_tls_with_config, Connector};

use crate::{
    helpers::{server_sender::ServerSender, traits::StringUtil},
    server_sender::ClientOptions,
};
use std::time::Duration;
use tokio::time::timeout;

use crate::log_debug;

pub async fn wrap_get_outer_websocket(
    db: Arc<RwLock<Database<'static>>>,
    server_sender: Arc<RwLock<ServerSender>>,
    options: ClientOptions,
) {
    match get_outer_websocket(db, server_sender, options).await {
        Ok(_) => (),
        Err(e) => {
            log_error!("Error getting websocket: {:?}", e);
        }
    }
}

#[cfg(feature = "native_tls")]
pub async fn get_outer_websocket(
    db: Arc<RwLock<Database<'static>>>,
    server_sender: Arc<RwLock<ServerSender>>,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
    let server_ip = format!("wss://{}", &options.url);

    let connector = TlsConnector::new().expect("Failed to create TLS connector");
    let connector = Connector::NativeTls(connector);

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

#[cfg(not(feature = "native_tls"))]
pub async fn get_outer_websocket(
    db: Arc<RwLock<Database<'static>>>,
    server_sender: Arc<RwLock<ServerSender>>,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
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
