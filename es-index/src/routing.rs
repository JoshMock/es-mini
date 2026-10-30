//! Document-to-shard routing, as Elasticsearch's `IndexRouting.Unpartitioned`,
//! `RoutingFunction`, and `Murmur3HashFunction` (9.5.5).

/// The shard a document goes to: murmur3 of `routing` (or `id` without one), floor-modulo
/// `number_of_shards` (`RoutingFunction.ModuloRoutingFunction`). Indices created before
/// `IndexVersions.MOD_ROUTING_FUNCTION` used `routing_num_shards` instead; es-mini only creates
/// 9.5.5 indices.
pub fn shard_id(id: &str, routing: Option<&str>, number_of_shards: u32) -> u32 {
    murmur3_utf16(routing.unwrap_or(id)).rem_euclid(number_of_shards as i32) as u32
}

/// Lucene's `murmurhash3_x86_32` (seed 0) over the string's UTF-16 code units, little-endian.
fn murmur3_utf16(s: &str) -> i32 {
    let bytes: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mix = |k: u32| {
        k.wrapping_mul(0xcc9e_2d51)
            .rotate_left(15)
            .wrapping_mul(0x1b87_3593)
    };
    let (chunks, tail) = bytes.as_chunks::<4>();
    let mut h = 0u32;
    for c in chunks {
        h ^= mix(u32::from_le_bytes(*c));
        h = h.rotate_left(13).wrapping_mul(5).wrapping_add(0xe654_6b64);
    }
    if !tail.is_empty() {
        h ^= mix(tail.iter().rev().fold(0, |k, &b| (k << 8) | u32::from(b)));
    }
    h ^= bytes.len() as u32;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    (h ^ (h >> 16)) as i32
}
