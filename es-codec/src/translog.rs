//! Writer for an empty Elasticsearch translog generation (`translog-N.tlog` + `translog.ckp`).

use crate::codec_util::crc32;
use crate::output::Output;

/// Fields of `translog.ckp` that vary for an empty translog.
pub struct Checkpoint {
    pub generation: i64,
    pub global_checkpoint: i64,
    pub primary_term: i64,
}

/// Returns `(translog-N.tlog, translog.ckp)` bytes for a translog holding no operations.
pub fn write_empty(uuid: &str, cp: &Checkpoint) -> (Vec<u8>, Vec<u8>) {
    let mut t = Output::default();
    t.header("translog", 3);
    t.be_int(uuid.len() as i32);
    t.bytes(uuid.as_bytes());
    t.be_long(cp.primary_term);
    let crc = crc32(&t.0);
    t.be_int(crc as i32);
    let header_len = t.0.len() as i64;

    let mut c = Output::default();
    c.header("ckp", 4);
    c.long(header_len);
    c.int(0); // numOps
    c.long(cp.generation);
    c.long(-1); // minSeqNo
    c.long(-1); // maxSeqNo
    c.long(cp.global_checkpoint);
    c.long(cp.generation); // minTranslogGeneration
    c.long(-2); // trimmedAboveSeqNo (unset)
    (t.0, c.finish())
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
