//! Error types for the atomic_websocket library.
//!
//! This module defines custom error types for better error handling
//! throughout the library.

use thiserror::Error;

/// Main error type for atomic_websocket operations.
#[allow(dead_code)]
#[derive(Error, Debug)]
pub enum AtomicWebsocketError {
    /// Database operation failed
    #[error("Database error: {0}")]
    Database(String),

    /// Serialization/deserialization failed
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// WebSocket connection error
    #[error("Connection error: {0}")]
    Connection(String),

    /// WebSocket protocol error
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    /// Channel send error
    #[error("Channel send error: {0}")]
    ChannelSend(String),

    /// Channel closed
    #[error("Channel closed")]
    ChannelClosed,

    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(String),

    /// Timeout error
    #[error("Operation timed out: {0}")]
    Timeout(String),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result type alias using AtomicWebsocketError
#[allow(dead_code)]
pub type Result<T> = std::result::Result<T, AtomicWebsocketError>;

#[cfg(feature = "bebop")]
impl From<bebop::DeserializeError> for AtomicWebsocketError {
    fn from(err: bebop::DeserializeError) -> Self {
        AtomicWebsocketError::Serialization(err.to_string())
    }
}

#[cfg(feature = "native-db")]
impl From<native_db::db_type::Error> for AtomicWebsocketError {
    fn from(err: native_db::db_type::Error) -> Self {
        AtomicWebsocketError::Database(err.to_string())
    }
}

impl<T> From<tokio::sync::mpsc::error::SendError<T>> for AtomicWebsocketError {
    fn from(err: tokio::sync::mpsc::error::SendError<T>) -> Self {
        AtomicWebsocketError::ChannelSend(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_database_display() {
        let err = AtomicWebsocketError::Database("connection failed".to_string());
        let display = format!("{}", err);
        assert!(display.contains("Database error"));
        assert!(display.contains("connection failed"));
    }

    #[test]
    fn test_error_serialization_display() {
        let err = AtomicWebsocketError::Serialization("invalid format".to_string());
        let display = format!("{}", err);
        assert!(display.contains("Serialization error"));
        assert!(display.contains("invalid format"));
    }

    #[test]
    fn test_error_connection_display() {
        let err = AtomicWebsocketError::Connection("timeout".to_string());
        let display = format!("{}", err);
        assert!(display.contains("Connection error"));
        assert!(display.contains("timeout"));
    }

    #[test]
    fn test_error_channel_send_display() {
        let err = AtomicWebsocketError::ChannelSend("channel full".to_string());
        let display = format!("{}", err);
        assert!(display.contains("Channel send error"));
        assert!(display.contains("channel full"));
    }

    #[test]
    fn test_error_channel_closed_display() {
        let err = AtomicWebsocketError::ChannelClosed;
        let display = format!("{}", err);
        assert!(display.contains("Channel closed"));
    }

    #[test]
    fn test_error_config_display() {
        let err = AtomicWebsocketError::Config("invalid option".to_string());
        let display = format!("{}", err);
        assert!(display.contains("Configuration error"));
        assert!(display.contains("invalid option"));
    }

    #[test]
    fn test_error_timeout_display() {
        let err = AtomicWebsocketError::Timeout("connection".to_string());
        let display = format!("{}", err);
        assert!(display.contains("timed out"));
        assert!(display.contains("connection"));
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: AtomicWebsocketError = io_err.into();
        assert!(matches!(err, AtomicWebsocketError::Io(_)));
        let display = format!("{}", err);
        assert!(display.contains("IO error"));
    }

    #[test]
    fn test_error_from_websocket() {
        use tokio_tungstenite::tungstenite::Error as WsError;
        let ws_err = WsError::ConnectionClosed;
        let err: AtomicWebsocketError = ws_err.into();
        assert!(matches!(err, AtomicWebsocketError::WebSocket(_)));
    }

    #[test]
    fn test_error_from_channel_send() {
        use tokio::sync::mpsc;
        // Create a SendError directly
        let send_err = mpsc::error::SendError(42i32);
        let err: AtomicWebsocketError = send_err.into();
        assert!(matches!(err, AtomicWebsocketError::ChannelSend(_)));
    }

    #[test]
    fn test_error_debug_impl() {
        let err = AtomicWebsocketError::Database("test".to_string());
        let debug = format!("{:?}", err);
        assert!(debug.contains("Database"));
    }

    #[test]
    fn test_result_type_alias() {
        fn test_fn() -> Result<i32> {
            Ok(42)
        }
        assert_eq!(test_fn().unwrap(), 42);

        fn test_err_fn() -> Result<i32> {
            Err(AtomicWebsocketError::Config("test".to_string()))
        }
        assert!(test_err_fn().is_err());
    }

    #[cfg(feature = "bebop")]
    #[test]
    fn test_error_from_bebop_deserialize() {
        let bebop_err = bebop::DeserializeError::MoreDataExpected(5);
        let err: AtomicWebsocketError = bebop_err.into();
        assert!(matches!(err, AtomicWebsocketError::Serialization(_)));
    }
}
