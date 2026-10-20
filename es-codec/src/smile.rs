//! SMILE (binary JSON) decoder, for Elasticsearch's cluster state documents.
//!
//! Supports the subset ES writes: shared property names, raw binary, no shared string values.

use crate::{Result, err};

/// A decoded SMILE value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(o) => o.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    names: Vec<String>,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8> {
        let b = self.data.get(self.pos).copied();
        self.pos += 1;
        b.map_or_else(|| err("truncated smile"), Ok)
    }

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let s = self.data.get(self.pos..self.pos + n);
        self.pos += n;
        s.map_or_else(|| err("truncated smile"), Ok)
    }

    fn text(&mut self, n: usize) -> Result<String> {
        String::from_utf8(self.take(n)?.to_vec()).or_else(|_| err("invalid utf-8 in smile"))
    }

    /// Zigzag-free VInt: 7 bits per byte, the last byte flagged by 0x80 and holding 6 bits.
    fn vint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        loop {
            let b = self.byte()?;
            if b & 0x80 != 0 {
                return Ok((v << 6) | u64::from(b & 0x3F));
            }
            v = (v << 7) | u64::from(b);
        }
    }

    fn zigzag(v: u64) -> i64 {
        (v >> 1) as i64 ^ -((v & 1) as i64)
    }

    /// `n` bytes of 7 bits each, big-endian.
    fn bits7(&mut self, n: usize) -> Result<u128> {
        Ok(self
            .take(n)?
            .iter()
            .fold(0, |v, &b| (v << 7) | u128::from(b)))
    }

    /// A string terminated by 0xFC.
    fn long_text(&mut self) -> Result<String> {
        let start = self.pos;
        while self.byte()? != 0xFC {}
        String::from_utf8(self.data[start..self.pos - 1].to_vec())
            .or_else(|_| err("invalid utf-8 in smile"))
    }

    fn key(&mut self) -> Result<String> {
        let b = self.byte()?;
        let name = match b {
            0x20 => String::new(),
            0x30..=0x33 => {
                let i = (usize::from(b & 3) << 8) | usize::from(self.byte()?);
                return self.shared_name(i);
            }
            0x34 => self.long_text()?,
            0x40..=0x7F => return self.shared_name(usize::from(b & 0x3F)),
            0x80..=0xBF => self.text(usize::from(b & 0x3F) + 1)?,
            0xC0..=0xF7 => self.text(usize::from(b - 0xC0) + 2)?,
            _ => return err(format!("bad smile key token {b:#x}")),
        };
        if name.len() <= 64 {
            if self.names.len() == 1024 {
                self.names.clear();
            }
            self.names.push(name.clone());
        }
        Ok(name)
    }

    fn shared_name(&self, i: usize) -> Result<String> {
        self.names
            .get(i)
            .cloned()
            .map_or_else(|| err("bad smile shared name reference"), Ok)
    }

    fn value(&mut self) -> Result<Value> {
        let b = self.byte()?;
        Ok(match b {
            0x20 => Value::String(String::new()),
            0x21 => Value::Null,
            0x22 => Value::Bool(false),
            0x23 => Value::Bool(true),
            0x24 | 0x25 => Value::Int(Self::zigzag(self.vint()?)),
            0x28 => Value::Float(f64::from(f32::from_bits(self.bits7(5)? as u32))),
            0x29 => Value::Float(f64::from_bits(self.bits7(10)? as u64)),
            0x40..=0x5F => Value::String(self.text(usize::from(b & 0x1F) + 1)?),
            0x60..=0x7F => Value::String(self.text(usize::from(b & 0x1F) + 33)?),
            0x80..=0x9F => Value::String(self.text(usize::from(b & 0x1F) + 2)?),
            0xA0..=0xBF => Value::String(self.text(usize::from(b & 0x1F) + 34)?),
            0xC0..=0xDF => Value::Int(Self::zigzag(u64::from(b & 0x1F))),
            0xE0 | 0xE4 => Value::String(self.long_text()?),
            0xFD => {
                let n = self.vint()? as usize;
                Value::Bytes(self.take(n)?.to_vec())
            }
            0xF8 => {
                let mut items = Vec::new();
                while self.data.get(self.pos) != Some(&0xF9) {
                    items.push(self.value()?);
                }
                self.pos += 1;
                Value::Array(items)
            }
            0xFA => {
                let mut fields = Vec::new();
                while self.data.get(self.pos) != Some(&0xFB) {
                    let k = self.key()?;
                    fields.push((k, self.value()?));
                }
                self.pos += 1;
                Value::Object(fields)
            }
            _ => return err(format!("unsupported smile value token {b:#x}")),
        })
    }
}

/// Decodes one SMILE document.
pub fn decode(data: &[u8]) -> Result<Value> {
    if data.get(..3) != Some(b":)\n") || data.get(3).is_none_or(|f| f & 0x02 != 0) {
        return err("not a smile document, or uses shared string values");
    }
    Reader {
        data,
        pos: 4,
        names: Vec::new(),
    }
    .value()
}
