//! Translog operations against a translog Elasticsearch 9.5.5 wrote (tests/data/es-translog:
//! index 1, abc, bulk 2 and 3, reindex 1, delete abc, then killed before any flush).

use es_codec::translog::{self, Op, TranslogCheckpoint};

fn data(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/data/es-translog/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn index(id: &str, source: &str, seq_no: i64, version: i64) -> Op {
    Op::Index {
        id: id.into(),
        source: source.as_bytes().to_vec(),
        seq_no,
        version,
        primary_term: 1,
    }
}

fn expected() -> Vec<Op> {
    vec![
        index("1", r#"{"a":1}"#, 0, 1),
        index("abc", r#"{"b":"x"}"#, 1, 1),
        index("2", r#"{"c":2}"#, 2, 1),
        index("3", r#"{"c":3}"#, 3, 1),
        index("1", r#"{"a":2}"#, 4, 2),
        Op::Delete {
            id: "abc".into(),
            seq_no: 5,
            version: 2,
            primary_term: 1,
        },
    ]
}

#[test]
fn reads_elasticsearch_operations() {
    let (header, ops) = translog::read(&data("translog-2.tlog")).unwrap();
    assert_eq!(header.primary_term, 1);
    assert_eq!(header.uuid.len(), 22);
    assert_eq!(ops, expected());
}

#[test]
fn writes_byte_identical_operations_and_checkpoint() {
    let file = data("translog-2.tlog");
    let (header, _) = translog::read(&file).unwrap();
    let mut out = translog::header(&header.uuid, 1);
    for op in expected() {
        out.extend(translog::encode(&op));
    }
    assert_eq!(out, file);
    let ckp = TranslogCheckpoint {
        offset: file.len() as i64,
        num_ops: 6,
        generation: 2,
        min_seq_no: 0,
        max_seq_no: 5,
        global_checkpoint: 5,
        min_generation: 2,
    };
    assert_eq!(translog::checkpoint(&ckp), data("translog.ckp"));
    assert_eq!(
        translog::read_checkpoint(&data("translog.ckp")).unwrap(),
        ckp
    );
}

#[test]
fn reading_stops_at_a_torn_last_record() {
    let file = data("translog-2.tlog");
    let (_, ops) = translog::read(&file[..file.len() - 3]).unwrap();
    assert_eq!(ops, expected()[..5]);
}
