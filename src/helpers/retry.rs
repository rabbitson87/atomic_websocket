//! Retry utilities with exponential backoff for atomic_websocket.
//!
//! This module provides reusable retry logic with configurable exponential backoff,
//! reducing code duplication across the library.

use std::time::Duration;
use tokio::time::sleep;

/// Configuration for exponential backoff retry behavior.
#[derive(Clone, Debug)]
pub struct ExponentialBackoff {
    /// Current backoff duration
    current: Duration,
    /// Initial backoff duration (kept for reset functionality)
    #[allow(dead_code)]
    initial: Duration,
    /// Maximum backoff duration
    max: Duration,
    /// Current retry count
    count: u32,
    /// Maximum number of retries
    max_count: u32,
}

impl ExponentialBackoff {
    /// Creates a new ExponentialBackoff configuration.
    ///
    /// # Arguments
    ///
    /// * `initial_ms` - Initial backoff in milliseconds
    /// * `max_secs` - Maximum backoff in seconds
    /// * `max_count` - Maximum number of retry attempts
    ///
    /// # Returns
    ///
    /// A new ExponentialBackoff instance
    pub fn new(initial_ms: u64, max_secs: u64, max_count: u32) -> Self {
        let initial = Duration::from_millis(initial_ms);
        Self {
            current: initial,
            initial,
            max: Duration::from_secs(max_secs),
            count: 0,
            max_count,
        }
    }

    /// Creates a default ExponentialBackoff (50ms initial, 1s max, 5 retries).
    pub fn default_retry() -> Self {
        Self::new(50, 1, 5)
    }

    /// Waits for the current backoff duration and increments the counter.
    ///
    /// # Returns
    ///
    /// `true` if more retries are available, `false` if max retries reached
    pub async fn wait(&mut self) -> bool {
        if self.count >= self.max_count {
            return false;
        }
        sleep(self.current).await;
        self.current = std::cmp::min(self.current * 2, self.max);
        self.count += 1;
        true
    }

    /// Resets the backoff to initial state.
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.current = self.initial;
        self.count = 0;
    }

    /// Returns the current retry count.
    #[allow(dead_code)]
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Returns whether max retries have been reached.
    #[allow(dead_code)]
    pub fn is_exhausted(&self) -> bool {
        self.count >= self.max_count
    }
}

impl Default for ExponentialBackoff {
    fn default() -> Self {
        Self::default_retry()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_exponential_backoff_count() {
        let mut backoff = ExponentialBackoff::new(10, 1, 3);

        assert_eq!(backoff.count(), 0);
        assert!(!backoff.is_exhausted());

        assert!(backoff.wait().await);
        assert_eq!(backoff.count(), 1);

        assert!(backoff.wait().await);
        assert_eq!(backoff.count(), 2);

        assert!(backoff.wait().await);
        assert_eq!(backoff.count(), 3);

        assert!(!backoff.wait().await);
        assert!(backoff.is_exhausted());
    }

    #[test]
    fn test_reset() {
        let mut backoff = ExponentialBackoff::new(10, 1, 3);
        backoff.count = 2;
        backoff.current = Duration::from_secs(1);

        backoff.reset();

        assert_eq!(backoff.count(), 0);
        assert_eq!(backoff.current, Duration::from_millis(10));
    }
}
