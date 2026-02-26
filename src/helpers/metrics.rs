//! Metrics and observability for atomic_websocket.
//!
//! Provides lightweight counters for monitoring connection health,
//! message throughput, and error rates using lock-free atomics.

use std::sync::atomic::{AtomicU64, Ordering};

/// Lock-free metrics counters for monitoring WebSocket activity.
///
/// All counters use relaxed ordering for maximum performance.
/// Use `snapshot()` to get a consistent point-in-time view.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Total messages sent
    pub messages_sent: AtomicU64,
    /// Total messages received
    pub messages_received: AtomicU64,
    /// Messages dropped (channel full or closed)
    pub messages_dropped: AtomicU64,
    /// Total connections established (cumulative)
    pub connections_total: AtomicU64,
    /// Currently active connections
    pub connections_active: AtomicU64,
    /// Total reconnection attempts
    pub reconnections: AtomicU64,
    /// Total send errors encountered
    pub send_errors: AtomicU64,
}

impl Metrics {
    /// Creates a new zeroed Metrics instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a point-in-time snapshot of all counters.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            messages_sent: self.messages_sent.load(Ordering::Relaxed),
            messages_received: self.messages_received.load(Ordering::Relaxed),
            messages_dropped: self.messages_dropped.load(Ordering::Relaxed),
            connections_total: self.connections_total.load(Ordering::Relaxed),
            connections_active: self.connections_active.load(Ordering::Relaxed),
            reconnections: self.reconnections.load(Ordering::Relaxed),
            send_errors: self.send_errors.load(Ordering::Relaxed),
        }
    }

    #[inline]
    pub fn inc_messages_sent(&self) {
        self.messages_sent.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_messages_received(&self) {
        self.messages_received.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_messages_dropped(&self) {
        self.messages_dropped.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_connections_total(&self) {
        self.connections_total.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_connections_active(&self) {
        self.connections_active.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn dec_connections_active(&self) {
        let _ = self
            .connections_active
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                if v > 0 {
                    Some(v - 1)
                } else {
                    None
                }
            });
    }

    #[inline]
    pub fn inc_reconnections(&self) {
        self.reconnections.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_send_errors(&self) {
        self.send_errors.fetch_add(1, Ordering::Relaxed);
    }
}

/// A plain-data snapshot of metrics for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub messages_sent: u64,
    pub messages_received: u64,
    pub messages_dropped: u64,
    pub connections_total: u64,
    pub connections_active: u64,
    pub reconnections: u64,
    pub send_errors: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_default_zero() {
        let m = Metrics::new();
        let s = m.snapshot();
        assert_eq!(s.messages_sent, 0);
        assert_eq!(s.messages_received, 0);
        assert_eq!(s.messages_dropped, 0);
        assert_eq!(s.connections_total, 0);
        assert_eq!(s.connections_active, 0);
        assert_eq!(s.reconnections, 0);
        assert_eq!(s.send_errors, 0);
    }

    #[test]
    fn test_metrics_increment() {
        let m = Metrics::new();
        m.inc_messages_sent();
        m.inc_messages_sent();
        m.inc_messages_received();
        m.inc_connections_total();
        m.inc_connections_active();
        m.inc_send_errors();
        m.inc_reconnections();
        m.inc_messages_dropped();

        let s = m.snapshot();
        assert_eq!(s.messages_sent, 2);
        assert_eq!(s.messages_received, 1);
        assert_eq!(s.messages_dropped, 1);
        assert_eq!(s.connections_total, 1);
        assert_eq!(s.connections_active, 1);
        assert_eq!(s.reconnections, 1);
        assert_eq!(s.send_errors, 1);
    }

    #[test]
    fn test_metrics_dec_connections_active() {
        let m = Metrics::new();
        m.inc_connections_active();
        m.inc_connections_active();
        m.dec_connections_active();

        assert_eq!(m.snapshot().connections_active, 1);
    }

    #[test]
    fn test_snapshot_clone() {
        let m = Metrics::new();
        m.inc_messages_sent();
        let s1 = m.snapshot();
        let s2 = s1.clone();
        assert_eq!(s1, s2);
    }
}
