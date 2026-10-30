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

/// Decodes `src` over `alphabet` (padding optional) as Java's `Base64.Decoder` does, failing
/// with its exception message.
pub fn decode(src: &[u8], alphabet: &[u8; 64]) -> Result<Vec<u8>, String> {
    if src.len() == 1 {
        return Err("Input byte[] should at least have 2 bytes for base64 bytes".into());
    }
    let (mut out, mut bits, mut shift, mut at) = (vec![], 0u32, 18, 0);
    while at < src.len() {
        let b = src[at];
        at += 1;
        if b == b'=' {
            if shift == 18 || shift == 6 && src.get(at).is_none_or(|b| *b != b'=') {
                return Err("Input byte array has wrong 4-byte ending unit".into());
            }
            at += usize::from(shift == 6);
            break;
        }
        let Some(v) = alphabet.iter().position(|a| *a == b) else {
            // Java's Integer.toString(byte, 16): signed
            let (sign, b) = (
                if b < 0x80 { "" } else { "-" },
                i32::from(b as i8).unsigned_abs(),
            );
            return Err(format!("Illegal base64 character {sign}{b:x}"));
        };
        bits |= (v as u32) << shift;
        if shift == 0 {
            out.extend(&bits.to_be_bytes()[1..]);
            (bits, shift) = (0, 18);
        } else {
            shift -= 6;
        }
    }
    match shift {
        6 => out.push((bits >> 16) as u8),
        0 => out.extend(&bits.to_be_bytes()[1..3]),
        12 => return Err("Last unit does not have enough valid bits".into()),
        _ => {}
    }
    if at < src.len() {
        return Err(format!(
            "Input byte array has incorrect ending byte at {at}"
        ));
    }
    Ok(out)
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

    #[test]
    fn decodes_with_java_errors() {
        assert_eq!(decode(b"YWI=", STANDARD).unwrap(), b"ab");
        assert_eq!(decode(b"YWI", STANDARD).unwrap(), b"ab");
        assert_eq!(decode(b"-_8", URL).unwrap(), [0xfb, 0xff]);
        assert_eq!(decode(b"", STANDARD).unwrap(), b"");
        let err = |s: &[u8]| decode(s, STANDARD).unwrap_err();
        assert_eq!(
            err(b"Y"),
            "Input byte[] should at least have 2 bytes for base64 bytes"
        );
        assert_eq!(err(b"!!!"), "Illegal base64 character 21");
        assert_eq!(err(b"YW\xe9"), "Illegal base64 character -17");
        assert_eq!(err(b"-_8"), "Illegal base64 character 2d");
        assert_eq!(
            err(b"YW=a"),
            "Input byte array has wrong 4-byte ending unit"
        );
        assert_eq!(err(b"YWJjZ"), "Last unit does not have enough valid bits");
        assert_eq!(
            err(b"YWI=YWI="),
            "Input byte array has incorrect ending byte at 4"
        );
    }
}
