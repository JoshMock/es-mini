//! Base64 encoding over the standard and URL-safe alphabets.

/// The URL-safe alphabet (`-_`).
pub const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
/// The standard alphabet (`+/`).
pub const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `b` with `alphabet`, appending `=` padding when `pad` is set.
pub fn encode(b: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    b.chunks(3)
        .flat_map(|c| {
            let n = c.iter().fold(0u32, |n, b| n << 8 | u32::from(*b)) << (8 * (3 - c.len()));
            (0..4).filter_map(move |i| {
                if i <= c.len() {
                    Some(char::from(alphabet[(n >> (18 - 6 * i) & 63) as usize]))
                } else {
                    pad.then_some('=')
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_both_alphabets_with_and_without_padding() {
        assert_eq!(encode(b"ab", STANDARD, true), "YWI=");
        assert_eq!(encode(b"ab", STANDARD, false), "YWI");
        assert_eq!(encode(&[0xfb, 0xff], STANDARD, true), "+/8=");
        assert_eq!(encode(&[0xfb, 0xff], URL, false), "-_8");
    }
}
