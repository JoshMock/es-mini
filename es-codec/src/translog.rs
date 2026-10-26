//! Elasticsearch translog generations (`translog-N.tlog`) and their checkpoint
//! (`translog.ckp`), in the format Elasticsearch 9.5.5 writes: a header, then length-prefixed,
//! CRC32-checked operations (`Translog.Index`/`Delete`/`NoOp`; index and delete in serialization
//! formats 12 and 8).

use crate::{Error, Result, codec_util::crc32, err, input::DataInput, output::Output};

const INDEX: u8 = 2;
const DELETE: u8 = 3;
const NO_OP: u8 = 4;
/// `Translog.Index.FORMAT_REORDERED` and `Translog.Delete.FORMAT_REORDERED`.
const INDEX_FORMAT: i32 = 12;
const DELETE_FORMAT: i32 = 8;

/// A translog operation, as Elasticsearch replays it.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Index {
        id: String,
        source: Vec<u8>,
        seq_no: i64,
        version: i64,
        primary_term: i64,
        routing: Option<String>,
    },
    Delete {
        id: String,
        seq_no: i64,
        version: i64,
        primary_term: i64,
    },
    /// A sequence number that holds no document (e.g. one Lucene rejected after it was assigned).
    NoOp {
        seq_no: i64,
        primary_term: i64,
        reason: String,
    },
}

impl Op {
    pub fn seq_no(&self) -> i64 {
        match self {
            Op::Index { seq_no, .. } | Op::Delete { seq_no, .. } | Op::NoOp { seq_no, .. } => {
                *seq_no
            }
        }
    }
}

/// The translog file header: which translog it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub uuid: String,
    pub primary_term: i64,
}

/// `translog.ckp`: how much of the current generation is durable, and which generations to keep.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TranslogCheckpoint {
    /// Bytes of the current generation that hold complete operations.
    pub offset: i64,
    pub num_ops: i32,
    pub generation: i64,
    /// -1 for an empty generation.
    pub min_seq_no: i64,
    pub max_seq_no: i64,
    pub global_checkpoint: i64,
    /// The oldest generation still needed for recovery.
    pub min_generation: i64,
}

/// The header that starts every translog generation.
pub fn header(uuid: &str, primary_term: i64) -> Vec<u8> {
    let mut t = Output::default();
    t.header("translog", 3);
    t.be_int(uuid.len() as i32);
    t.bytes(uuid.as_bytes());
    t.be_long(primary_term);
    let crc = crc32(&t.0);
    t.be_int(crc as i32);
    t.0
}

/// One length-prefixed, checksummed operation record.
pub fn encode(op: &Op) -> Vec<u8> {
    let mut body = Output::default();
    match op {
        Op::Index {
            id,
            source,
            seq_no,
            version,
            primary_term,
            routing,
        } => {
            body.byte(INDEX);
            body.vint(INDEX_FORMAT);
            body.be_long(*version);
            body.be_long(*seq_no);
            body.be_long(*primary_term);
            body.be_long(-1); // no auto-generated id timestamp
            let uid = crate::uid::encode_id(id);
            body.vint(uid.len() as i32);
            body.bytes(&uid);
            match routing {
                Some(r) => {
                    body.byte(1);
                    body.es_string(r);
                }
                None => body.byte(0),
            }
            body.vint(source.len() as i32);
            body.bytes(source);
        }
        Op::Delete {
            id,
            seq_no,
            version,
            primary_term,
        } => {
            body.byte(DELETE);
            body.vint(DELETE_FORMAT);
            body.be_long(*version);
            body.be_long(*seq_no);
            body.be_long(*primary_term);
            let uid = crate::uid::encode_id(id);
            body.vint(uid.len() as i32);
            body.bytes(&uid);
        }
        Op::NoOp {
            seq_no,
            primary_term,
            reason,
        } => {
            body.byte(NO_OP);
            body.be_long(*seq_no);
            body.be_long(*primary_term);
            body.es_string(reason);
        }
    }
    let crc = crc32(&body.0);
    let mut out = Output::default();
    out.be_int(body.0.len() as i32 + 4);
    out.bytes(&body.0);
    out.be_int(crc as i32);
    out.0
}

/// The checkpoint file for `cp`.
pub fn checkpoint(cp: &TranslogCheckpoint) -> Vec<u8> {
    let mut c = Output::default();
    c.header("ckp", 4);
    c.long(cp.offset);
    c.int(cp.num_ops);
    c.long(cp.generation);
    c.long(cp.min_seq_no);
    c.long(cp.max_seq_no);
    c.long(cp.global_checkpoint);
    c.long(cp.min_generation);
    c.long(-2); // trimmedAboveSeqNo: unset
    c.finish()
}

