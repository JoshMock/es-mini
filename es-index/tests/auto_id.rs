//! Its own test binary: auto_id's sequence counter is process-global, so other tests running in
//! parallel would bump it between the two calls.

/// Byte 0 of an auto id is the low byte of its sequence number (url-safe base64, no padding).
fn low_seq_byte(id: &str) -> u8 {
    let sextet = |c: u8| {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
            .iter()
            .position(|a| *a == c)
            .expect("url-safe base64") as u8
    };
    let b = id.as_bytes();
    (sextet(b[0]) << 2) | (sextet(b[1]) >> 4)
}

#[test]
fn consecutive_auto_ids_count_up_by_one() {
    let (a, _) = es_index::auto_id();
    let (b, _) = es_index::auto_id();
    assert_eq!(low_seq_byte(&b), low_seq_byte(&a).wrapping_add(1));
}
