//! Builder patterns for atomic_websocket configuration.
//!
//! This module provides fluent builder APIs for constructing client and server
//! configurations, making it easier to set up connections with sensible defaults.

use crate::AtomicWebsocketType;

use super::{internal_client::ClientOptions, internal_server::ServerOptions};

/// Builder for constructing [`ClientOptions`] with a fluent API.
///
/// # Example
///
/// ```ignore
/// let options = ClientOptionsBuilder::new()
///     .url("192.168.1.100:9000")
///     .use_ping(true)
///     .retry_seconds(30)
///     .build();
/// ```
#[derive(Default)]
pub struct ClientOptionsBuilder {
    options: ClientOptions,
}

impl ClientOptionsBuilder {
    /// Creates a new builder with default options.
    pub fn new() -> Self {
        Self {
            options: ClientOptions::default(),
        }
    }

    /// Creates a builder for internal (local network) connections.
    ///
    /// This sets up the builder with appropriate defaults for discovering
    /// and connecting to servers on the local network.
    pub fn internal() -> Self {
        Self {
            options: ClientOptions {
                atomic_websocket_type: AtomicWebsocketType::Internal,
                ..Default::default()
            },
        }
    }

    /// Creates a builder for external (remote server) connections.
    ///
    /// This sets up the builder with appropriate defaults for connecting
    /// to a specific remote WebSocket server.
    pub fn external(url: &str) -> Self {
        Self {
            options: ClientOptions {
                atomic_websocket_type: AtomicWebsocketType::External,
                url: url.to_owned(),
                ..Default::default()
            },
        }
    }

    /// Sets the server URL for external connections.
    pub fn url(mut self, url: &str) -> Self {
        self.options.url = url.to_owned();
        self
    }

    /// Enables or disables automatic ping/pong for connection health monitoring.
    pub fn use_ping(mut self, use_ping: bool) -> Self {
        self.options.use_ping = use_ping;
        self
    }

    /// Sets the time in seconds between reconnection attempts.
    pub fn retry_seconds(mut self, seconds: u64) -> Self {
        self.options.retry_seconds = seconds;
        self
    }

    /// Enables or disables remembering the last working server IP.
    pub fn use_keep_ip(mut self, use_keep_ip: bool) -> Self {
        self.options.use_keep_ip = use_keep_ip;
        self
    }

    /// Sets the connection timeout in seconds.
    pub fn connect_timeout(mut self, seconds: u64) -> Self {
        self.options.connect_timeout_seconds = seconds;
        self
    }

    /// Sets the connection type (internal or external).
    pub fn connection_type(mut self, connection_type: AtomicWebsocketType) -> Self {
        self.options.atomic_websocket_type = connection_type;
        self
    }

    /// Enables or disables TLS for secure connections.
    ///
    /// Only available when the `rustls` feature is enabled.
    #[cfg(feature = "rustls")]
    pub fn use_tls(mut self, use_tls: bool) -> Self {
        self.options.use_tls = use_tls;
        self
    }

    /// Builds and returns the configured [`ClientOptions`].
    pub fn build(self) -> ClientOptions {
        self.options
    }
}

/// Builder for constructing [`ServerOptions`] with a fluent API.
///
/// # Example
///
/// ```ignore
/// let options = ServerOptionsBuilder::new()
///     .use_ping(true)
///     .proxy_ping(-1)
///     .build();
/// ```
#[derive(Default)]
pub struct ServerOptionsBuilder {
    options: ServerOptions,
}

impl ServerOptionsBuilder {
    /// Creates a new builder with default options.
    pub fn new() -> Self {
        Self {
            options: ServerOptions::default(),
        }
    }

    /// Enables or disables automatic ping/pong responses.
    pub fn use_ping(mut self, use_ping: bool) -> Self {
        self.options.use_ping = use_ping;
        self
    }

    /// Sets the category ID for proxying ping messages.
    ///
    /// Set to -1 to disable ping proxying (default).
    /// When set to a positive value, ping messages will be forwarded
    /// to the application with this category ID instead of being
    /// automatically responded to.
    pub fn proxy_ping(mut self, category: i16) -> Self {
        self.options.proxy_ping = category;
        self
    }

    /// Builds and returns the configured [`ServerOptions`].
    pub fn build(self) -> ServerOptions {
        self.options
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_options_builder_defaults() {
        let options = ClientOptionsBuilder::new().build();
        assert!(options.use_ping);
        assert_eq!(options.retry_seconds, 30);
        assert_eq!(options.connect_timeout_seconds, 3);
    }

    #[test]
    fn test_client_options_builder_internal() {
        let options = ClientOptionsBuilder::internal()
            .use_ping(false)
            .retry_seconds(60)
            .build();

        assert!(!options.use_ping);
        assert_eq!(options.retry_seconds, 60);
        assert!(matches!(
            options.atomic_websocket_type,
            AtomicWebsocketType::Internal
        ));
    }

    #[test]
    fn test_client_options_builder_external() {
        let options = ClientOptionsBuilder::external("example.com:9000")
            .connect_timeout(10)
            .build();

        assert_eq!(options.url, "example.com:9000");
        assert_eq!(options.connect_timeout_seconds, 10);
        assert!(matches!(
            options.atomic_websocket_type,
            AtomicWebsocketType::External
        ));
    }

    #[test]
    fn test_server_options_builder() {
        let options = ServerOptionsBuilder::new()
            .use_ping(false)
            .proxy_ping(100)
            .build();

        assert!(!options.use_ping);
        assert_eq!(options.proxy_ping, 100);
    }
}