pub fn read_checkpoint(file: &[u8]) -> Result<TranslogCheckpoint> {
    crate::codec_util::check_footer(file)?;
    let input = &mut DataInput::new(file);
    crate::codec_util::check_header(input, "ckp", 4, 4)?;
    Ok(TranslogCheckpoint {
        offset: input.read_long()?,
        num_ops: input.read_int()?,
        generation: input.read_long()?,
        min_seq_no: input.read_long()?,
        max_seq_no: input.read_long()?,
        global_checkpoint: input.read_long()?,
        min_generation: input.read_long()?,
    })
}

/// Reads a generation's header and its complete operations. A record cut short at the end (a
/// crash mid-append, beyond the checkpoint's offset) ends the list; a damaged complete record is
/// an error.
pub fn read(file: &[u8]) -> Result<(Header, Vec<Op>)> {
    let input = &mut DataInput::new(file);
    crate::codec_util::check_header(input, "translog", 3, 3)?;
    let len = input.read_be_int()?;
    let uuid = String::from_utf8(input.read_bytes(len.max(0) as usize)?.to_vec())
        .map_err(|e| Error(e.to_string()))?;
    let primary_term = input.read_be_long()?;
    let crc = input.read_be_int()? as u32;
    if crc != crc32(&file[..input.pos - 4]) {
        return err("translog header checksum mismatch");
    }
    let mut ops = vec![];
    while input.pos + 4 <= file.len() {
        let size = input.read_be_int()?;
        if size < 4 {
            return err(format!(
                "translog operation size {size} at {}",
                input.pos - 4
            ));
        }
        let Ok(record) = input.read_bytes(size as usize) else {
            break;
        };
        let (body, crc) = record.split_at(record.len() - 4);
        if crc32(body) != u32::from_be_bytes(crc.try_into().expect("4 bytes")) {
            return err(format!(
                "translog operation checksum mismatch at {}",
                input.pos - record.len()
            ));
        }
        ops.push(decode(body)?);
    }
    Ok((Header { uuid, primary_term }, ops))
}

fn decode(body: &[u8]) -> Result<Op> {
    let input = &mut DataInput::new(body);
    let kind = input.read_byte()?;
    if kind == NO_OP {
        return Ok(Op::NoOp {
            seq_no: input.read_be_long()?,
            primary_term: input.read_be_long()?,
            reason: input.read_es_string()?,
        });
    }
    let format = input.read_vint()?;
    let (version, seq_no, primary_term) = (
        input.read_be_long()?,
        input.read_be_long()?,
        input.read_be_long()?,
    );
    match (kind, format) {
        (INDEX, INDEX_FORMAT) => {
            input.read_be_long()?; // auto-generated id timestamp
            let n = input.read_len()?;
            let id = crate::uid::decode_id(input.read_bytes(n)?);
            let routing = match input.read_byte()? {
                0 => None,
                _ => Some(input.read_es_string()?),
            };
            let n = input.read_len()?;
            Ok(Op::Index {
                id,
                source: input.read_bytes(n)?.to_vec(),
                seq_no,
                version,
                primary_term,
                routing,
            })
        }
        (DELETE, DELETE_FORMAT) => {
            let n = input.read_len()?;
            Ok(Op::Delete {
                id: crate::uid::decode_id(input.read_bytes(n)?),
                seq_no,
                version,
                primary_term,
            })
        }
        // ponytail: older formats and batches; Elasticsearch 9.5.5 writes these two
        _ => err(format!(
            "unsupported translog operation type {kind} format {format}"
        )),
    }
}

/// Fields of `translog.ckp` that vary for an empty translog.
pub struct Checkpoint {
    pub generation: i64,
    pub global_checkpoint: i64,
    pub primary_term: i64,
}

/// Returns `(translog-N.tlog, translog.ckp)` bytes for a translog holding no operations.
pub fn write_empty(uuid: &str, cp: &Checkpoint) -> (Vec<u8>, Vec<u8>) {
    let t = header(uuid, cp.primary_term);
    let c = checkpoint(&TranslogCheckpoint {
        offset: t.len() as i64,
        num_ops: 0,
        generation: cp.generation,
        min_seq_no: -1,
        max_seq_no: -1,
        global_checkpoint: cp.global_checkpoint,
        min_generation: cp.generation,
    });
    (t, c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_es_9_5_5_fixture() {
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures");
        let uuid = std::fs::read_to_string(fixtures.join("expected/index_uuid")).unwrap();
        let dir = fixtures
            .join("es-9.5.5/data/indices")
            .join(uuid.trim())
            .join("0/translog");
        let tlog = std::fs::read(dir.join("translog-3.tlog")).unwrap();
        // header: magic(4) + string(9) + version(4) + len(4) + uuid + term(8) + crc(4)
        let tuuid = std::str::from_utf8(&tlog[21..43]).unwrap();
        let cp = Checkpoint {
            generation: 3,
            global_checkpoint: 7,
            primary_term: 1,
        };
        let (t, c) = write_empty(tuuid, &cp);
        assert_eq!(t, tlog);
        assert_eq!(c, std::fs::read(dir.join("translog.ckp")).unwrap());
    }
}
