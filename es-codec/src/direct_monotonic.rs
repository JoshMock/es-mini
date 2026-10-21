//! `DirectWriter` packing and `DirectMonotonicWriter`, shared by doc values and stored fields.

use crate::output::Output;

/// Smallest width `DirectReader` supports that holds `range`.
pub(crate) fn direct_bits(range: u64) -> u8 {
    let need = 64 - range.leading_zeros() as u8;
    [0, 1, 2, 4, 8, 12, 16, 20, 24, 28, 32, 40, 48, 56, 64]
        .into_iter()
        .find(|b| *b >= need)
        .unwrap_or(64)
}

/// `DirectWriter` packing: little-endian, `bits` per value, plus the zero padding that lets
/// Lucene's `DirectReader` read the last value with one 2-, 4- or 8-byte read.
pub(crate) fn pack(values: &[u64], bits: u8) -> Vec<u8> {
    let word = [8, 16, 32, 64]
        .into_iter()
        .find(|w| *w >= bits)
        .unwrap_or(64);
    let padding = if bits > 8 { word - bits } else { 0 }.div_ceil(8);
    let mut packed = vec![0u8; (values.len() * bits as usize).div_ceil(8) + padding as usize];
    for (i, v) in values.iter().enumerate() {
        for b in (0..bits as usize).filter(|b| v >> b & 1 == 1) {
            let bit = i * bits as usize + b;
            packed[bit / 8] |= 1 << (bit % 8);
        }
    }
    packed
}

/// `DirectMonotonicWriter`: block metadata to `meta`, packed deltas returned.
pub(crate) fn write_direct_monotonic(
    meta: &mut Output,
    values: &[i64],
    block_shift: i32,
) -> Vec<u8> {
    let mut data = Vec::new();
    for chunk in values.chunks(1 << block_shift) {
        let avg =
            ((chunk[chunk.len() - 1] - chunk[0]) as f64 / (chunk.len() - 1).max(1) as f64) as f32;
        let mut deltas: Vec<i64> = chunk
            .iter()
            .enumerate()
            .map(|(i, v)| v - (avg * i as f32) as i64)
            .collect();
        let min = deltas.iter().copied().min().unwrap_or(0);
        deltas.iter_mut().for_each(|d| *d -= min);
        let bits = direct_bits(deltas.iter().fold(0, |m, d| m | *d as u64));
        meta.long(min);
        meta.bytes(&avg.to_bits().to_le_bytes());
        meta.long(data.len() as i64);
        meta.byte(bits);
        data.extend(pack(
            &deltas.iter().map(|d| *d as u64).collect::<Vec<_>>(),
            bits,
        ));
    }
    data
}

#[cfg(test)]
mod tests {
    #[test]
    fn pack_pads_for_whole_word_reads_like_direct_writer() {
        // Lucene's DirectReader reads 40-bit values as longs, 20-bit ones as ints, and so on.
        for (bits, len) in [(1, 1), (8, 2), (12, 4), (20, 7), (40, 13), (64, 16)] {
            assert_eq!(super::pack(&[1, 1], bits).len(), len, "{bits} bits");
        }
    }
}
