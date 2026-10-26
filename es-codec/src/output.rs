//! Write-side counterpart of [`crate::input::DataInput`]: an in-memory Lucene `DataOutput`.

use crate::codec_util::{CODEC_MAGIC, FOOTER_MAGIC, ID_LEN, crc32};
use std::collections::BTreeMap;

#[derive(Default)]
pub struct Output(pub Vec<u8>);

impl Output {
    pub fn byte(&mut self, v: u8) {
        self.0.push(v);
    }

    pub fn bytes(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }

    pub fn long(&mut self, v: i64) {
        self.bytes(&v.to_le_bytes());
    }

    pub fn int(&mut self, v: i32) {
        self.bytes(&v.to_le_bytes());
    }

    /// Codec header without the segment id/suffix: magic, codec name, version.
    pub fn header(&mut self, codec: &str, version: i32) {
        self.be_int(CODEC_MAGIC);
        self.string(codec);
        self.be_int(version);
    }

    pub fn be_int(&mut self, v: i32) {
        self.bytes(&v.to_be_bytes());
    }

    pub fn be_long(&mut self, v: i64) {
        self.bytes(&v.to_be_bytes());
    }

    pub fn vlong(&mut self, v: i64) {
        let mut v = v as u64;
        while v >= 0x80 {
            self.byte(v as u8 | 0x80);
            v >>= 7;
        }
        self.byte(v as u8);
    }

    /// Negative values take 5 bytes, as in Java.
    pub fn vint(&mut self, v: i32) {
        self.vlong(v as u32 as i64);
    }

    pub fn string(&mut self, s: &str) {
        self.vint(s.len() as i32);
        self.bytes(s.as_bytes());
    }

    /// Elasticsearch's `StreamOutput.writeString`; see [`crate::input::DataInput::read_es_string`].
    pub fn es_string(&mut self, s: &str) {
        self.vint(s.encode_utf16().count() as i32);
        for c in s.encode_utf16() {
            match c {
                0..=0x7f => self.byte(c as u8),
                0x80..=0x7ff => self.bytes(&[0xc0 | (c >> 6) as u8, 0x80 | (c & 0x3f) as u8]),
                _ => self.bytes(&[
                    0xe0 | (c >> 12) as u8,
                    0x80 | (c >> 6 & 0x3f) as u8,
                    0x80 | (c & 0x3f) as u8,
                ]),
            }
        }
    }

    pub fn map_of_strings(&mut self, m: &BTreeMap<String, String>) {
        self.vint(m.len() as i32);
        for (k, v) in m {
            self.string(k);
            self.string(v);
        }
    }

    pub fn set_of_strings(&mut self, set: &std::collections::BTreeSet<String>) {
        self.vint(set.len() as i32);
        for v in set {
            self.string(v);
        }
    }

    pub fn index_header(&mut self, codec: &str, version: i32, id: &[u8; ID_LEN], suffix: &str) {
        self.header(codec, version);
        self.bytes(id);
        self.byte(suffix.len() as u8);
        self.bytes(suffix.as_bytes());
    }

    /// Appends the checksummed footer and returns the file bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.be_int(FOOTER_MAGIC);
        self.be_int(0);
        let crc = crc32(&self.0);
        self.be_long(crc as i64);
        self.0
    }
}
