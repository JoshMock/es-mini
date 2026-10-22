//! Elasticsearch's HyperLogLog++ cardinality counter, ported from `HyperLogLogPlusPlus`,
//! `AbstractHyperLogLog` and `AbstractLinearCounting` at 9.5.5: values are hashed like
//! `CardinalityAggregator` does, counted exactly (linear counting of 32-bit encoded hashes) up
//! to a threshold set by the precision, then in HyperLogLog registers with bias correction.

use crate::hll_tables::{BIAS_DATA, RAW_ESTIMATE_DATA};
use std::collections::HashSet;

/// The precision without a `precision_threshold`.
pub(crate) const DEFAULT_PRECISION: u32 = 14;
const P2: u32 = 25;
const BIAS_K: usize = 6;
const THRESHOLDS: [i64; 15] = [
    10, 20, 40, 80, 220, 400, 900, 1800, 3100, 6500, 11500, 20000, 50000, 120000, 350000,
];

/// The precision that counts `count` distinct values with linear counting.
pub(crate) fn precision_from_threshold(count: u64) -> u32 {
    // Java divides by the float load factor 0.75f
    let entries = f64::from(count as f32 / 0.75f32).ceil() as u64;
    let bits = 64 - (entries * 4).max(1).leading_zeros();
    bits.clamp(4, 18)
}

/// hppc's `BitMixer.mix64`, the hash of long and double values.
pub(crate) fn mix64(z: u64) -> u64 {
    let z = (z ^ (z >> 32)).wrapping_mul(0x4cd6_944c_5cc2_0b6d);
    let z = (z ^ (z >> 29)).wrapping_mul(0xfc12_c5b1_9d32_59e9);
    z ^ (z >> 32)
}

/// The first half (`h1`) of MurmurHash3_x64_128 with seed 0, the hash of string values.
pub(crate) fn murmur3(key: &[u8]) -> u64 {
    const C1: u64 = 0x87c3_7b91_1142_53d5;
    const C2: u64 = 0x4cf5_ad43_2745_937f;
    let mix1 = |k: u64| k.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2);
    let mix2 = |k: u64| k.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1);
    let fmix = |mut k: u64| {
        k ^= k >> 33;
        k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
        k ^= k >> 33;
        k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        k ^ (k >> 33)
    };
    let le = |b: &[u8]| {
        let mut w = [0u8; 8];
        w[..b.len()].copy_from_slice(b);
        u64::from_le_bytes(w)
    };
    let (mut h1, mut h2) = (0u64, 0u64);
    let (blocks, tail) = key.as_chunks::<16>();
    for b in blocks {
        h1 ^= mix1(le(&b[..8]));
        h1 = h1.rotate_left(27).wrapping_add(h2);
        h1 = h1.wrapping_mul(5).wrapping_add(0x52dc_e729);
        h2 ^= mix2(le(&b[8..]));
        h2 = h2.rotate_left(31).wrapping_add(h1);
        h2 = h2.wrapping_mul(5).wrapping_add(0x3849_5ab5);
    }
    if tail.len() > 8 {
        h2 ^= mix2(le(&tail[8..]));
    }
    if !tail.is_empty() {
        h1 ^= mix1(le(&tail[..tail.len().min(8)]));
    }
    h1 ^= key.len() as u64;
    h2 ^= key.len() as u64;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    fmix(h1).wrapping_add(fmix(h2))
}

/// Java's `Math.round`.
fn round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

fn linear_counting(m: i64, v: i64) -> i64 {
    round(m as f64 * (m as f64 / v as f64).ln())
}

/// One cardinality counter.
#[derive(Debug, Clone)]
pub(crate) struct Hll {
    p: u32,
    /// Linear counting: the distinct encoded hashes, until there are more than the threshold.
    linear: HashSet<u32>,
    /// HyperLogLog run lengths, one register per `1 << p`, once linear counting is over.
    registers: Option<Vec<u8>>,
}

impl Hll {
    pub(crate) fn new(p: u32) -> Self {
        Hll {
            p,
            linear: HashSet::new(),
            registers: None,
        }
    }

    pub(crate) fn collect(&mut self, hash: u64) {
        let p = self.p;
        match &mut self.registers {
            Some(r) => {
                let run = 1 + (hash << p).leading_zeros().min(64 - p);
                let i = (hash >> (64 - p)) as usize;
                r[i] = r[i].max(run as u8);
            }
            None => self.add_encoded(encode_hash(hash, p)),
        }
    }

    fn add_encoded(&mut self, encoded: u32) {
        if let Some(r) = &mut self.registers {
            let i = decode_index(encoded, self.p);
            r[i] = r[i].max(decode_run_len(encoded, self.p));
            return;
        }
        self.linear.insert(encoded);
        // the threshold of a hash table of `(1 << p) / 4` entries at load factor 0.75
        if self.linear.len() > (1 << self.p) / 4 * 3 / 4 {
            self.upgrade();
        }
    }

