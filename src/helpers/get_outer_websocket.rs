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
    let connector = TlsConnector::new().expect("Failed to create TLS connector");
    let connector = Connector::NativeTls(connector);

    let url = format!("wss://{}", &options.url);
    log_debug!("Connecting to WebSocket server: {:?}", &url);
    if let Ok((ws_stream, _)) =
        connect_async_tls_with_config(&url, None, false, Some(connector)).await
    {
        handle_websocket(db, server_sender, options, url.copy_string(), ws_stream).await?;
    }

    Ok(())
}

#[cfg(not(feature = "native_tls"))]
pub async fn get_outer_websocket(
    db: Arc<RwLock<Database<'static>>>,
    server_sender: Arc<RwLock<ServerSender>>,
    options: ClientOptions,
) -> tokio_tungstenite::tungstenite::Result<()> {
    use crate::log_debug;

    let url = format!("ws://{}", &options.url);
    log_debug!("Connecting to WebSocket server: {:?}", &url);
    if let Ok((ws_stream, _)) = connect_async(&url).await {
        handle_websocket(db, server_sender, options, url.copy_string(), ws_stream).await?;
    }

    Ok(())
}
