//! Pluggable persistence for the connection-identity state atomic_websocket
//! manages internally: the generated client ID and the last-known server
//! connect info (ip/port) used for reconnection.
//!
//! This is a narrower, purpose-built abstraction over the same `Settings`
//! table that [`crate::helpers::common::get_setting_by_key`] /
//! [`crate::helpers::common::set_setting`] operate on generically — it does
//! not replace those free functions, which remain available for arbitrary
//! application settings unrelated to the connection flow.

use async_trait::async_trait;

#[cfg(feature = "bebop")]
use crate::generated::schema::ServerConnectInfo;
#[cfg(feature = "bebop")]
use bebop::Record;

#[cfg(not(feature = "bebop"))]
use crate::helpers::common::remove_setting;
use crate::{
    helpers::{
        common::{get_setting_by_key, set_setting},
        types::{save_key, DB},
    },
    log_error, Settings,
};

/// Manages the connection-identity state atomic_websocket persists
/// internally, decoupling `ServerSender`/`AtomicClient` from any particular
/// storage backend. Implement this to back the state with your own
/// database; [`NativeDbConnectionStore`] is a ready-to-use implementation
/// backed by the library's own `DB`/`Settings` table.
#[async_trait]
pub trait ConnectionStore: Send + Sync {
    /// Ensures a client ID exists, generating and persisting one only if
    /// absent. Idempotent.
    async fn ensure_client_id(&self);

    /// Returns the persisted client ID, or `""` if none has been registered
    /// yet. Read-only — does not generate one.
    async fn get_client_id(&self) -> String;

    /// Returns the persisted server connect info as `(server_ip, port)`, or
    /// `None` if nothing has been stored (or it failed to deserialize).
    /// `server_ip` may be `""` when only a port has been reserved ahead of
    /// the first successful connection.
    async fn get_server_connect_info(&self) -> Option<(String, String)>;

    /// Upserts the server connect info. `server_ip` may be `""` to reserve
    /// a port before the IP is known.
    async fn set_server_connect_info(&self, server_ip: &str, port: &str);

    /// Clears the persisted server IP, keeping any other stored connection
    /// info (e.g. the port) intact where the backend supports it.
    async fn clear_server_ip(&self, server_ip: &str);
}

/// Default [`ConnectionStore`] backed by the library's own `DB`/`Settings`
/// table — a drop-in replacement for the old `db: DB`-threading pattern
/// that reuses the exact same [`get_setting_by_key`]/[`set_setting`]/
/// [`remove_setting`] logic (including their `spawn_blocking` handling of
/// the redb fsync), so adopting it is a behavior-preserving swap.
pub struct NativeDbConnectionStore {
    db: DB,
}