    /// Replays the linear counting hashes into HyperLogLog registers.
    fn upgrade(&mut self) -> &mut Vec<u8> {
        if self.registers.is_none() {
            self.registers = Some(vec![0; 1 << self.p]);
            for e in std::mem::take(&mut self.linear) {
                self.add_encoded(e);
            }
        }
        self.registers.as_mut().expect("just set")
    }

    /// Merges another shard's counter of the same precision into this one.
    pub(crate) fn merge(&mut self, other: &Hll) {
        match &other.registers {
            None => other.linear.iter().for_each(|&e| self.add_encoded(e)),
            Some(o) => {
                let r = self.upgrade();
                r.iter_mut().zip(o).for_each(|(a, b)| *a = (*a).max(*b));
            }
        }
    }

    pub(crate) fn cardinality(&self) -> i64 {
        let Some(r) = &self.registers else {
            let m = 1i64 << P2;
            return linear_counting(m, m - self.linear.len() as i64);
        };
        let p = self.p as usize;
        let m = 1i64 << p;
        let inverse_sum: f64 = r.iter().map(|&x| 1.0 / (1i64 << x) as f64).sum();
        let zeros = r.iter().filter(|&&x| x == 0).count() as i64;
        let alpha = match p {
            4 => 0.673,
            5 => 0.697,
            _ => 0.7213 / (1.0 + 1.079 / m as f64),
        };
        let e1 = alpha * m as f64 * m as f64 / inverse_sum;
        let e2 = match e1 <= (5 * m) as f64 {
            true => e1 - estimate_bias(p, e1),
            false => e1,
        };
        let h = match zeros {
            0 => round(e2),
            _ => linear_counting(m, zeros),
        };
        match h <= THRESHOLDS[p - 4] {
            true => h,
            false => round(e2),
        }
    }
}

/// Encodes a hash on 32 bits (never 0) for linear counting.
fn encode_hash(hash: u64, p: u32) -> u32 {
    let e = hash >> (64 - P2);
    if e & ((1 << (P2 - p)) - 1) == 0 {
        let run = 1 + u64::from((hash << P2).leading_zeros().min(64 - P2));
        ((e << 7) | (run << 1) | 1) as u32
    } else {
        (e << 1) as u32
    }
}

fn decode_run_len(encoded: u32, p: u32) -> u8 {
    let run = match encoded & 1 {
        1 => ((encoded >> 1) & 0x3F) + (P2 - p),
        _ => 1 + (encoded << (31 + p - P2)).leading_zeros(),
    };
    run as u8
}

fn decode_index(encoded: u32, p: u32) -> usize {
    let index = match encoded & 1 {
        1 => encoded >> 7,
        _ => encoded >> 1,
    };
    (index >> (P2 - p)) as usize
}

/// The bias of raw estimate `e`, interpolated from its nearest table entries as ES does
/// (including its pairing of weights with entries when the scan stops mid-cycle).
fn estimate_bias(p: usize, e: f64) -> f64 {
    let (raw, bias) = (RAW_ESTIMATE_DATA[p - 4], BIAS_DATA[p - 4]);
    let mut weights = [0.0; BIAS_K];
    let mut index = bias.len() - BIAS_K;
    for (i, r) in raw.iter().enumerate() {
        let w = 1.0 / (r - e).abs();
        let j = i % BIAS_K;
        if w.is_infinite() {
            return bias[i];
        } else if weights[j] >= w {
            index = i - BIAS_K;
            break;
        }
        weights[j] = w;
    }
    let (mut weight_sum, mut bias_sum) = (0.0, 0.0);
    for (w, b) in weights.iter().zip(&bias[index..]) {
        bias_sum += w * b;
        weight_sum += w;
    }
    bias_sum / weight_sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precision_from_threshold_matches_es() {
        assert_eq!(precision_from_threshold(0), 4);
        assert_eq!(precision_from_threshold(100), 10);
        assert_eq!(precision_from_threshold(3000), 14);
        assert_eq!(precision_from_threshold(40000), 18);
        assert_eq!(precision_from_threshold(u64::MAX >> 4), 18);
    }

    #[test]
    fn linear_counting_is_exact_for_few_values_and_merges() {
        let mut a = Hll::new(DEFAULT_PRECISION);
        let mut b = Hll::new(DEFAULT_PRECISION);
        (0..1000u64).for_each(|i| a.collect(mix64(i)));
        (500..1500u64).for_each(|i| b.collect(mix64(i)));
        assert_eq!(a.cardinality(), 1000);
        a.merge(&b);
        assert_eq!(a.cardinality(), 1500);
    }
}
