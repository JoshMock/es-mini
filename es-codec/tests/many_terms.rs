//! A terms dictionary big enough for sub-blocks, floor blocks, and both suffix compressions,
//! from an index written by real Lucene 10.5.1: see tests/gen/GenManyTerms.java (`terms()` there
//! must match `terms` here). Doc d holds term d of field "t"; the 100 docs after them hold
//! field "q".

mod common;

fn base36(mut n: u32) -> String {
    let mut out = vec![];
    loop {
        out.push(char::from_digit(n % 36, 36).unwrap());
        n /= 36;
        if n == 0 {
            return out.iter().rev().collect();
        }
    }
}

fn terms() -> Vec<String> {
    let mut out: Vec<String> = (0..60)
        .map(|i| format!("lz4{i:03}-abcdefabcdefabcdef"))
        .collect();
    let mut x: u64 = 42;
    for i in 0..60 {
        let mut t = "low".to_string();
        for _ in 0..8 {
            x = (x * 1_103_515_245 + 12345) & 0x7fff_ffff;
            t.push(char::from(b'a' + ((x >> 16) % 26) as u8));
        }
        out.push(format!("{t}{i}"));
    }
    out.extend((0..400).map(|i| format!("flo{i:03}")));
    out.extend((0..100).map(|i| base36(i * 37)));
    out
}

#[test]
fn reads_a_multi_block_terms_dictionary() {
    let seg = common::open("many-terms");
    let mut want: Vec<(String, u32)> = terms().into_iter().zip(0..).collect();
    want.sort();
    let got: Vec<(String, Vec<(u32, u32)>)> = seg
        .terms("t")
        .unwrap()
        .into_iter()
        .map(|t| (String::from_utf8(t.term).unwrap(), t.docs))
        .collect();
    assert_eq!(got.len(), want.len());
    for ((term, docs), (want_term, doc)) in got.iter().zip(&want) {
        assert_eq!(term, want_term);
        assert_eq!(docs, &[(*doc, 1)], "{term}");
    }

    // Every "q" term starts with the same byte: the trie root has one child.
    let q = seg.terms("q").unwrap();
    let base = want.len() as u32;
    let q: Vec<_> = q.into_iter().map(|t| (t.term, t.docs)).collect();
    let want_q: Vec<_> = (0..100)
        .map(|i| (format!("q{i:03}").into_bytes(), vec![(base + i, 1)]))
        .collect();
    assert_eq!(q, want_q);
}

#[test]
fn looks_up_one_term_without_decoding_the_others() {
    let seg = common::open("many-terms");
    for (term, doc) in terms().into_iter().zip(0..) {
        let got = seg.term("t", term.as_bytes()).unwrap().unwrap();
        assert_eq!((got.term, got.docs), (term.into_bytes(), vec![(doc, 1)]));
    }
    assert_eq!(seg.term("t", b"absent").unwrap(), None);
    assert_eq!(seg.term("q", b"q042").unwrap().unwrap().docs.len(), 1);
}
