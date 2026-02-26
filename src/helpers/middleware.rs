//! Middleware/interceptor pattern for WebSocket message handling.
//!
//! Middlewares are called in the order they are added to [`ServerOptions`].
//! They can intercept connections, messages, and disconnections on the server side.

use async_trait::async_trait;

/// Result of middleware processing a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiddlewareResult {
    /// Continue processing — forward the message to the application handler.
    Continue,
    /// Stop processing — do not forward the message to the application handler.
    Stop,
}

/// Trait for intercepting WebSocket events on the server side.
///
/// Implement this trait to add cross-cutting concerns such as rate limiting,
/// authentication, message validation, or logging.
///
/// Middlewares are called in order. For `on_message`, if any middleware
/// returns [`MiddlewareResult::Stop`], the message is not forwarded to the
/// application handler and no further middlewares are called.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_websocket::middleware::{MessageMiddleware, MiddlewareResult};
/// use async_trait::async_trait;
///
/// struct LoggingMiddleware;
///
/// #[async_trait]
/// impl MessageMiddleware for LoggingMiddleware {
///     async fn on_connect(&self, peer: &str) -> bool {
///         println!("Client connected: {}", peer);
///         true
///     }
///
///     async fn on_message(&self, peer: &str, data: &[u8]) -> MiddlewareResult {
///         println!("Message from {}: {} bytes", peer, data.len());
///         MiddlewareResult::Continue
///     }
///
///     async fn on_disconnect(&self, peer: &str) {
///         println!("Client disconnected: {}", peer);
///     }
/// }
/// ```
#[async_trait]
pub trait MessageMiddleware: Send + Sync {
    /// Called when a new client connects and has been registered.
    ///
    /// Return `true` to allow the connection, or `false` to reject it.
    /// If rejected, the client will be disconnected immediately.
    ///
    /// Default implementation allows all connections.
    async fn on_connect(&self, _peer: &str) -> bool {
        true
    }

    /// Called for each incoming message after protocol handling (ping/pong/disconnect).
    ///
    /// Return [`MiddlewareResult::Continue`] to forward the message to the
    /// application handler, or [`MiddlewareResult::Stop`] to drop it.
    async fn on_message(&self, peer: &str, data: &[u8]) -> MiddlewareResult;

    /// Called when a client disconnects (only if `on_connect` returned `true`).
    ///
    /// Default implementation does nothing.
    async fn on_disconnect(&self, _peer: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AllowAllMiddleware;

    #[async_trait]
    impl MessageMiddleware for AllowAllMiddleware {
        async fn on_message(&self, _peer: &str, _data: &[u8]) -> MiddlewareResult {
            MiddlewareResult::Continue
        }
    }

    struct RejectAllMiddleware;

    #[async_trait]
    impl MessageMiddleware for RejectAllMiddleware {
        async fn on_connect(&self, _peer: &str) -> bool {
            false
        }

        async fn on_message(&self, _peer: &str, _data: &[u8]) -> MiddlewareResult {
            MiddlewareResult::Stop
        }
    }

    #[tokio::test]
    async fn test_default_on_connect_allows() {
        let mw = AllowAllMiddleware;
        assert!(mw.on_connect("peer1").await);
    }

    #[tokio::test]
    async fn test_default_on_disconnect_is_noop() {
        let mw = AllowAllMiddleware;
        mw.on_disconnect("peer1").await;
    }

    #[tokio::test]
    async fn test_allow_all_middleware() {
        let mw = AllowAllMiddleware;
        assert!(mw.on_connect("peer1").await);
        assert_eq!(
            mw.on_message("peer1", b"hello").await,
            MiddlewareResult::Continue
        );
    }

    #[tokio::test]
    async fn test_reject_all_middleware() {
        let mw = RejectAllMiddleware;
        assert!(!mw.on_connect("peer1").await);
        assert_eq!(
            mw.on_message("peer1", b"hello").await,
            MiddlewareResult::Stop
        );
    }

    #[test]
    fn test_middleware_result_equality() {
        assert_eq!(MiddlewareResult::Continue, MiddlewareResult::Continue);
        assert_eq!(MiddlewareResult::Stop, MiddlewareResult::Stop);
        assert_ne!(MiddlewareResult::Continue, MiddlewareResult::Stop);
    }

    #[test]
    fn test_middleware_result_copy() {
        let r = MiddlewareResult::Continue;
        let r2 = r;
        assert_eq!(r, r2);
    }
}
