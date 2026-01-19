use std::error::Error;

#[cfg(feature = "bebop")]
use bebop::Record;
use tokio_tungstenite::tungstenite::{Bytes, Message};

#[cfg(feature = "bebop")]
use crate::schema::{Category, Data, Disconnect, Expired, Ping};
use crate::Settings;

use super::types::DB;

/// Log debug macro: prioritizes rinf > debug > default
#[cfg(feature = "rinf")]
#[macro_export]
macro_rules! log_debug {
    ($($rest:tt)*) => {
        rinf::debug_print!($($rest)*)
    };
}

#[cfg(all(not(feature = "rinf"), feature = "debug"))]
#[macro_export]
macro_rules! log_debug {
    ($($rest:tt)*) => {
        log::debug!($($rest)*)
    };
}

#[cfg(all(not(feature = "rinf"), not(feature = "debug")))]
#[macro_export]
macro_rules! log_debug {
    ($($rest:tt)*) => {
        if cfg!(debug_assertions) {
            println!($($rest)*)
        }
    };
}

/// Log error macro: prioritizes rinf > debug > default
#[cfg(feature = "rinf")]
#[macro_export]
macro_rules! log_error {
    ($($rest:tt)*) => {
        rinf::debug_print!($($rest)*)
    };
}

#[cfg(all(not(feature = "rinf"), feature = "debug"))]
#[macro_export]
macro_rules! log_error {
    ($($rest:tt)*) => {
        log::error!($($rest)*)
    };
}

#[cfg(all(not(feature = "rinf"), not(feature = "debug")))]
#[macro_export]
macro_rules! log_error {
    ($($rest:tt)*) => {
        if cfg!(debug_assertions) {
            println!($($rest)*)
        }
    };
}

#[cfg(feature = "native-db")]
pub async fn get_setting_by_key(db: DB, key: String) -> Result<Option<Settings>, Box<dyn Error>> {
    let db = db.lock().await;
    let reader = db.r_transaction()?;

    Ok(reader.get().primary::<Settings>(key)?)
}

#[cfg(not(feature = "native-db"))]
pub async fn get_setting_by_key(db: DB, key: String) -> Result<Option<Settings>, Box<dyn Error>> {
    let db = db.lock().await;
    Ok(db.get(&key).map(|v| Settings {
        key,
        value: v.clone(),
    }))
}

#[cfg(feature = "native-db")]
pub async fn set_setting(db: DB, settings: Settings) -> Result<bool, Box<dyn Error>> {
    let db = db.lock().await;
    let reader = db.r_transaction()?;
    let writer = db.rw_transaction()?;

    let setting = reader.get().primary::<Settings>(settings.key.clone())?;
    drop(reader);

    match setting {
        Some(setting) => {
            writer.update::<Settings>(setting, settings)?;
        }
        None => {
            writer.insert::<Settings>(settings)?;
        }
    }
    writer.commit()?;

    Ok(true)
}

#[cfg(not(feature = "native-db"))]
pub async fn set_setting(db: DB, settings: Settings) -> Result<bool, Box<dyn Error>> {
    let mut db = db.lock().await;
    db.insert(settings.key, settings.value);
    Ok(true)
}

#[cfg(feature = "bebop")]
pub fn make_ping_message(peer: &str) -> Message {
    let mut datas = Vec::with_capacity(64);
    Ping {
        peer,
        activations: 0,
    }
    .serialize(&mut datas)
    .expect("Ping serialization should never fail");
    make_response_message(Category::Ping, datas)
}

#[cfg(not(feature = "bebop"))]
pub fn make_ping_message(_peer: &str) -> Message {
    Message::Binary(Bytes::new())
}

#[cfg(feature = "bebop")]
pub fn get_data_schema(data: &[u8]) -> Result<Data<'_>, Box<dyn Error>> {
    if data.len() < 2 {
        return Err("Data length is too short".into());
    }
    Ok(Data {
        category: data[0] as u16 + data[1] as u16 * 256,
        datas: bebop::SliceWrapper::from_raw(&data[2..]),
    })
}

pub fn make_atomic_message(category: u16, mut datas: Vec<u8>) -> Message {
    let mut byte = {
        let quotient = category / 256;
        let remainder = category % 256;
        vec![remainder as u8, quotient as u8]
    };
    byte.append(&mut datas);
    Message::Binary(Bytes::from(byte))
}

/// Create a message from raw bytes without category prefix.
/// Use this when bebop feature is disabled.
#[allow(dead_code)]
pub fn make_raw_message(data: &[u8]) -> Message {
    Message::Binary(Bytes::copy_from_slice(data))
}

#[cfg(feature = "bebop")]
pub fn make_response_message(category: Category, datas: Vec<u8>) -> Message {
    make_atomic_message(category as u16, datas)
}

#[cfg(feature = "bebop")]
pub fn make_disconnect_message(peer: &str) -> Message {
    let mut datas = Vec::with_capacity(64);
    Disconnect { peer }
        .serialize(&mut datas)
        .expect("Disconnect serialization should never fail");
    make_response_message(Category::Disconnect, datas)
}

