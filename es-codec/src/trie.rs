//! The block tree's `.tip` index: a trie from term prefixes to blocks in `.tim`
//! (Lucene103 `TrieBuilder` and `TrieReader`).

use crate::{Result, err, output::Output};

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

/// A node's output as read: its block's `.tim` pointer, and where its floor data starts.
pub(crate) struct NodeOutput {
    pub fp: u64,
    pub has_terms: bool,
    pub floor: Option<usize>,
}

/// One trie node (`TrieReader.Node`); positions are offsets into the trie's bytes.
pub(crate) struct Node {
    fp: usize,
    sign: u64,
    child_delta: u64,
    min_label: u8,
    strategy: u64,
    strategy_fp: usize,
    strategy_bytes: usize,
    child_bytes: usize,
    pub output: Option<NodeOutput>,
}

fn mask(bytes_minus_1: u64) -> u64 {
    u64::MAX >> (8 * (7 - bytes_minus_1))
}

/// A field's trie: `.tip` from the field's index start to its index end.
pub(crate) struct TrieReader<'a>(pub &'a [u8]);

impl TrieReader<'_> {
    /// Like Lucene, reads 8 bytes even near the end: the trie ends with 8 bytes of padding.
    fn long(&self, pos: usize) -> Result<u64> {
        match self.0.get(pos..pos.saturating_add(8)) {
            Some(b) => Ok(u64::from_le_bytes(b.try_into().unwrap())),
            None => err("trie node extends past the index"),
        }
    }

    fn byte(&self, pos: usize) -> Result<u8> {
        self.0
            .get(pos)
            .copied()
            .ok_or_else(|| crate::Error("trie node extends past the index".into()))
    }

    pub fn load(&self, fp: usize) -> Result<Node> {
        let first = self.long(fp)?;
        let flags = first & 0xffff_ffff;
        let mut node = Node {
            fp,
            sign: flags & 3,
            child_delta: 0,
            min_label: 0,
            strategy: 0,
            strategy_fp: 0,
            strategy_bytes: 0,
            child_bytes: 0,
            output: None,
        };
        let non_leaf = |encoded: u64, floor: usize| NodeOutput {
            fp: encoded >> 2,
            has_terms: encoded & NON_LEAF_NODE_HAS_TERMS != 0,
            floor: (encoded & NON_LEAF_NODE_HAS_FLOOR != 0).then_some(floor),
        };
        match node.sign {
            SIGN_NO_CHILDREN => {
                let n = (flags >> 2) & 7;
                let out_fp = if n <= 6 {
                    (first >> 8) & mask(n)
                } else {
                    self.long(fp + 1)?
                };
                node.output = Some(NodeOutput {
                    fp: out_fp,
                    has_terms: flags & LEAF_NODE_HAS_TERMS != 0,
                    floor: (flags & LEAF_NODE_HAS_FLOOR != 0).then_some(fp + 2 + n as usize),
                });
            }
            SIGN_MULTI_CHILDREN => {
                node.child_bytes = ((flags >> 2) & 7) as usize + 1;
                node.strategy = (flags >> 9) & 3;
                node.strategy_bytes = ((flags >> 11) & 0x1f) as usize + 1;
                node.min_label = (flags >> 16) as u8;
                node.strategy_fp = fp + 3;
                if flags & 0x20 != 0 {
                    let n = (flags >> 6) & 7;
                    let l = if n <= 4 {
                        first >> 24
                    } else {
                        self.long(fp + 3)?
                    };
                    let encoded = l & mask(n);
                    node.strategy_fp = fp + 4 + n as usize;
                    let mut children = 0;
                    if encoded & NON_LEAF_NODE_HAS_FLOOR != 0 {
                        children = self.byte(node.strategy_fp)? as usize + 1;
                        node.strategy_fp += 1;
                    }
                    let floor =
                        node.strategy_fp + node.strategy_bytes + children * node.child_bytes;
                    node.output = Some(non_leaf(encoded, floor));
                }
            }
            sign => {
                let n = (flags >> 2) & 7;
                let l = if n <= 5 {
                    first >> 16
                } else {
                    self.long(fp + 2)?
                };
                node.child_delta = l & mask(n);
                node.min_label = (flags >> 8) as u8;
                if sign == SIGN_SINGLE_CHILD_WITH_OUTPUT {
                    let m = (flags >> 5) & 7;
                    let at = fp + n as usize + 3;
                    let encoded = self.long(at)? & mask(m);
                    node.output = Some(non_leaf(encoded, at + m as usize + 1));
                }
            }
        }
        Ok(node)
    }

    /// `TrieReader.lookupChild`: the child of `parent` reached by `label`, if any.
    pub fn child(&self, parent: &Node, label: u8) -> Result<Option<Node>> {
        let delta = match parent.sign {
            SIGN_NO_CHILDREN => return Ok(None),
            SIGN_MULTI_CHILDREN => {
                let Some(pos) = self.child_position(parent, label)? else {
                    return Ok(None);
                };
                let at = parent.strategy_fp + parent.strategy_bytes + parent.child_bytes * pos;
                self.long(at)? & mask(parent.child_bytes as u64 - 1)
            }
            _ if label == parent.min_label => parent.child_delta,
            _ => return Ok(None),
        };
        match (parent.fp as u64).checked_sub(delta).filter(|_| delta > 0) {
            Some(fp) => self.load(fp as usize).map(Some),
            None => err("corrupt trie child pointer"),
        }
    }

    /// `ChildSaveStrategy.lookup`: the index of `label` among a multi-child node's children.
    fn child_position(&self, node: &Node, label: u8) -> Result<Option<usize>> {
        let min = node.min_label;
        if label < min {
            return Ok(None);
        }
        let Some(data) = self
            .0
            .get(node.strategy_fp..node.strategy_fp + node.strategy_bytes)
        else {
            return err("trie node extends past the index");
        };
        let rel = (label - min) as usize;
        Ok(match node.strategy {
            _ if label == min => Some(0),
            2 => (rel < data.len() * 8 && data[rel / 8] & (1 << (rel % 8)) != 0).then(|| {
                data[..rel / 8]
                    .iter()
                    .map(|b| b.count_ones() as usize)
                    .sum::<usize>()
                    + (data[rel / 8] & ((1 << (rel % 8)) - 1)).count_ones() as usize
            }),
            1 => data.binary_search(&label).ok().map(|i| i + 1),
            0 => {
                let absent = &data[1..];
                (label <= data[0] && !absent.contains(&label))
                    .then(|| rel - absent.iter().filter(|&&a| a < label).count())
            }
            s => return err(format!("unknown trie child strategy {s}")),
        })
    }
}
