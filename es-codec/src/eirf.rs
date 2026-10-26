//! Elasticsearch's EIRF row batches (`org.elasticsearch.eirf` at 9.5.5), which hold the
//! documents of a translog `IndexBatch`. Read-only: a row comes back as the JSON source
//! `EirfRowToXContent` writes for it.

use crate::{Result, err};

// SourceValueType
const ABSENT: u8 = 0x00;
const NULL: u8 = 0x01;
const TRUE: u8 = 0x02;
const FALSE: u8 = 0x03;
const INT: u8 = 0x04;
const FLOAT: u8 = 0x05;
const STRING: u8 = 0x06;
const BINARY: u8 = 0x07;
const UNION_ARRAY: u8 = 0x08;
const FIXED_ARRAY: u8 = 0x09;
const KEY_VALUE: u8 = 0x0A;
const LONG: u8 = 0x0B;
const DOUBLE: u8 = 0x0C;

fn bytes(data: &[u8], at: usize, n: usize) -> Result<&[u8]> {
    match at.checked_add(n).and_then(|end| data.get(at..end)) {
        Some(b) => Ok(b),
        None => err(format!("EIRF read past the end: {n} bytes at {at}")),
    }
}

fn u16_at(data: &[u8], at: usize) -> Result<usize> {
    Ok(u16::from_le_bytes(bytes(data, at, 2)?.try_into().expect("2 bytes")).into())
}

fn i64_at(data: &[u8], at: usize) -> Result<i64> {
    Ok(i64::from_le_bytes(
        bytes(data, at, 8)?.try_into().expect("8 bytes"),
    ))
}

fn i32_at(data: &[u8], at: usize) -> Result<i32> {
    Ok(i32::from_le_bytes(
        bytes(data, at, 4)?.try_into().expect("4 bytes"),
    ))
}

fn len_at(data: &[u8], at: usize) -> Result<usize> {
    let n = i32_at(data, at)?;
    usize::try_from(n).or_else(|_| err(format!("negative EIRF length {n}")))
}

fn utf8(b: &[u8]) -> Result<&str> {
    std::str::from_utf8(b).or_else(|_| err("invalid UTF-8 in EIRF batch"))
}

/// A batch: its schema (object and leaf field names with their parent objects) and rows.
pub struct Batch<'a> {
    data: &'a [u8],
    docs: usize,
    doc_index: usize,
    rows: usize,
    /// `(parent, name)`; object 0 is the root.
    objects: Vec<(usize, &'a str)>,
    leaves: Vec<(usize, &'a str)>,
}

impl<'a> Batch<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.get(..4) != Some(b"eirf") {
            return err("EIRF batch without its magic");
        }
        if i32_at(data, 4)? != 1 {
            return err(format!("unsupported EIRF version {}", i32_at(data, 4)?));
        }
        let mut at = len_at(data, 16)?;
        let mut fields = || -> Result<Vec<(usize, &'a str)>> {
            let n = u16_at(data, at)?;
            at += 2;
            (0..n)
                .map(|_| {
                    let (parent, len) = (u16_at(data, at)?, u16_at(data, at + 2)?);
                    at += 4 + len;
                    Ok((parent, utf8(bytes(data, at - len, len)?)?))
                })
                .collect()
        };
        let (objects, leaves) = (fields()?, fields()?);
        // Parents precede their children, which keeps the object walk finite.
        let objects_ok = objects.iter().enumerate().skip(1).all(|(i, o)| o.0 < i);
        if objects.is_empty() || !objects_ok || leaves.iter().any(|l| l.0 >= objects.len()) {
            return err("EIRF schema with an invalid parent");
        }
        Ok(Batch {
            data,
            docs: len_at(data, 12)?,
            doc_index: len_at(data, 20)?,
            rows: len_at(data, 24)?,
            objects,
            leaves,
        })
    }

    /// Row `doc` as JSON.
    pub fn source(&self, doc: usize) -> Result<Vec<u8>> {
        if doc >= self.docs {
            return err(format!("EIRF row {doc} of {}", self.docs));
        }
        let entry = self.doc_index + doc * 8;
        let start = self.rows + len_at(self.data, entry)?;
        let row = Row::new(bytes(self.data, start, len_at(self.data, entry + 4)?)?)?;
        let mut out = vec![];
        self.object(0, &row, &mut out)?;
        Ok(out)
    }

    /// Writes object `parent`: its objects, then its leaves (`EirfRowXContentParser`'s schema
    /// tree order), leaving out absent leaves and objects with none present.
    fn object(&self, parent: usize, row: &Row, out: &mut Vec<u8>) -> Result<()> {
        out.push(b'{');
        for (i, &(p, name)) in self.objects.iter().enumerate().skip(1) {
            if p == parent {
                let mut child = vec![];
                self.object(i, row, &mut child)?;
                if child != b"{}" {
                    field(out, name);
                    out.extend(child);
                }
            }
        }
        for (i, &(p, name)) in self.leaves.iter().enumerate() {
            if p == parent && row.kind(i) != ABSENT {
                field(out, name);
                row.value(i, out)?;
            }
        }
        out.push(b'}');
        Ok(())
    }
}

/// `EirfRowReader`: a type byte per column, then fixed-size slots (values, or offset and length
/// into the variable section), sized down in small rows.
struct Row<'a> {
    data: &'a [u8],
    small: bool,
    kinds: &'a [u8],
    fixed: usize,
    var: usize,
}