impl NativeDbConnectionStore {
    /// Creates a new store backed by the given database handle.
    pub fn new(db: DB) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ConnectionStore for NativeDbConnectionStore {
    async fn ensure_client_id(&self) {
        // Drop the `Box<dyn Error>` (not `Send`) before the next `.await` —
        // holding the raw `Result` across it would make this method's
        // future non-`Send`, which `async_trait` requires by default.
        let existing = get_setting_by_key(self.db.clone(), save_key::CLIENT_ID.to_owned())
            .await
            .ok()
            .flatten();
        if existing.is_none() {
            use nanoid::nanoid;
            if let Err(e) = set_setting(
                self.db.clone(),
                Settings {
                    key: save_key::CLIENT_ID.to_owned(),
                    value: nanoid!().as_bytes().to_vec(),
                },
            )
            .await
            {
                log_error!("Failed to persist ClientId: {:?}", e);
            }
        }
    }

    async fn get_client_id(&self) -> String {
        match get_setting_by_key(self.db.clone(), save_key::CLIENT_ID.to_owned()).await {
            Ok(Some(setting)) => String::from_utf8(setting.value).unwrap_or_default(),
            _ => String::new(),
        }
    }

    async fn get_server_connect_info(&self) -> Option<(String, String)> {
        let setting =
            get_setting_by_key(self.db.clone(), save_key::SERVER_CONNECT_INFO.to_owned())
                .await
                .ok()
                .flatten()?;

        #[cfg(feature = "bebop")]
        {
            match ServerConnectInfo::deserialize(&setting.value) {
                Ok(info) => Some((info.server_ip.to_owned(), info.port.to_owned())),
                Err(e) => {
                    log_error!("Failed to deserialize ServerConnectInfo: {:?}", e);
                    None
                }
            }
        }
        #[cfg(not(feature = "bebop"))]
        {
            Some((
                String::from_utf8(setting.value).unwrap_or_default(),
                String::new(),
            ))
        }
    }

    async fn set_server_connect_info(&self, server_ip: &str, port: &str) {
        #[cfg(feature = "bebop")]
        let value = {
            let mut buf = Vec::new();
            if let Err(e) = (ServerConnectInfo { server_ip, port }).serialize(&mut buf) {
                log_error!("Failed to serialize ServerConnectInfo: {:?}", e);
                return;
            }
            buf
        };
        #[cfg(not(feature = "bebop"))]
        let value = {
            let _ = port;
            server_ip.as_bytes().to_vec()
        };

        if let Err(e) = set_setting(
            self.db.clone(),
            Settings {
                key: save_key::SERVER_CONNECT_INFO.to_owned(),
                value,
            },
        )
        .await
        {
            log_error!("Failed to persist connection info: {:?}", e);
        }
    }

    async fn clear_server_ip(&self, _server_ip: &str) {
        #[cfg(feature = "bebop")]
        {
            let Some((_, port)) = self.get_server_connect_info().await else {
                return;
            };
            self.set_server_connect_info("", &port).await;
        }
        #[cfg(not(feature = "bebop"))]
        {
            if let Err(e) =
                remove_setting(self.db.clone(), save_key::SERVER_CONNECT_INFO.to_owned()).await
            {
                log_error!("Failed to remove connection info: {:?}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "native-db"))]
    use std::sync::Arc;
    #[cfg(not(feature = "native-db"))]
    use tokio::sync::Mutex;

    #[cfg(not(feature = "native-db"))]
    use crate::helpers::types::InMemoryStorage;

    #[cfg(not(feature = "native-db"))]
    fn create_test_db() -> DB {
        Arc::new(Mutex::new(InMemoryStorage::new()))
    }

    #[cfg(feature = "native-db")]
    fn create_test_db() -> DB {
        use native_db::{Builder, Models};
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let mut models = Models::new();
        models.define::<Settings>().unwrap();
        let models: &'static Models = Box::leak(Box::new(models));
        let temp = tempfile::NamedTempFile::new().unwrap();
        Arc::new(Mutex::new(
            Builder::new().create(models, temp.path()).unwrap(),
        ))
    }

    #[tokio::test]
    async fn test_ensure_client_id_generates_once() {
        let store = NativeDbConnectionStore::new(create_test_db());
        assert_eq!(store.get_client_id().await, "");

        store.ensure_client_id().await;
        let first_id = store.get_client_id().await;
        assert!(!first_id.is_empty());

        // Idempotent: calling again should not change the stored id.
        store.ensure_client_id().await;
        assert_eq!(store.get_client_id().await, first_id);
    }

    #[tokio::test]
    async fn test_server_connect_info_roundtrip() {
        let store = NativeDbConnectionStore::new(create_test_db());
        assert!(store.get_server_connect_info().await.is_none());

        store.set_server_connect_info("192.168.1.100", "9000").await;
        let (ip, port) = store.get_server_connect_info().await.unwrap();
        assert_eq!(ip, "192.168.1.100");
        #[cfg(feature = "bebop")]
        assert_eq!(port, "9000");
    }

    #[cfg(feature = "bebop")]
    #[tokio::test]
    async fn test_clear_server_ip_keeps_port() {
        let store = NativeDbConnectionStore::new(create_test_db());
        store.set_server_connect_info("192.168.1.100", "9000").await;

        store.clear_server_ip("192.168.1.100").await;

        let (ip, port) = store.get_server_connect_info().await.unwrap();
        assert_eq!(ip, "");
        assert_eq!(port, "9000");
    }

    #[cfg(not(feature = "bebop"))]
    #[tokio::test]
    async fn test_clear_server_ip_removes_setting() {
        let store = NativeDbConnectionStore::new(create_test_db());
        store.set_server_connect_info("192.168.1.100", "9000").await;

        store.clear_server_ip("192.168.1.100").await;

        assert!(store.get_server_connect_info().await.is_none());
    }
}
