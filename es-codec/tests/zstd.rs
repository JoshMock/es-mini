//! es_codec::zstd decodes frames the zstd CLI wrote (tests/gen/gen_zstd.sh): raw, RLE and
//! compressed blocks, raw and Huffman (one and four streams, treeless) literals, predefined, RLE
//! and FSE tables, and a content checksum.

use es_codec::zstd::decompress;
use std::path::PathBuf;

const WORDS: &str = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike \
    november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu red \
    orange yellow green blue indigo violet black white grey brown pink cyan magenta";

fn lcg(seed: u64) -> impl Iterator<Item = u64> {
    std::iter::successors(Some(seed), |x| {
        Some(
            x.wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407),
        )
    })
    .skip(1)
    .map(|x| x >> 33)
}

fn words(n: usize, seed: u64) -> Vec<u8> {
    let words: Vec<&str> = WORDS.split_whitespace().collect();
    let mut s = String::new();
    let mut r = lcg(seed);
    while s.len() < n {
        s += words[(r.next().unwrap() % 40) as usize];
        s += " ";
    }
    s.truncate(n);
    s.into_bytes()
}

/// Words, with now and then a line of dashes: statistics that drift from block to block.
fn lines(n: usize, seed: u64) -> Vec<u8> {
    let words: Vec<&str> = WORDS.split_whitespace().collect();
    let mut s = String::new();
    for v in lcg(seed) {
        if s.len() >= n {
            break;
        }
        s += words[(v % 40) as usize];
        if v % 9 == 0 {
            s += &format!("\n{}\n", "-".repeat((v % 50) as usize));
        } else {
            s += " ";
        }
    }
    s.truncate(n);
    s.into_bytes()
}

fn noise(n: usize, seed: u64, modulo: u64) -> Vec<u8> {
    lcg(seed).take(n).map(|x| (x % modulo) as u8).collect()
}

#[test]
fn decodes_cli_written_frames() {
    let cases = [
        ("empty", vec![]),
        ("hello", b"hello world".to_vec()),
        ("rle", vec![b'a'; 5000]),
        ("words-small", words(1000, 1)),
        ("words-3", words(160_000, 2)),
        ("words-19", words(160_000, 2)),
        ("lines", lines(280_000, 5)),
        ("noise", noise(5000, 3, 256)),
        ("nibbles", noise(20_000, 4, 16)),
    ];
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/zstd");
    for (name, want) in cases {
        let frame = std::fs::read(dir.join(format!("{name}.zst"))).unwrap();
        let got = decompress(&frame).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert!(got == want, "{name}: decoded {} bytes differ", got.len());
    }
}

#[test]
fn rejects_a_truncated_frame() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/zstd");
    let frame = std::fs::read(dir.join("words-3.zst")).unwrap();
    assert!(decompress(&frame[..frame.len() / 2]).is_err());
}