impl<'a> Row<'a> {
    fn new(data: &'a [u8]) -> Result<Self> {
        let small = bytes(data, 0, 1)?[0] & 1 != 0;
        let columns = u16_at(data, 1)?;
        let (var, kinds_at) = match small {
            true => (u16_at(data, 3)?, 5),
            false => (len_at(data, 3)?, 7),
        };
        Ok(Row {
            data,
            small,
            kinds: bytes(data, kinds_at, columns)?,
            fixed: kinds_at + columns,
            var,
        })
    }

    fn kind(&self, col: usize) -> u8 {
        self.kinds.get(col).copied().unwrap_or(ABSENT)
    }

    /// `SourceValueType.fixedSize`.
    fn slot_size(&self, kind: u8) -> usize {
        let four = if self.small {
            kind < LONG
        } else {
            kind < STRING
        };
        match kind {
            ..INT => 0,
            _ if four => 4,
            _ => 8,
        }
    }

    fn value(&self, col: usize, out: &mut Vec<u8>) -> Result<()> {
        let at = self.fixed
            + self.kinds[..col]
                .iter()
                .map(|&k| self.slot_size(k))
                .sum::<usize>();
        let kind = self.kind(col);
        let data = match size(kind) {
            Some(n) => bytes(self.data, at, n)?,
            None => {
                let (offset, len) = match self.small {
                    true => (u16_at(self.data, at)?, u16_at(self.data, at + 2)?),
                    false => (len_at(self.data, at)?, len_at(self.data, at + 4)?),
                };
                bytes(self.data, self.var + offset, len)?
            }
        };
        match kind {
            BINARY => json_string(
                out,
                &crate::base64::encode(data, crate::base64::STANDARD, true),
            ),
            _ => write(kind, data, out)?,
        }
        Ok(())
    }
}

/// `SourceValueType.elemDataSize`: the size of a fixed-size value, `None` for variable ones.
fn size(kind: u8) -> Option<usize> {
    match kind {
        ABSENT | NULL | TRUE | FALSE => Some(0),
        INT | FLOAT => Some(4),
        LONG | DOUBLE => Some(8),
        _ => None,
    }
}

/// Writes the value `data` holds (a variable-size value's bytes without their length).
fn write(kind: u8, data: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let float = |out: &mut Vec<u8>, v: f64| {
        if !v.is_finite() {
            return err("non-finite EIRF number");
        }
        // Debug keeps a fraction or exponent, so the value stays a JSON float.
        out.extend(format!("{v:?}").as_bytes());
        Ok(())
    };
    match kind {
        NULL => out.extend(b"null"),
        TRUE => out.extend(b"true"),
        FALSE => out.extend(b"false"),
        INT => out.extend(i32_at(data, 0)?.to_string().as_bytes()),
        LONG => out.extend(i64_at(data, 0)?.to_string().as_bytes()),
        FLOAT => float(out, f32::from_bits(i32_at(data, 0)? as u32).into())?,
        DOUBLE => float(out, f64::from_bits(i64_at(data, 0)? as u64))?,
        STRING => json_string(out, utf8(data)?),
        UNION_ARRAY | FIXED_ARRAY => {
            // `InlineArrayReader`: a fixed array shares one leading type byte.
            let (mut at, shared) = match (kind, data.first()) {
                (FIXED_ARRAY, Some(&k)) => (1, Some(k)),
                _ => (0, None),
            };
            if shared.and_then(size) == Some(0) {
                return err("EIRF fixed array of zero-size values");
            }
            out.push(b'[');
            while at < data.len() {
                if out.last() != Some(&b'[') {
                    out.push(b',');
                }
                let kind = match shared {
                    Some(k) => k,
                    None => {
                        at += 1;
                        data[at - 1]
                    }
                };
                at = element(kind, data, at, out)?;
            }
            out.push(b']');
        }
        KEY_VALUE => {
            // `KeyValueReader`: length-prefixed key, type byte, value.
            out.push(b'{');
            let mut at = 0;
            while at < data.len() {
                let len = len_at(data, at)?;
                field(out, utf8(bytes(data, at + 4, len)?)?);
                at += 4 + len;
                at = element(bytes(data, at, 1)?[0], data, at + 1, out)?;
            }
            out.push(b'}');
        }
        kind => return err(format!("unsupported EIRF value type {kind}")),
    }
    Ok(())
}

/// Writes an array element or object value of `kind` at `at` (a variable-size one behind its
/// i32 length); returns where it ends.
fn element(kind: u8, data: &[u8], at: usize, out: &mut Vec<u8>) -> Result<usize> {
    let (start, len) = match size(kind) {
        Some(n) => (at, n),
        None => (at + 4, len_at(data, at)?),
    };
    write(kind, bytes(data, start, len)?, out)?;
    Ok(start + len)
}

/// Starts an object field: a comma unless it is the first, then the name.
fn field(out: &mut Vec<u8>, name: &str) {
    if out.last() != Some(&b'{') {
        out.push(b',');
    }
    json_string(out, name);
    out.push(b':');
}

/// A JSON string as Jackson escapes it.
fn json_string(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend(b"\\\""),
            '\\' => out.extend(b"\\\\"),
            '\n' => out.extend(b"\\n"),
            '\r' => out.extend(b"\\r"),
            '\t' => out.extend(b"\\t"),
            '\u{8}' => out.extend(b"\\b"),
            '\u{c}' => out.extend(b"\\f"),
            ..' ' => out.extend(format!("\\u{:04X}", c as u32).as_bytes()),
            _ => out.extend(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    out.push(b'"');
}
