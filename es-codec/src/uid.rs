//! ES `_id` term encoding (`org.elasticsearch.index.mapper.Uid.encodeId`).
//!
//! Only the numeric and UTF-8 forms; ES's base64 form for 20-char auto-generated ids is not
//! written (unverified against 9.5.5), so such ids would not match ES lookups.

const NUMERIC: u8 = 0xfe;
const UTF8: u8 = 0xff;

/// All-digit ids pack two digits per byte (odd tail padded with nibble `f`) after a `0xfe`
/// prefix; anything else is `0xff` + UTF-8.
pub fn encode_id(id: &str) -> Vec<u8> {
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        let mut out = vec![NUMERIC];
        for pair in id.as_bytes().chunks(2) {
            let hi = pair[0] - b'0';
            let lo = pair.get(1).map_or(0xf, |b| b - b'0');
            out.push(hi << 4 | lo);
        }
        out
    } else {
        [&[UTF8], id.as_bytes()].concat()
    }
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
        assert_eq!(encode_id("abc"), b"\xffabc");
        assert_eq!(encode_id("123"), [0xfe, 0x12, 0x3f]);
    }
}
