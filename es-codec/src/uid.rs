//! ES `_id` term encoding (`org.elasticsearch.index.mapper.Uid`).

const BASE64_ESCAPE: u8 = 0xfd;
const NUMERIC: u8 = 0xfe;
const UTF8: u8 = 0xff;

/// All-digit ids pack two digits per byte (odd tail padded with nibble `f`) after a `0xfe`
/// prefix; unpadded URL-safe base64 ids (e.g. auto-generated ones) are stored decoded, with a
/// `0xfd` escape when the first byte is `0xfd` or above; anything else is `0xff` + UTF-8.
/// Elasticsearch rejects empty ids, as es-mini's callers do; `""` encodes as a bare `0xff`.
pub fn encode_id(id: &str) -> Vec<u8> {
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        let mut out = vec![NUMERIC];
        for pair in id.as_bytes().chunks(2) {
            let hi = pair[0] - b'0';
            let lo = pair.get(1).map_or(0xf, |b| b - b'0');
            out.push(hi << 4 | lo);
        }
        out
    } else if let Some(b) = decode_url_base64(id) {
        if b[0] >= BASE64_ESCAPE {
            [&[BASE64_ESCAPE], &b[..]].concat()
        } else {
            b
        }
    } else {
        [&[UTF8], id.as_bytes()].concat()
    }
}

/// Inverse of [`encode_id`].
pub fn decode_id(b: &[u8]) -> String {
    match b.first() {
        Some(&NUMERIC) => b[1..]
            .iter()
            .flat_map(|x| [x >> 4, x & 0xf])
            .take_while(|n| *n != 0xf)
            .map(|n| char::from(b'0' + n))
            .collect(),
        Some(&UTF8) => String::from_utf8_lossy(&b[1..]).into_owned(),
        Some(&BASE64_ESCAPE) => encode_url_base64(&b[1..]),
        _ => encode_url_base64(b),
    }
}

/// Decodes non-empty unpadded URL-safe base64, refusing input that would not re-encode to
/// itself (`Uid.isURLBase64WithoutPadding`): length 1 mod 4 or non-zero unused tail bits.
fn decode_url_base64(id: &str) -> Option<Vec<u8>> {
    if id.is_empty() || id.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(id.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in id.bytes() {
        acc = acc << 6 | crate::base64::URL.iter().position(|&a| a == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (acc == 0).then_some(out)
}

fn encode_url_base64(b: &[u8]) -> String {
    crate::base64::encode(b, crate::base64::URL, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SegmentInfos, SegmentReader, stored_fields::StoredValue};

    #[test]
    fn matches_ids_stored_by_es_9_5_5() {
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures");
        let uuid = std::fs::read_to_string(fixtures.join("expected/index_uuid")).unwrap();
        let dir = fixtures
            .join("es-9.5.5/data/indices")
            .join(uuid.trim())
            .join("0/index");
        let infos = SegmentInfos::read_latest(&dir).unwrap();
        let r = SegmentReader::open(&dir, infos.segments[0].clone()).unwrap();
        for (i, doc) in r.stored_fields().unwrap().iter().enumerate() {
            assert_eq!(
                doc[0],
                (0, StoredValue::Bytes(encode_id(&(i + 1).to_string())))
            );
        }
        assert_eq!(encode_id("123"), [0xfe, 0x12, 0x3f]);
    }

    #[test]
    fn url_base64_ids_are_decoded() {
        // `c` can end a 3-char id: its low 2 bits, past the 16 encoded bits, are zero.
        assert_eq!(encode_id("abc"), [0x69, 0xb7]);
        assert_eq!(encode_id("abcd"), [0x69, 0xb7, 0x1d]);
        // A first byte of 0xfd or above is escaped so it cannot clash with the prefixes.
        assert_eq!(encode_id("_w"), [0xfd, 0xff]);
        assert_eq!(encode_id("_g"), [0xfd, 0xfe]);
        assert_eq!(encode_id("_Q"), [0xfd, 0xfd]);
        assert_eq!(encode_id("_A"), [0xfc]);
        // Not base64 without padding: a stray tail bit, length 1 mod 4, or a non-URL char.
        assert_eq!(encode_id("ab"), b"\xffab");
        assert_eq!(encode_id("abcde"), b"\xffabcde");
        assert_eq!(encode_id("ab+d"), b"\xffab+d");
        assert_eq!(encode_id("abc="), b"\xffabc=");
    }

    #[test]
    fn decode_inverts_encode() {
        for id in [
            "1",
            "12",
            "007",
            "abc",
            "abcd",
            "_w",
            "_A",
            "ab",
            "a b",
            "é",
            "kqQ3e5QBpXc8pGvYvCTM",
        ] {
            assert_eq!(decode_id(&encode_id(id)), id);
        }
    }
}
