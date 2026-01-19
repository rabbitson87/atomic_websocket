//! Feature-specific integration tests.
//!
//! Tests that verify functionality depending on specific feature flags.

mod common;

use atomic_websocket::types::save_key;

// ============================================================================
// Save Key Constants
// ============================================================================

#[test]
fn test_save_key_constants() {
    assert_eq!(save_key::SERVER_CONNECT_INFO, "ServerConnectInfo");
    assert_eq!(save_key::VALID_CLIENT, "ValidClient");
    assert_eq!(save_key::CLIENT_ID, "ClientId");
}

// ============================================================================
// Bebop Feature Tests
// ============================================================================

#[cfg(feature = "bebop")]
mod bebop_tests {
    use atomic_websocket::schema::{Category, SaveKey};
    use atomic_websocket::types::save_key;
    use atomic_websocket::types::SaveKeyExt;

    #[test]
    fn test_save_key_ext_trait() {
        assert_eq!(
            SaveKey::ServerConnectInfo.as_str(),
            save_key::SERVER_CONNECT_INFO
        );
        assert_eq!(SaveKey::ValidClient.as_str(), save_key::VALID_CLIENT);
        assert_eq!(SaveKey::ClientId.as_str(), save_key::CLIENT_ID);
    }

    #[test]
    fn test_category_values() {
        assert_eq!(Category::Ping as u16, 10000);
        assert_eq!(Category::Pong as u16, 10001);
        assert_eq!(Category::Expired as u16, 10002);
        assert_eq!(Category::Disconnect as u16, 10003);
    }
}

// ============================================================================
// Native-DB Feature Tests
// ============================================================================

#[cfg(feature = "native-db")]
mod native_db_tests {
    use atomic_websocket::Settings;

    #[test]
    fn test_settings_struct() {
        let settings = Settings {
            key: "test_key".to_string(),
            value: vec![1, 2, 3],
        };

        assert_eq!(settings.key, "test_key");
        assert_eq!(settings.value, vec![1, 2, 3]);
    }
}

// ============================================================================
// In-Memory Storage Tests (when native-db is disabled)
// ============================================================================

#[cfg(not(feature = "native-db"))]
mod in_memory_tests {
    use atomic_websocket::types::InMemoryStorage;

    #[test]
    fn test_in_memory_storage_crud() {
        let mut storage = InMemoryStorage::new();

        // Insert
        storage.insert("key1".to_string(), vec![1, 2, 3]);
        assert_eq!(storage.get("key1"), Some(&vec![1, 2, 3]));

        // Update
        storage.insert("key1".to_string(), vec![4, 5, 6]);
        assert_eq!(storage.get("key1"), Some(&vec![4, 5, 6]));

        // Remove
        let removed = storage.remove("key1");
        assert_eq!(removed, Some(vec![4, 5, 6]));
        assert!(storage.get("key1").is_none());
    }

    #[test]
    fn test_in_memory_storage_multiple_keys() {
        let mut storage = InMemoryStorage::new();

        storage.insert("a".to_string(), vec![1]);
        storage.insert("b".to_string(), vec![2]);
        storage.insert("c".to_string(), vec![3]);

        assert_eq!(storage.get("a"), Some(&vec![1]));
        assert_eq!(storage.get("b"), Some(&vec![2]));
        assert_eq!(storage.get("c"), Some(&vec![3]));
    }
}

// ============================================================================
// Rustls Feature Tests
// ============================================================================

#[cfg(feature = "rustls")]
mod rustls_tests {
    use atomic_websocket::server_sender::ClientOptions;
    use atomic_websocket::AtomicWebsocketType;

    #[test]
    fn test_client_options_tls_default() {
        let options = ClientOptions::default();
        assert!(options.use_tls);
    }

    #[test]
    fn test_client_options_tls_disabled() {
        let options = ClientOptions {
            use_ping: true,
            url: "example.com".to_string(),
            retry_seconds: 30,
            use_keep_ip: false,
            connect_timeout_seconds: 3,
            atomic_websocket_type: AtomicWebsocketType::External,
            use_tls: false,
        };

        assert!(!options.use_tls);
    }
}