#[cfg(not(feature = "bebop"))]
pub fn make_disconnect_message(_peer: &str) -> Message {
    Message::Binary(Bytes::new())
}

#[cfg(feature = "bebop")]
pub fn make_pong_message() -> Message {
    make_response_message(Category::Pong, Vec::new())
}

#[cfg(not(feature = "bebop"))]
pub fn make_pong_message() -> Message {
    Message::Binary(Bytes::new())
}

#[cfg(feature = "bebop")]
pub fn make_expired_output_message() -> Message {
    let mut datas = Vec::with_capacity(16);
    Expired { is_expired: true }
        .serialize(&mut datas)
        .expect("Expired serialization should never fail");
    make_response_message(Category::Expired, datas)
}

#[cfg(not(feature = "bebop"))]
pub fn make_expired_output_message() -> Message {
    Message::Binary(Bytes::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_atomic_message_basic() {
        let msg = make_atomic_message(100, vec![1, 2, 3]);
        let data = msg.into_data();
        // category 100 = 100 % 256 = 100, 100 / 256 = 0
        assert_eq!(data[0], 100);
        assert_eq!(data[1], 0);
        assert_eq!(&data[2..], &[1, 2, 3]);
    }

    #[test]
    fn test_make_atomic_message_large_category() {
        // Category 10000 = 10000 % 256 = 16, 10000 / 256 = 39
        let msg = make_atomic_message(10000, vec![]);
        let data = msg.into_data();
        assert_eq!(data[0], 16);
        assert_eq!(data[1], 39);
        assert_eq!(data.len(), 2);
    }

    #[test]
    fn test_make_atomic_message_empty_data() {
        let msg = make_atomic_message(0, vec![]);
        let data = msg.into_data();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0], 0);
        assert_eq!(data[1], 0);
    }

    #[test]
    fn test_make_raw_message() {
        let raw_data = vec![10, 20, 30, 40];
        let msg = make_raw_message(&raw_data);
        let data = msg.into_data();
        assert_eq!(data, raw_data);
    }

    #[cfg(feature = "bebop")]
    #[test]
    fn test_get_data_schema_valid() {
        // Create a valid data with category 100
        let data = vec![100, 0, 1, 2, 3];
        let result = get_data_schema(&data);
        assert!(result.is_ok());
        let schema = result.unwrap();
        assert_eq!(schema.category, 100);
    }

    #[cfg(feature = "bebop")]
    #[test]
    fn test_get_data_schema_large_category() {
        // category 10000 = 16 + 39*256
        let data = vec![16, 39, 1, 2, 3];
        let result = get_data_schema(&data);
        assert!(result.is_ok());
        let schema = result.unwrap();
        assert_eq!(schema.category, 10000);
    }

    #[cfg(feature = "bebop")]
    #[test]
    fn test_get_data_schema_too_short() {
        let data = vec![1];
        let result = get_data_schema(&data);
        assert!(result.is_err());
    }

    #[cfg(feature = "bebop")]
    #[test]
    fn test_get_data_schema_empty() {
        let data: Vec<u8> = vec![];
        let result = get_data_schema(&data);
        assert!(result.is_err());
    }

    #[cfg(feature = "bebop")]
    #[test]
    fn test_make_ping_message() {
        let msg = make_ping_message("test-peer");
        let data = msg.into_data();
        // Should have category bytes at the start
        assert!(data.len() > 2);
        // Category::Ping = 10000 => 16, 39
        assert_eq!(data[0], 16);
        assert_eq!(data[1], 39);
    }

    #[cfg(feature = "bebop")]
    #[test]
    fn test_make_pong_message() {
        let msg = make_pong_message();
        let data = msg.into_data();
        // Category::Pong = 10001 => 17, 39
        assert_eq!(data[0], 17);
        assert_eq!(data[1], 39);
    }

    #[cfg(feature = "bebop")]
    #[test]
    fn test_make_disconnect_message() {
        let msg = make_disconnect_message("peer-123");
        let data = msg.into_data();
        // Category::Disconnect = 10003 => 19, 39
        assert_eq!(data[0], 19);
        assert_eq!(data[1], 39);
        assert!(data.len() > 2);
    }

    #[cfg(feature = "bebop")]
    #[test]
    fn test_make_expired_output_message() {
        let msg = make_expired_output_message();
        let data = msg.into_data();
        // Category::Expired = 10002 => 18, 39
        assert_eq!(data[0], 18);
        assert_eq!(data[1], 39);
    }

    #[cfg(not(feature = "bebop"))]
    #[test]
    fn test_make_ping_message_no_bebop() {
        let msg = make_ping_message("test-peer");
        let data = msg.into_data();
        assert!(data.is_empty());
    }

    #[cfg(not(feature = "bebop"))]
    #[test]
    fn test_make_pong_message_no_bebop() {
        let msg = make_pong_message();
        let data = msg.into_data();
        assert!(data.is_empty());
    }

    #[cfg(not(feature = "bebop"))]
    #[test]
    fn test_make_disconnect_message_no_bebop() {
        let msg = make_disconnect_message("peer-123");
        let data = msg.into_data();
        assert!(data.is_empty());
    }

    // ========================================================================
    // get_setting_by_key, set_setting 데이터베이스 함수 테스트
    // ========================================================================

    #[cfg(not(feature = "native-db"))]
    mod db_tests {
        use super::*;
        use crate::helpers::types::InMemoryStorage;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        fn create_test_db() -> DB {
            Arc::new(Mutex::new(InMemoryStorage::new()))
        }

        #[tokio::test]
        async fn test_get_setting_by_key_not_found() {
            let db = create_test_db();

            let result = get_setting_by_key(db, "nonexistent_key".to_string()).await;
            assert!(result.is_ok());
            assert!(result.unwrap().is_none());
        }

        #[tokio::test]
        async fn test_set_setting_insert_new() {
            let db = create_test_db();

            let settings = Settings {
                key: "test_key".to_string(),
                value: vec![1, 2, 3, 4, 5],
            };

            let result = set_setting(db.clone(), settings).await;
            assert!(result.is_ok());
            assert!(result.unwrap());

            // 저장된 값 확인
            let retrieved = get_setting_by_key(db, "test_key".to_string()).await;
            assert!(retrieved.is_ok());
            let settings = retrieved.unwrap();
            assert!(settings.is_some());
            assert_eq!(settings.unwrap().value, vec![1, 2, 3, 4, 5]);
        }

        #[tokio::test]
        async fn test_set_setting_update_existing() {
            let db = create_test_db();

            // 첫 번째 저장
            let settings1 = Settings {
                key: "update_key".to_string(),
                value: vec![1, 2, 3],
            };
            set_setting(db.clone(), settings1).await.unwrap();

            // 같은 키로 업데이트
            let settings2 = Settings {
                key: "update_key".to_string(),
                value: vec![10, 20, 30, 40],
            };
            let result = set_setting(db.clone(), settings2).await;
            assert!(result.is_ok());

            // 업데이트된 값 확인
            let retrieved = get_setting_by_key(db, "update_key".to_string()).await;
            assert!(retrieved.is_ok());
            let settings = retrieved.unwrap().unwrap();
            assert_eq!(settings.value, vec![10, 20, 30, 40]);
        }

        #[tokio::test]
        async fn test_get_setting_by_key_returns_correct_key() {
            let db = create_test_db();

            let settings = Settings {
                key: "my_key".to_string(),
                value: vec![100],
            };
            set_setting(db.clone(), settings).await.unwrap();

            let retrieved = get_setting_by_key(db, "my_key".to_string())
                .await
                .unwrap()
                .unwrap();

            assert_eq!(retrieved.key, "my_key");
            assert_eq!(retrieved.value, vec![100]);
        }

        #[tokio::test]
        async fn test_multiple_keys_independent() {
            let db = create_test_db();

            // 여러 키 저장
            set_setting(
                db.clone(),
                Settings {
                    key: "key1".to_string(),
                    value: vec![1],
                },
            )
            .await
            .unwrap();

            set_setting(
                db.clone(),
                Settings {
                    key: "key2".to_string(),
                    value: vec![2],
                },
            )
            .await
            .unwrap();

            set_setting(
                db.clone(),
                Settings {
                    key: "key3".to_string(),
                    value: vec![3],
                },
            )
            .await
            .unwrap();

            // 각 키가 독립적으로 조회됨
            let v1 = get_setting_by_key(db.clone(), "key1".to_string())
                .await
                .unwrap()
                .unwrap();
            let v2 = get_setting_by_key(db.clone(), "key2".to_string())
                .await
                .unwrap()
                .unwrap();
            let v3 = get_setting_by_key(db.clone(), "key3".to_string())
                .await
                .unwrap()
                .unwrap();

            assert_eq!(v1.value, vec![1]);
            assert_eq!(v2.value, vec![2]);
            assert_eq!(v3.value, vec![3]);
        }

        #[tokio::test]
        async fn test_set_setting_empty_value() {
            let db = create_test_db();

            let settings = Settings {
                key: "empty_key".to_string(),
                value: vec![],
            };

            let result = set_setting(db.clone(), settings).await;
            assert!(result.is_ok());

            let retrieved = get_setting_by_key(db, "empty_key".to_string())
                .await
                .unwrap()
                .unwrap();
            assert!(retrieved.value.is_empty());
        }

        #[tokio::test]
        async fn test_set_setting_large_value() {
            let db = create_test_db();

            // 큰 데이터 저장
            let large_value: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();

            let settings = Settings {
                key: "large_key".to_string(),
                value: large_value.clone(),
            };

            let result = set_setting(db.clone(), settings).await;
            assert!(result.is_ok());

            let retrieved = get_setting_by_key(db, "large_key".to_string())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(retrieved.value.len(), 10000);
            assert_eq!(retrieved.value, large_value);
        }
    }
}
