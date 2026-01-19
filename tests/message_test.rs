//! Integration tests for message handling.
//!
//! These tests verify message encoding/decoding and channel communication.

mod common;

use atomic_websocket::common::make_atomic_message;

// ============================================================================
// Message Encoding Tests
// ============================================================================

#[test]
fn test_atomic_message_structure() {
    let msg = make_atomic_message(100, vec![1, 2, 3, 4, 5]);
    let data = msg.into_data();

    // First two bytes are category (little-endian)
    assert_eq!(data[0], 100);
    assert_eq!(data[1], 0);
    // Rest is the payload
    assert_eq!(&data[2..], &[1, 2, 3, 4, 5]);
}

#[test]
fn test_atomic_message_empty_payload() {
    let msg = make_atomic_message(100, vec![]);
    let data = msg.into_data();

    assert_eq!(data.len(), 2);
}

#[test]
fn test_atomic_message_large_payload() {
    let large_payload: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
    let msg = make_atomic_message(42, large_payload.clone());
    let data = msg.into_data();

    assert_eq!(data.len(), 2 + large_payload.len());
    assert_eq!(&data[2..], &large_payload[..]);
}

#[test]
fn test_category_encoding_roundtrip() {
    let test_cases = vec![
        (0u16, 0u8, 0u8),
        (255, 255, 0),
        (256, 0, 1),
        (1000, 232, 3),
        (10000, 16, 39),
        (65535, 255, 255),
    ];

    for (category, expected_low, expected_high) in test_cases {
        let msg = make_atomic_message(category, vec![]);
        let data = msg.into_data();

        assert_eq!(data[0], expected_low, "Category {} low byte", category);
        assert_eq!(data[1], expected_high, "Category {} high byte", category);

        // Verify roundtrip
        let decoded = data[0] as u16 + (data[1] as u16) * 256;
        assert_eq!(decoded, category);
    }
}

// ============================================================================
// Bebop Message Tests (feature-gated)
// ============================================================================

#[cfg(feature = "bebop")]
mod bebop_tests {
    use atomic_websocket::common::make_response_message;
    use atomic_websocket::schema::Category;

    #[test]
    fn test_response_message_ping() {
        let msg = make_response_message(Category::Ping, vec![1, 2, 3]);
        let data = msg.into_data();

        // Category::Ping = 10000 => 16, 39 (little-endian)
        assert_eq!(data[0], 16);
        assert_eq!(data[1], 39);
        assert_eq!(&data[2..], &[1, 2, 3]);
    }

    #[test]
    fn test_response_message_pong() {
        let msg = make_response_message(Category::Pong, vec![]);
        let data = msg.into_data();

        assert_eq!(data[0], 17);
        assert_eq!(data[1], 39);
    }

    #[test]
    fn test_category_values() {
        assert_eq!(Category::Ping as u16, 10000);
        assert_eq!(Category::Pong as u16, 10001);
        assert_eq!(Category::Expired as u16, 10002);
        assert_eq!(Category::Disconnect as u16, 10003);
    }
}
