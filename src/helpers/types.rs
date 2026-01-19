#[cfg(not(feature = "native-db"))]
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(feature = "native-db")]
use native_db::Database;
use tokio::sync::{Mutex, RwLock};

use super::{client_sender::ClientSenders, server_sender::ServerSender};
#[cfg(feature = "bebop")]
use crate::generated::schema::SaveKey;

#[cfg(feature = "native-db")]
pub type DB = Arc<Mutex<Database<'static>>>;

#[cfg(not(feature = "native-db"))]
pub type DB = Arc<Mutex<InMemoryStorage>>;

/// In-memory storage for when native-db feature is disabled.
/// Data is stored in a HashMap and is lost when the process exits.
#[cfg(not(feature = "native-db"))]
#[derive(Debug, Default)]
pub struct InMemoryStorage {
    data: HashMap<String, Vec<u8>>,
}

#[cfg(not(feature = "native-db"))]
impl InMemoryStorage {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&Vec<u8>> {
        self.data.get(key)
    }

    pub fn insert(&mut self, key: String, value: Vec<u8>) {
        self.data.insert(key, value);
    }

    pub fn remove(&mut self, key: &str) -> Option<Vec<u8>> {
        self.data.remove(key)
    }
}

pub type RwServerSender = Arc<RwLock<ServerSender>>;

pub type RwClientSenders = Arc<RwLock<ClientSenders>>;

/// SaveKey constant string keys for database operations.
/// These avoid repeated format!("{:?}", SaveKey::...) allocations.
pub mod save_key {
    pub const SERVER_CONNECT_INFO: &str = "ServerConnectInfo";
    pub const VALID_CLIENT: &str = "ValidClient";
    pub const CLIENT_ID: &str = "ClientId";
}

/// Extension trait for SaveKey to get string representation without allocation.
#[cfg(feature = "bebop")]
pub trait SaveKeyExt {
    fn as_str(&self) -> &'static str;
}

#[cfg(feature = "bebop")]
impl SaveKeyExt for SaveKey {
    #[inline]
    fn as_str(&self) -> &'static str {
        match self {
            SaveKey::ServerConnectInfo => save_key::SERVER_CONNECT_INFO,
            SaveKey::ValidClient => save_key::VALID_CLIENT,
            SaveKey::ClientId => save_key::CLIENT_ID,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_save_key_constants() {
        assert_eq!(save_key::SERVER_CONNECT_INFO, "ServerConnectInfo");
        assert_eq!(save_key::VALID_CLIENT, "ValidClient");
        assert_eq!(save_key::CLIENT_ID, "ClientId");
    }

    #[cfg(feature = "bebop")]
    #[test]
    fn test_save_key_ext() {
        assert_eq!(SaveKey::ServerConnectInfo.as_str(), "ServerConnectInfo");
        assert_eq!(SaveKey::ValidClient.as_str(), "ValidClient");
        assert_eq!(SaveKey::ClientId.as_str(), "ClientId");
    }

    #[cfg(feature = "bebop")]
    #[test]
    fn test_save_key_ext_matches_constants() {
        assert_eq!(
            SaveKey::ServerConnectInfo.as_str(),
            save_key::SERVER_CONNECT_INFO
        );
        assert_eq!(SaveKey::ValidClient.as_str(), save_key::VALID_CLIENT);
        assert_eq!(SaveKey::ClientId.as_str(), save_key::CLIENT_ID);
    }

    #[cfg(not(feature = "native-db"))]
    mod in_memory_tests {
        use super::*;

        #[test]
        fn test_in_memory_storage_new() {
            let storage = InMemoryStorage::new();
            assert!(storage.get("nonexistent").is_none());
        }

        #[test]
        fn test_in_memory_storage_default() {
            let storage = InMemoryStorage::default();
            assert!(storage.get("nonexistent").is_none());
        }

        #[test]
        fn test_in_memory_storage_insert_and_get() {
            let mut storage = InMemoryStorage::new();
            storage.insert("key1".to_string(), vec![1, 2, 3]);

            let value = storage.get("key1");
            assert!(value.is_some());
            assert_eq!(value.unwrap(), &vec![1, 2, 3]);
        }

        #[test]
        fn test_in_memory_storage_get_nonexistent() {
            let storage = InMemoryStorage::new();
            assert!(storage.get("nonexistent").is_none());
        }

        #[test]
        fn test_in_memory_storage_remove() {
            let mut storage = InMemoryStorage::new();
            storage.insert("key1".to_string(), vec![1, 2, 3]);

            let removed = storage.remove("key1");
            assert!(removed.is_some());
            assert_eq!(removed.unwrap(), vec![1, 2, 3]);
            assert!(storage.get("key1").is_none());
        }

        #[test]
        fn test_in_memory_storage_remove_nonexistent() {
            let mut storage = InMemoryStorage::new();
            let removed = storage.remove("nonexistent");
            assert!(removed.is_none());
        }

        #[test]
        fn test_in_memory_storage_overwrite() {
            let mut storage = InMemoryStorage::new();
            storage.insert("key1".to_string(), vec![1, 2, 3]);
            storage.insert("key1".to_string(), vec![4, 5, 6]);

            let value = storage.get("key1");
            assert_eq!(value.unwrap(), &vec![4, 5, 6]);
        }

        #[test]
        fn test_in_memory_storage_multiple_keys() {
            let mut storage = InMemoryStorage::new();
            storage.insert("key1".to_string(), vec![1]);
            storage.insert("key2".to_string(), vec![2]);
            storage.insert("key3".to_string(), vec![3]);

            assert_eq!(storage.get("key1").unwrap(), &vec![1]);
            assert_eq!(storage.get("key2").unwrap(), &vec![2]);
            assert_eq!(storage.get("key3").unwrap(), &vec![3]);
        }

        #[test]
        fn test_in_memory_storage_debug() {
            let storage = InMemoryStorage::new();
            let debug = format!("{:?}", storage);
            assert!(debug.contains("InMemoryStorage"));
        }
    }
}
