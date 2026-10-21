---
name: port-lucene-format
description: Port a Lucene 10.5.1 or Elasticsearch 9.5.5 on-disk format, codec detail, or algorithm (tokenizer, scoring) into es-mini's Rust crates. Use when adding or fixing readers and writers in es-codec, or matching Lucene/Elasticsearch behavior exactly.
---

# Port a Lucene or Elasticsearch format

es-mini must be byte-compatible with Elasticsearch 9.5.5 and Lucene 10.5.1 (see
COMPATIBILITY.md). Port from their source at those versions; do not work from memory.

## Safety Rules (Do Not Violate)

- Fetch sources at the pinned versions only, with
  `.agents/skills/port-lucene-format/scripts/fetch_source.sh lucene|es <path>` (or
  `es-find <ClassName>` to locate an Elasticsearch class).
- No new crates for codec work (hand-write CRC, LZ4, DEFLATE framing, SMILE, ...).
- Writers must never panic on input they cannot encode: return `es_codec::Error`, and make the
  caller decide (fail the save, or leave a field out and say so in a `ponytail:` comment).

## Suggested Workflow

1. Find the codec that is actually on disk before porting: the per-segment codec name in
   `segments_N` and the per-field formats in `.fnm` attributes (the fixture uses `Lucene104`
   postings, `Lucene90` stored fields and doc values, and `ES87BloomFilter` for `_id`).
2. Fetch the reader and writer Java classes. Tell **format constraints** (what the reader
   requires) from **writer policies** (what Lucene's writer happens to do): for example,
   Lucene's writer caps block-tree leaf blocks at 48 terms, but its reader accepts any count.
3. Reader first: decode the fixture's bytes (`es-codec/tests/fixture.rs`) and assert values you
   know from how the fixture was built. Small generated inputs live in `es-codec/tests/data/`.
4. Writer second: Rust-writes then Rust-reads round-trip tests, and byte-for-byte comparison
   with the fixture where the content is the same.
5. Then have Elasticsearch read it (see the verify-with-elasticsearch skill). A round trip
   through es-codec alone does not prove Lucene accepts the bytes.

## Notes

- Ground truth for algorithms: Lucene's own tests and generated tables beat hand-written ones.
  The StandardTokenizer is a port of its JFlex DFA tables
  (`scripts/gen_standard_tokenizer_tables.py`) tested against Lucene's test expectations
  (`scripts/extract_lucene_analysis_tests.py`).
- Elasticsearch wraps Lucene in places that change results; check the Elasticsearch side too
  (e.g. `LegacyBM25Similarity` multiplies Lucene's BM25 by `1 + k1`).
- Java cannot start inside the sandbox (it cannot read `/etc/java21-openjdk/security/java.security`),
  so run Lucene code only through the Elasticsearch Docker image, or not at all.
- Lucene104 postings blocks hold 256 docs; level-1 skip data starts at 8192 docs.
