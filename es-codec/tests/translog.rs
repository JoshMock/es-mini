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

/// tests/data/es-translog-records, written by tests/gen/gen_translog.sh.
fn records() -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/data/es-translog-records/translog-2.tlog",
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
        routing: None,
        auto_id_timestamp: -1,
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

#[test]
fn reads_and_writes_routing_and_no_ops() {
    let file = records();
    // header: magic(4) + string(9) + version(4) + len(4) + uuid(22) + term(8) + crc(4)
    let start = translog::header(std::str::from_utf8(&file[21..43]).unwrap(), 1).len();
    let reason = "java.lang.IllegalArgumentException: Document contains at least one immense term \
                  in field=\"w\"";
    let mut ops = vec![
        Op::Index {
            id: "r1".into(),
            source: br#"{"a":1}"#.to_vec(),
            seq_no: 0,
            version: 1,
            primary_term: 1,
            routing: Some("user1".into()),
            auto_id_timestamp: -1,
        },
        Op::Delete {
            id: "gone".into(),
            seq_no: 1,
            version: 1,
            primary_term: 1,
        },
    ];
    let encoded: Vec<u8> = ops.iter().flat_map(translog::encode).collect();
    let no_op = 4 + i32::from_be_bytes(file[start + encoded.len()..][..4].try_into().unwrap());
    let end = start + encoded.len() + no_op as usize;
    let (_, read) = translog::read(&file[..end]).unwrap();
    let Some(Op::NoOp {
        seq_no: 2,
        primary_term: 1,
        reason: r,
    }) = read.get(2)
    else {
        panic!("{:?}", read.get(2));
    };
    assert!(r.starts_with(reason), "{r}");
    ops.push(read[2].clone());
    assert_eq!(read, ops);
    let encoded: Vec<u8> = ops.iter().flat_map(translog::encode).collect();
    assert_eq!(encoded, file[start..end]);
}

#[test]
fn reads_index_batches_as_their_operations() {
    let (_, ops) = translog::read(&records()).unwrap();
    assert_eq!(ops.len(), 49);
    assert_eq!(
        ops.iter().map(Op::seq_no).collect::<Vec<_>>(),
        (0..49).collect::<Vec<_>>()
    );
    for i in 1..=40 {
        let source =
            format!(r#"{{"o":{{"x":-{i}}},"n":{i},"s":"v{i}","f":1.5,"t":true,"z":null}}"#);
        let mut op = index(&format!("b{i}"), &source, i + 2, 1);
        if let Op::Index { routing, .. } = &mut op {
            *routing = (i == 7).then(|| "user2".to_owned());
        }
        assert_eq!(ops[i as usize + 2], op);
    }
    assert_eq!(ops[43], index("c1", r#"{"a":2}"#, 43, 1));
    let Op::NoOp {
        seq_no: 44,
        primary_term: 1,
        reason,
    } = &ops[44]
    else {
        panic!("{:?}", ops[44]);
    };
    assert!(reason.contains("immense term in field=\"w\""), "{reason}");
    assert_eq!(ops[45], index("d1", r#"{"k":["x","y"]}"#, 45, 1));
    assert_eq!(
        ops[46],
        index("d2", "{\"g\":2.5e300,\"s\":\"q\\\"é😀\\n\"}", 46, 1)
    );
    let long = format!(r#"{{"l":"{}"}}"#, "ab ".repeat(22000));
    assert_eq!(ops[47], index("d3", &long, 47, 1));
    let shapes =
        r#"{"u":{"arr":[1,"a",[2.5,null],false],"fix":[1,2],"objs":[{"p":1,"q":[true]}],"e":{}}}"#;
    assert_eq!(ops[48], index("e1", shapes, 48, 1));
}

#[test]
fn keeps_the_auto_generated_id_timestamp() {
    let mut op = index("abc", "{}", 0, 1);
    if let Op::Index {
        auto_id_timestamp, ..
    } = &mut op
    {
        *auto_id_timestamp = 1_700_000_000_123;
    }
    let mut file = translog::header("0123456789abcdefghijkl", 1);
    file.extend(translog::encode(&op));
    assert_eq!(translog::read(&file).unwrap().1, vec![op]);
}
