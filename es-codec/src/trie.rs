//! The block tree's `.tip` index: a trie from term prefixes to blocks in `.tim`
//! (Lucene103 `TrieBuilder` and `TrieReader`).

use crate::output::Output;

const SIGN_NO_CHILDREN: u64 = 0;
const SIGN_SINGLE_CHILD_WITH_OUTPUT: u64 = 1;
const SIGN_SINGLE_CHILD_WITHOUT_OUTPUT: u64 = 2;
const SIGN_MULTI_CHILDREN: u64 = 3;
const LEAF_NODE_HAS_TERMS: u64 = 1 << 5;
const LEAF_NODE_HAS_FLOOR: u64 = 1 << 6;
const NON_LEAF_NODE_HAS_TERMS: u64 = 1 << 1;
const NON_LEAF_NODE_HAS_FLOOR: u64 = 1;

/// Where a prefix's block starts in `.tim`, and the following floor blocks if it was split.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TrieOutput {
    pub fp: u64,
    pub has_terms: bool,
    /// `vint(count) (byte lead label, vlong(fp delta << 1 | has terms))*`, as in `.tip`.
    pub floor: Option<Vec<u8>>,
}

fn bytes_required(v: u64) -> usize {
    8 - ((v | 1).leading_zeros() as usize >> 3)
}

fn write_n_bytes(out: &mut Output, v: u64, n: usize) {
    out.bytes(&v.to_le_bytes()[..n]);
}

fn encode_fp(o: &TrieOutput) -> u64 {
    (o.fp << 2)
        | if o.has_terms {
            NON_LEAF_NODE_HAS_TERMS
        } else {
            0
        }
        | if o.floor.is_some() {
            NON_LEAF_NODE_HAS_FLOOR
        } else {
            0
        }
}

/// Child label encodings, by code: `REVERSE_ARRAY`, `ARRAY`, `BITS`.
#[derive(Clone, Copy)]
enum Strategy {
    ReverseArray = 0,
    Array = 1,
    Bits = 2,
}

impl Strategy {
    fn need_bytes(self, min: usize, max: usize, count: usize) -> usize {
        match self {
            Self::Bits => (max - min + 1).div_ceil(8),
            Self::Array => count - 1,
            Self::ReverseArray => max - min + 1 - count + 1,
        }
    }

    /// `ChildSaveStrategy.choose`: the cheapest, ties going to `BITS`, then `ARRAY`.
    fn choose(min: usize, max: usize, count: usize) -> (Self, usize) {
        let mut best = (Self::Bits, Self::Bits.need_bytes(min, max, count));
        for s in [Self::Array, Self::ReverseArray] {
            let n = s.need_bytes(min, max, count);
            if n < best.1 {
                best = (s, n);
            }
        }
        best
    }

    fn save(self, labels: &[u8], out: &mut Output) {
        match self {
            Self::Bits => {
                let span = (labels[labels.len() - 1] - labels[0]) as usize + 1;
                let mut bits = vec![0u8; span.div_ceil(8)];
                for &l in labels {
                    let i = (l - labels[0]) as usize;
                    bits[i / 8] |= 1 << (i % 8);
                }
                out.bytes(&bits);
            }
            Self::Array => out.bytes(&labels[1..]),
            Self::ReverseArray => {
                out.byte(labels[labels.len() - 1]);
                for w in labels.windows(2) {
                    for absent in w[0] + 1..w[1] {
                        out.byte(absent);
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct Frontier<'a> {
    output: Option<&'a TrieOutput>,
    children: Vec<(u8, u64)>,
}

/// `TrieBuilder.save`: writes the trie of `entries` (keys ascending, the empty key first if
/// present) into `tip`, returning the root node's offset from where the trie starts.
pub(crate) fn save(entries: &[(Vec<u8>, TrieOutput)], tip: &mut Output) -> u64 {
    let start = tip.0.len() as u64;
    let depth = entries.iter().map(|e| e.0.len()).max().unwrap_or(0);
    let mut frontier: Vec<Frontier> = (0..=depth).map(|_| Frontier::default()).collect();
    let mut prev: &[u8] = &[];
    for (key, output) in entries {
        if key.is_empty() {
            frontier[0].output = Some(output);
            continue;
        }
        let common = prev.iter().zip(key).take_while(|(a, b)| a == b).count();
        freeze_from(prev, common, &mut frontier, start, tip);
        frontier[key.len()].output = Some(output);
        prev = key;
    }
    freeze_from(prev, 0, &mut frontier, start, tip);
    freeze_node(&frontier[0], start, tip)
}

fn freeze_from(key: &[u8], to: usize, frontier: &mut [Frontier], start: u64, tip: &mut Output) {
    for d in (to + 1..=key.len()).rev() {
        let fp = freeze_node(&frontier[d], start, tip);
        frontier[d - 1].children.push((key[d - 1], fp));
        frontier[d] = Frontier::default();
    }
}

fn freeze_node(node: &Frontier, start: u64, tip: &mut Output) -> u64 {
    let fp = tip.0.len() as u64 - start;
    let floor = node.output.and_then(|o| o.floor.as_deref());
    match node.children[..] {
        [] => {
            let o = node.output.expect("a trie leaf has an output");
            let n = bytes_required(o.fp);
            let header = SIGN_NO_CHILDREN
                | ((n as u64 - 1) << 2)
                | if o.has_terms { LEAF_NODE_HAS_TERMS } else { 0 }
                | if o.floor.is_some() {
                    LEAF_NODE_HAS_FLOOR
                } else {
                    0
                };
            tip.byte(header as u8);
            write_n_bytes(tip, o.fp, n);
        }
        [(label, child)] => {
            let delta = fp - child;
            let child_bytes = bytes_required(delta);
            let out_bytes = node.output.map_or(0, |o| bytes_required(o.fp << 2));
            let sign = match node.output {
                Some(_) => SIGN_SINGLE_CHILD_WITH_OUTPUT,
                None => SIGN_SINGLE_CHILD_WITHOUT_OUTPUT,
            };
            let header =
                sign | ((child_bytes as u64 - 1) << 2) | ((out_bytes as u64).wrapping_sub(1) << 5);
            tip.byte(header as u8);
            tip.byte(label);
            write_n_bytes(tip, delta, child_bytes);
            if let Some(o) = node.output {
                write_n_bytes(tip, encode_fp(o), out_bytes);
            }
        }
        _ => {
            let labels: Vec<u8> = node.children.iter().map(|c| c.0).collect();
            let (min, max) = (labels[0] as usize, labels[labels.len() - 1] as usize);
            let (strategy, strategy_bytes) = Strategy::choose(min, max, labels.len());
            let child_bytes = bytes_required(fp - node.children[0].1);
            let out_bytes = node.output.map_or(1, |o| bytes_required(o.fp << 2));
            let header = SIGN_MULTI_CHILDREN
                | ((child_bytes as u64 - 1) << 2)
                | (u64::from(node.output.is_some()) << 5)
                | ((out_bytes as u64 - 1) << 6)
                | ((strategy as u64) << 9)
                | ((strategy_bytes as u64 - 1) << 11)
                | ((min as u64) << 16);
            write_n_bytes(tip, header, 3);
            if let Some(o) = node.output {
                write_n_bytes(tip, encode_fp(o), out_bytes);
                if o.floor.is_some() {
                    tip.byte((labels.len() - 1) as u8);
                }
            }
            strategy.save(&labels, tip);
            for &(_, child) in &node.children {
                write_n_bytes(tip, fp - child, child_bytes);
            }
        }
    }
    if let Some(f) = floor {
        tip.bytes(f);
    }
    fp
}
