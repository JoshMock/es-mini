# Tutorial 15 — Writing Binary Formats: Lucene Segment Writer

**Phase 5, Tasks 19–21. The final tutorial in the series.**

## What you've built

Fourteen tutorials ago this was an empty Cargo workspace. Since then: a full ES-shaped HTTP
API (index management, document CRUD, bulk, search, ES|QL) backed by an in-memory store; a YAML
REST test harness running real `rest-api-spec` cases in CI; and, in Tutorial 14, a **reader**
that decodes real segments a Java Elasticsearch wrote — `segments_N`, `.si`, `.fnm`, `.fdt`/
`.fdx` — well enough to pull `_source` back out of them.

This tutorial closes the loop: **write** those same formats. Not the full Lucene write path —
that's a multi-year project inside Lucene itself (postings, doc values, points, compound files,
soft deletes, merges). The scope is exactly what Task 19's DoD asks for: a Rust server that can
create an index, ingest documents, and produce on-disk segments that (a) its own Tutorial 14
reader can read back byte-for-byte, and (b) a real Java Elasticsearch can open on the same data
directory and return correct results for. Search itself keeps running against the in-memory
`InvertedIndex` (Tutorial 11) regardless of storage backend — this tutorial is only about what
lands on disk, not about replacing the query engine.

Every format fact below was re-verified against Lucene source at tag `releases/lucene/10.1.0`
during authoring, same discipline as Tutorial 14. Two corrections to the original task brief
surfaced from that verification — both called out explicitly where they matter (§9, §17).

---

## Part 1: Binary Write I/O

### 1. `Write`, mirrored from `Read`

Tutorial 14 built everything on `std::io::Read`. The write side is its mirror:

```rust
use std::io::{self, Write};

trait Write {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize>; // may write fewer bytes than given
    fn flush(&mut self) -> io::Result<()>;
    // provided:
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> { /* loops write() to completion */ }
}
```

Same rule as `read_exact` (Tutorial 14 §1): use `write_all`, not raw `write`, for anything where
a short write would corrupt the file. Every write helper in `es-codec` is built on `write_all`.

`flush()` matters because `BufWriter` (next) buffers in memory; nothing is guaranteed on disk
until you flush (or drop the `BufWriter`, which flushes automatically — but ignore-on-drop
errors are exactly the footgun `.flush()` explicit calls exist to avoid, same reasoning as
Tutorial 13's `Drop`-impl error-swallowing discussion, applied in the opposite direction: here
you *want* to see the error, so you call `.flush()` yourself instead of relying on `Drop`).

```rust
use std::io::BufWriter;
use std::fs::File;

let file = File::create(&tmp_path)?;
let mut w = BufWriter::new(file);
write_header(&mut w, "Lucene90SegmentInfo", 0)?;
w.flush()?;
```

Same analogy as Tutorial 14's `BufReader`: Python's `open(path, 'wb')` already buffers; Rust's
`File` does not, so `BufWriter` wrapping is not optional here either.

### 2. Writing multi-byte integers, BE and LE

Mirrors Tutorial 14 §4's table exactly — same split, same exceptions:

```rust
pub fn write_i32_be(w: &mut impl Write, v: i32) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}
pub fn write_i32_le(w: &mut impl Write, v: i32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
pub fn write_i64_be(w: &mut impl Write, v: i64) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}
pub fn write_u32_be(w: &mut impl Write, v: u32) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}
```

`i32::to_be_bytes()` / `to_le_bytes()` return `[u8; 4]` — a fixed-size array, stack-allocated,
no heap involved. `write_all` takes `&[u8]`, and `&[u8; 4]` coerces to that automatically (array
→ slice coercion, the write-side mirror of the array-literal `[0u8; 4]` buffer Tutorial 14 read
into).

Header/footer magic and most of `segments_N`/`.si`'s scalar fields are **big-endian**
(`CodecUtil.writeBEInt`/`writeBELong` — this is the same "legacy back-compat" BE path Tutorial 14
found at header/footer boundaries, verified in §9 below to also cover several `segments_N`
fields, which is a slightly wider BE footprint than Tutorial 14's read side needed to touch).
Ordinary scalar fields elsewhere (`.si`'s version triple, `.fnm`'s `dvGen`) are **little-endian**
(`DataOutput.writeInt`) — same default-is-LE rule as Tutorial 14.

### 3. VInt / VLong — the encode side

Tutorial 14 decoded these; here's the mirror-image encoder. Read algorithm was "peel 7 bits at a
time off the low end while the continuation bit is set." Write is the same shift, backwards:

```rust
pub fn write_vint(w: &mut impl Write, mut value: u32) -> io::Result<()> {
    loop {
        if value & !0x7F == 0 {
            w.write_all(&[value as u8])?;
            return Ok(());
        }
        w.write_all(&[((value & 0x7F) | 0x80) as u8])?;
        value >>= 7;
    }
}

pub fn write_vlong(w: &mut impl Write, mut value: u64) -> io::Result<()> {
    loop {
        if value & !0x7F == 0 {
            w.write_all(&[value as u8])?;
            return Ok(());
        }
        w.write_all(&[((value & 0x7F) | 0x80) as u8])?;
        value >>= 7;
    }
}
```

Worked example, mirroring Tutorial 14's `300` decode: `300 = 0b1_0010_1100`. Low 7 bits =
`0101100` = `0x2C`, continuation bit set → first byte `0xAC`. Remaining `300 >> 7 = 2`, under
`0x7F` → final byte `0x02`. Output `[0xAC, 0x02]` — identical bytes Tutorial 14 decoded back into
`300`. This round-trip *is* the test (see §17).

### 4. Length-prefixed strings

```rust
pub fn write_string(w: &mut impl Write, s: &str) -> io::Result<()> {
    let bytes = s.as_bytes();
    write_vint(w, bytes.len() as u32)?;
    w.write_all(bytes)
}
```

Mirror of Tutorial 14's `read_string`. `s.as_bytes()` borrows the UTF-8 bytes already backing the
`&str` — no allocation, no re-encoding (Rust strings are UTF-8 internally; this is just exposing
the existing buffer as `&[u8]`).

### 5. Atomic writes: temp file + rename

Every Lucene file write in `es-codec` follows one rule: **never write directly to the final
filename.** Write to `<name>.tmp` in the same directory, flush, then `std::fs::rename` the temp
file over the final name.

```rust
use std::path::Path;

fn write_atomic(dir: &Path, final_name: &str, contents: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> io::Result<()> {
    let tmp_path = dir.join(format!("{final_name}.tmp"));
    {
        let file = std::fs::File::create(&tmp_path)?;
        let mut w = BufWriter::new(file);
        contents(&mut w)?;
        w.flush()?;
    } // file closed here — required before rename on some platforms, cheap insurance on all of them
    std::fs::rename(&tmp_path, dir.join(final_name))?;
    Ok(())
}
```

`rename(2)` is atomic on POSIX filesystems when source and destination are on the *same*
filesystem: the directory entry flips from old name to new in one operation, there is no instant
where a reader sees a partially-written file under the final name. (Windows' `MoveFileEx` has
different semantics here — irrelevant: `COMPATIBILITY.md` scopes this project to Linux only,
same policy already applied to `127.0.0.1`-only binding.)

Same pattern as safe config deploys (write `config.json.tmp`, `mv` over `config.json`) and
database WAL/checkpoint files — never leave a half-written file visible under a name anything
else might open concurrently. Every `.si`/`segments_N`/`.fdt` write in this tutorial goes through
`write_atomic`; skipping it for "just this one file" is exactly the kind of shortcut that
produces the 3am corrupt-segment page.

### 6. `tempfile` crate — `NamedTempFile`

`write_atomic` above is correct but hand-rolled: it doesn't clean up the `.tmp` file if
`contents(...)` panics or errors before the rename. `tempfile = "3"` (current: **3.27.0**) gives
a `NamedTempFile` that deletes itself on `Drop` unless explicitly persisted — the RAII guard
pattern from Tutorial 06 (`MutexGuard`) and Tutorial 13 (`ServerGuard`), applied to a filesystem
path instead of a lock or a process:

```rust
use tempfile::NamedTempFile;

fn write_atomic(dir: &Path, final_name: &str, contents: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> io::Result<()> {
    let mut tmp = NamedTempFile::new_in(dir)?; // same filesystem as `dir` — required for atomic rename
    {
        let mut w = BufWriter::new(tmp.as_file_mut());
        contents(&mut w)?;
        w.flush()?;
    }
    tmp.persist(dir.join(final_name))?; // renames; consumes `tmp`, disarming its Drop-delete
    Ok(())
}
```

`NamedTempFile::new_in(dir)` (not the plain `NamedTempFile::new()`, which uses the system temp
directory — almost always a *different* filesystem, e.g. `/tmp` as `tmpfs` vs a data directory on
a real disk, which would make the later rename a cross-filesystem copy, not atomic). `.persist()`
does the rename and returns the now-permanent `File`; if `contents(...)` returns `Err` before
that point, `tmp` drops and the partial temp file is deleted automatically — no manual cleanup
code, no leaked `.tmp` files after a crash mid-write. This is the version used in `es-codec`
going forward; the hand-rolled version above was the teaching step.

---

## Part 2: Writing Lucene segments

### 7. Scope, restated precisely

Tutorial 14 read: `segments_N`, `.si`, `.fnm`, `.fdt`/`.fdx`. This tutorial writes:
`segments_N`, `.si`, `.fdt`/`.fdx`. **`.fnm` (field infos) is written too — an index needs it to
declare its field names/numbers even for a pure stored-fields MVP** — but postings (`.tim`/
`.tip`), doc values (`.dvd`/`.dvm`), points/BKD trees, and compound files (`.cfs`/`.cfe`) are not
implemented on the write side, mirroring Tutorial 14's read-side scope exactly. Every one of
these gaps gets a `// ponytail:` comment at its logical call site and shows up again in §20's
"Known limitations" list — this is the single biggest scope decision in the whole series, so it
gets said twice.

Search after a write still goes through the in-memory `InvertedIndex` (Tutorial 11) — writing a
segment does not currently make it queryable by `DiskBackend::search` (that method is still the
`Err(...)` stub Tutorial 14 left it as). What lands on disk in this tutorial is *durability* and
*cross-engine readability*, not a new search execution path.

### 8. `write_header` — `CodecUtil.writeHeader`

Every Lucene file's `IndexHeader` (Tutorial 14 §11 read this; here's the write mirror):

```rust
pub const CODEC_MAGIC: i32 = 0x3fd76c17; // same constant Tutorial 14 defined; re-exported, not redefined

pub fn write_index_header(
    w: &mut impl Write,
    codec: &str,
    version: i32,
    object_id: &[u8; 16],
    suffix: &str,
) -> io::Result<()> {
    write_i32_be(w, CODEC_MAGIC)?;
    write_string(w, codec)?;
    write_i32_be(w, version)?;
    w.write_all(object_id)?;
    debug_assert!(suffix.len() <= 255);
    w.write_all(&[suffix.len() as u8])?;
    w.write_all(suffix.as_bytes())?;
    Ok(())
}
```

`object_id`: 16 random bytes, one per segment, generated once at segment-creation time and reused
for every file belonging to that segment (`.si`, `.fdt`, `.fdx`, `.fnm` all carry the *same* ID —
it's how a reader confirms a set of files actually belong together, not stray leftovers from a
half-deleted older segment). Generate with the already-in-tree `uuid` crate's random bytes
(Tutorial 08 pulled in `uuid = { features = ["v4"] }`) rather than adding a `rand` dependency
solely for 16 bytes:

```rust
fn random_segment_id() -> [u8; 16] {
    *uuid::Uuid::new_v4().as_bytes() // ponytail: not cryptographically meaningful here, just
                                      // needs to be unlikely to collide across segments — v4 UUID
                                      // bytes are already exactly the 16 bytes Lucene wants
}
```

`suffix`: empty string (`""`) for every file this tutorial writes — Lucene uses a non-empty
suffix only for per-generation auxiliary files (segment-local doc-values updates, not touched
here).

### 9. `write_footer` — and the corrected magic number

**Correction to the task brief:** the footer magic is not `0x2DF046E`. Checking
`CodecUtil.java` at `releases/lucene/10.1.0` directly:

```java
public static final int CODEC_MAGIC = 0x3fd76c17;
public static final int FOOTER_MAGIC = ~CODEC_MAGIC;
```

`~0x3fd76c17` as a 32-bit two's-complement value is `0xc02893e8` (signed `i32`:
`-1071082520`) — the footer magic is the *bitwise complement* of the header magic, not an
independent constant. Use the derived value, not a separately-typed-in literal, so this can never
drift out of sync if the header magic ever needs revisiting:

```rust
pub const FOOTER_MAGIC: i32 = !CODEC_MAGIC; // Rust's `!` on a signed int is bitwise NOT, same as Java's `~`

pub fn write_footer(w: &mut impl Write, crc: u32) -> io::Result<()> {
    write_i32_be(w, FOOTER_MAGIC)?;
    write_i32_be(w, 0)?; // algorithm ID — always 0 (CRC-32/zlib), verified from CodecUtil.writeFooter
    write_i64_be(w, crc as i64)?; // stored as a signed 64-bit field; CRC-32 always fits, no sign issue
    Ok(())
}
```

`footerLength() == 16` bytes total (4 + 4 + 8) — verified directly from the same source file.
The CRC-32 argument is computed over *every preceding byte in the file*, header through the last
content byte — which means you can't just write footer bytes at the end of a stream you've
already flushed and forgotten; you need the running checksum. That's §15/§16.

### 10. Stored fields write — `.fdt`

Chunk format, write side (mirrors Tutorial 14 §14's read side exactly, since read and write
must agree byte-for-byte on this):

- Per chunk: `docBase` (VInt) + `token` (VInt, `token = (numDocs << 2) | dirtyBit | slicedBit`)
- Then, per document in the chunk: `numStoredFields[i]` and `length[i]` as raw VInts (the
  "packed ints" fast path Tutorial 14 only implemented for `count < 128` — this tutorial's MVP
  never buffers more than 128 docs per chunk, so the fast path is what both sides use; going
  above 128 docs/chunk needs the interleaved bit-transpose block format neither side implements
  — flagged again in §20)
- Then the LZ4-compressed block of raw document bytes

Per-document record (before compression): `infoAndBits` (VLong, `(field_number << 3) | type`) +
value bytes. The MVP writes exactly one field per document — `_source`, field number `0`, type
`1` (`BYTE_ARR`, per `Lucene90CompressingStoredFieldsWriter`'s verified constants) — as raw UTF-8
JSON bytes with a VInt length prefix:

```rust
pub fn write_stored_fields_chunk<W: Write>(
    w: &mut ChecksumWriter<W>,
    doc_base: u32,
    sources: &[Vec<u8>], // one _source JSON blob per doc, already serialized
) -> io::Result<()> {
    const FIELD_NUMBER: u64 = 0;
    const TYPE_BYTE_ARR: u64 = 1;

    write_vint(w, doc_base)?;
    let token = ((sources.len() as u32) << 2) as u32; // not dirty, not sliced (MVP: always one chunk)
    write_vint(w, token)?;

    for _ in sources {
        write_vint(w, 1)?; // numStoredFields per doc: always 1 (only _source)
    }
    for src in sources {
        write_vint(w, src.len() as u32)?; // per-doc byte length, pre-compression
    }

    // Build the uncompressed block: one (infoAndBits VLong, len VInt, bytes) record per doc
    let mut block = Vec::new();
    for src in sources {
        write_vlong(&mut block, (FIELD_NUMBER << 3) | TYPE_BYTE_ARR)?;
        write_vint(&mut block, src.len() as u32)?;
        block.extend_from_slice(src);
    }

    let compressed = lz4_flex::block::compress(&block);
    w.write_all(&compressed)
}
```

**Chunk size:** the task brief's `16384` is not what Lucene's `BEST_SPEED` mode actually uses —
verified from `Lucene90StoredFieldsFormat.java`: `BEST_SPEED_BLOCK_LENGTH = 10 * 8 * 1024 =
81920` bytes, with a separate `maxDocsPerChunk = 1024` cap. The exact threshold barely matters
for *this* tutorial's MVP, though, for a reason worth stating plainly: **the MVP writer always
flushes exactly one chunk per segment**, sized to whatever the ingested batch is. Two
consequences fall out of that single decision:

1. `sliced` is always `false` — slicing only triggers when buffered bytes exceed *twice* the
   configured chunk threshold, which never happens if there's only one chunk to begin with.
2. `LZ4WithPresetDict` (the real `BEST_SPEED_MODE` — verified as a **preset-dictionary** LZ4
   variant, not plain LZ4, where chunk *N+1* uses chunk *N*'s bytes as a dictionary) degrades to
   plain LZ4 for a segment's first (and, here, only) chunk: an empty dictionary. `lz4_flex::
   block::compress` (raw block, **no** prepended length — confirmed by reading `lz4_flex` 0.11's
   own source, the version Tutorial 14 already pinned in `es-codec/Cargo.toml`; `compress_prepend_
   size` writes an extra 4-byte length header Lucene's format has no room for, since the length is
   already carried by the VInt written just before the block) produces exactly the bytes a real
   Lucene reader expects for an empty-dictionary first chunk.

This is a real, load-bearing scope decision, not a convenience shortcut glossed over: a writer
that needs to span multiple chunks per segment (large bulk ingests) needs preset-dictionary LZ4
chaining, which `lz4_flex` supports (`compress_with_dict`/`compress_into_with_dict`) but this
tutorial does not wire up. `// ponytail: single-chunk-per-segment MVP; multi-chunk writes need
LZ4WithPresetDict chaining via lz4_flex::block::compress_with_dict, add when ingest batches
regularly exceed ~1000 docs or ~80KB of _source per segment.`

### 11. `.fdm` and `.fdx` — a second, sharper correction

The task brief (and Tutorial 14's own read-side scoping) describes stored fields as living in
two files, `.fdt`/`.fdx`. **Checking `Lucene90CompressingStoredFieldsWriter.java` directly shows
three files, not two:** `.fdt` (compressed document data — what §10 writes), `.fdm` (a small
metadata stream: codec header, `chunkSize` as a VInt, footer), and `.fdx` (a skip index over
chunk offsets, built through a `FieldsIndexWriter`/`DirectMonotonicWriter` structure — a
block-compressed monotonic-integer index, not a flat array).

Tutorial 14's reader deliberately skipped `.fdx` (linear-scanned chunk headers from the start of
`.fdt` instead — correctness never depended on it, only random-access speed did). **On the write
side that shortcut doesn't fully carry over**: a real Java Elasticsearch's `StoredFieldsReader`
opens and header/footer-checks `.fdm` and `.fdx` when it opens a segment at all, whether or not
it ever needs random access to a specific chunk. A segment missing valid `.fdm`/`.fdx` files (or
listing them in `.si`'s `Files` set without them existing) fails to open, full stop — this is not
an optional-perf-path file the way it was for Tutorial 14's reader.

The MVP writer therefore does write both files, deliberately minimally:

- **`.fdm`**: `write_index_header(w, "Lucene90StoredFieldsFastDataMeta", 1, id, "")` +
  `write_vint(w, chunk_size)` + `write_footer(w, crc)`. That's the entire real format for this
  MVP — the metadata stream genuinely is just the chunk size, nothing else at `VERSION_START`.
- **`.fdx`**: since the MVP always writes exactly one chunk per segment, the "index" degenerates
  to one fact: *chunk 0 starts at byte offset X and covers N docs*. Rather than implementing
  `DirectMonotonicWriter`'s general block-compressed varying-bit-width encoding for a single data
  point, this tutorial writes a **fixed-format single-entry index**: header, VInt doc count,
  VLong start offset, footer. `// ponytail: valid for exactly one chunk per segment (true for
  every segment this MVP writer produces); a real multi-chunk .fdx needs
  org.apache.lucene.util.packed.DirectMonotonicWriter's actual block format — add if/when §10's
  single-chunk-per-segment limitation is lifted, the two changes are coupled.`

This is flagged as loudly as it is because it's the one place this tutorial's `.fdx` is not
simply "a smaller version of the real format" the way `.fdt`'s single-chunk simplification is —
it is a **different, non-standard format** that only a segment written by this exact writer
(never a real Lucene `IndexWriter`) will ever produce. Cross-engine compatibility (§18/§19) works
here only because *reading* is symmetric: our own `es-codec` reader needs updating to understand
this simplified `.fdx` shape too (a small addition to Tutorial 14's reader, not shown in full
here — the round-trip test in §16 exercises it). A real Java Elasticsearch reading this file
needs the **real** `DirectMonotonicWriter`-format `.fdx`, not this simplified one — which is
precisely why this is called out as a limitation in §20 rather than treated as done.

### 12. `.si` — segment info write

Mirrors Tutorial 14's read order exactly (verified again against
`Lucene99SegmentInfoFormat.java`'s `writeSegmentInfo`, which the tutorial's original read-side
notes already matched field-for-field):

```rust
pub fn write_segment_info(
    w: &mut impl Write,
    id: &[u8; 16],
    version: (i32, i32, i32),     // (major, minor, bugfix) — Lucene version that created this segment
    doc_count: i32,
    diagnostics: &BTreeMap<String, String>,
    files: &BTreeSet<String>,
) -> io::Result<()> {
    write_index_header(w, "Lucene90SegmentInfo", 0, id, "")?;
    write_i32_le(w, version.0)?;
    write_i32_le(w, version.1)?;
    write_i32_le(w, version.2)?;
    w.write_all(&[0])?;              // minVersion present flag: 0 = absent (MVP doesn't track it)
    write_i32_le(w, doc_count)?;
    w.write_all(&[0])?;              // isCompoundFile: 0 = false (never write .cfs, see §20)
    w.write_all(&[0])?;              // hasBlocks: 0 = false
    write_map_of_strings(w, diagnostics)?;
    write_set_of_strings(w, files)?;
    write_map_of_strings(w, &BTreeMap::new())?; // attributes: empty
    write_vint(w, 0)?;               // numSortFields: 0 (no index sort)
    // footer written by caller once the whole file's CRC is known — see §15/§16
    Ok(())
}
```

`diagnostics`: a free-form string map real Lucene populates with things like `{"source":
"flush", "lucene.version": "10.1.0", "os": "Linux", ...}` — pure metadata, nothing reads it
back structurally. The MVP writes a minimal but honest version: `{"source": "es-mini-write",
"os": "Linux"}`. `BTreeMap`/`BTreeSet` (not `HashMap`/`HashSet`) — same determinism reasoning
Tutorial 03 gave for `_cat/indices`: byte-identical output across runs matters when the goal is a
reproducible fixture, and iteration order over a `HashMap` is not guaranteed stable.

`files`: the set of every filename this segment owns (`<name>.fdt`, `<name>.fdm`, `<name>.fdx`,
`<name>.fnm`, `<name>.si` itself included by convention) — a real reader cross-checks this set
against what's actually on disk before trusting the segment.

### 13. `segments_N` — the commit point, and its own BE exception

Verified directly against `SegmentInfos.java`'s `write(IndexOutput)` method. This surfaces a
detail Tutorial 14's own notes didn't need to flag because its reader only pulled segment
name/ID/codec out and skipped the rest: **`segments_N` uses `CodecUtil.writeBEInt`/
`writeBELong` for several fields beyond just the header/footer** — `version` (a monotonic commit
counter, distinct from the Lucene version triple), `size()` (segment count), and, per segment,
`delGen`, `delCount`, `fieldInfosGen`, `docValuesGen`, `softDelCount`. This is a real, source-
verified exception to "only header/footer are big-endian" — noted here explicitly so nobody
carries Tutorial 14's simpler rule forward uncritically into new format work.

```rust
pub fn write_segments_file(
    w: &mut impl Write,
    generation: u64,
    counter: u64,
    segments: &[SegmentCommitInfo],
    user_data: &BTreeMap<String, String>,
) -> io::Result<()> {
    let id = random_segment_id();
    write_index_header(w, "segments", 0, &id, &to_base36(generation))?;

    // Lucene version triple that wrote this commit — VInt here (unlike .si's LE i32 triple)
    write_vint(w, 10)?; // major
    write_vint(w, 1)?;  // minor
    write_vint(w, 0)?;  // bugfix

    write_vint(w, 9)?;                 // indexCreatedVersionMajor — this project's pinned ES major
    write_i64_be(w, 0)?;               // "version" commit counter — BE, per source
    write_vlong(w, counter)?;          // the VLong doc/segment-name counter — LE-style VLong, not BE
    write_i32_be(w, segments.len() as i32)?; // segment count — BE, per source

    if !segments.is_empty() {
        // minSegmentVersion triple — always this project's single pinned Lucene version
        write_vint(w, 10)?;
        write_vint(w, 1)?;
        write_vint(w, 0)?;
    }

    for seg in segments {
        write_string(w, &seg.name)?;
        w.write_all(&seg.id)?;
        write_string(w, &seg.codec_name)?;
        write_i64_be(w, seg.del_gen)?;        // BE
        write_i32_be(w, seg.del_count)?;      // BE
        write_i64_be(w, seg.field_infos_gen)?;// BE
        write_i64_be(w, seg.doc_values_gen)?; // BE
        write_i32_be(w, seg.soft_del_count)?; // BE
        w.write_all(&[0])?;                   // per-commit ID present flag: 0 = absent (MVP)
        write_set_of_strings(w, &seg.field_infos_files)?;
        write_i32_be(w, 0)?;                  // dvUpdatesFiles map size — always 0 (MVP)
    }

    write_map_of_strings(w, user_data)?;
    // footer written by caller
    Ok(())
}
```

`user_data`: an arbitrary string map real ES/Lucene use for commit metadata (translog generation,
history UUID) — empty `BTreeMap` is valid and sufficient for this MVP; nothing downstream reads
it.

### 14. Generation numbering — base 36, not hex

**Second correction to the task brief**: `segments_N`'s `N` is not hexadecimal. Verified from
`IndexFileNames.fileNameFromGeneration`: `Long.toString(gen, Character.MAX_RADIX)`, and
`Character.MAX_RADIX == 36` — base 36 (digits `0`–`9` then lowercase `a`–`z`), Java's own
"biggest radix `Long.toString` supports," chosen because it's the most compact ASCII
representation of an increasing integer, not because it maps to anything hex-specific. First
generation is `1` (`generation == 0` is the sentinel for "no commit yet, not a `segments_0` file
on disk" — verified from `IndexFileNames.fileNameFromGeneration`'s `gen == 0` special case,
which returns the *unsuffixed* base name, never taken in practice for `segments_N` since a fresh
index's first real commit always writes generation `1`).

```rust
fn to_base36(n: u64) -> String {
    if n == 0 {
        return String::new();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut n = n;
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(digits[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

/// Scan `dir` for existing `segments_*` files and return the generation to use for the *next*
/// commit — 1 if none exist yet.
pub fn next_generation(dir: &Path) -> io::Result<u64> {
    let mut max_gen = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(suffix) = name.strip_prefix("segments_") {
            if let Ok(gen) = u64::from_str_radix(suffix, 36) {
                max_gen = max_gen.max(gen);
            }
        }
    }
    Ok(max_gen + 1)
}
```

The reader "always opens the highest-`N` file" rule (this is what makes commits crash-safe:
write `segments_2` fully, `rename` it into place, and any reader mid-scan either still sees
`segments_1` — the last fully-committed state — or the complete new `segments_2`; never a
half-written file under either name, because §5's atomic-rename discipline applies here too, on
top of Lucene's own generation scheme). `next_generation` is a directory scan, `u64::from_str_
radix(s, 36)` is the base-36 parse mirror of `to_base36` above — same round-trip discipline as
§3's VInt worked example.

---

## Part 3: Rust concepts this tutorial introduces

### 15. `Cursor<Vec<u8>>` — writing to memory first

Every file this tutorial writes needs a **footer CRC-32 over everything that came before it in
the same file** — which means the checksum can't be computed until the whole file body is known,
but the footer has to be the *last* thing written. Two ways to reconcile that: seek back after
the fact (fiddly, and `.tmp` files created via `tempfile` don't make seeking-back any safer), or
build the file in memory first, compute the checksum over the finished buffer, then write the
buffer plus footer to disk in one shot. This tutorial uses the second approach, and
`std::io::Cursor<Vec<u8>>` is the vehicle:

```rust
use std::io::Cursor;

let mut buf = Cursor::new(Vec::new());
write_index_header(&mut buf, "Lucene90SegmentInfo", 0, &id, "")?;
write_segment_info(&mut buf, &id, version, doc_count, &diagnostics, &files)?;
let crc = crc32fast::hash(buf.get_ref());
write_footer(&mut buf, crc)?;

write_atomic(dir, &format!("{name}.si"), |w| w.write_all(buf.get_ref()))?;
```

`Cursor<Vec<u8>>` implements both `Write` (appends to the inner `Vec`, growing it) and `Read` +
`Seek` (for anything that needs to read back what it just wrote before flushing to disk — not
needed here, but the same type Tutorial 14 could have used to test its reader against an
in-memory buffer instead of a real file). `.get_ref()` borrows the accumulated `&Vec<u8>` without
consuming the cursor; `.into_inner()` consumes it and takes ownership of the `Vec<u8>` — pick
whichever the next step needs.

### 16. CRC-32 with `crc32fast`, and the `ChecksumWriter<W>` newtype

`crc32fast = "1"` (current: **1.5.0**) gives a one-shot `crc32fast::hash(&[u8])` — fine for the
in-memory-buffer approach in §15, where the whole file body already exists as one `&[u8]` by the
time the checksum is needed. But it also has an incremental `Hasher` (`update(&[u8])`,
`finalize() -> u32`), and wrapping that in a type that forwards `Write` calls is a more general,
more reusable pattern worth building explicitly — the **newtype wrapper pattern**:

```rust
pub struct ChecksumWriter<W: Write> {
    inner: W,
    hasher: crc32fast::Hasher,
}

impl<W: Write> ChecksumWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner, hasher: crc32fast::Hasher::new() }
    }

    /// Consumes the wrapper, returning the inner writer and the final CRC-32 of everything
    /// written through this wrapper.
    pub fn finish(self) -> (W, u32) {
        (self.inner, self.hasher.finalize())
    }
}

impl<W: Write> Write for ChecksumWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]); // only checksum the bytes actually written, mirroring
                                        // write()'s own "may write fewer than given" contract
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
```

`ChecksumWriter<W>` is a **struct wrapping an existing type to add behavior without changing its
interface** — the newtype pattern. Every `write_all` call in this tutorial's helpers already
takes `&mut impl Write` and has no idea whether the concrete type underneath is a `File`, a
`Cursor<Vec<u8>>`, or this wrapper — that's the entire point: wrap once at the top, and every
`write_header`/`write_vint`/`write_string` call written earlier in this tutorial works completely
unmodified, transparently accumulating a running checksum as a side effect.

Direct analogue: Python's decorator pattern, or wrapping a file object in a `hashlib`-updating
proxy (`class HashingWriter: def write(self, b): self._h.update(b); return self._f.write(b)`) —
same shape, same motivation (add cross-cutting behavior to something that already implements an
interface, without touching every call site that uses it).

```rust
let mut w = ChecksumWriter::new(Cursor::new(Vec::new()));
write_index_header(&mut w, "Lucene90SegmentInfo", 0, &id, "")?;
write_segment_info(&mut w, &id, version, doc_count, &diagnostics, &files)?;
let (cursor, crc) = w.finish();
let mut buf = cursor.into_inner();
write_footer(&mut buf, crc)?; // footer itself is NOT included in its own checksum — verified from
                              // CodecUtil: checksum covers header+body, footer's magic/algo/crc
                              // fields come after
write_atomic(dir, &format!("{name}.si"), |out| out.write_all(&buf))?;
```

Either approach (§15's after-the-fact `crc32fast::hash` over a finished buffer, or this section's
incremental `ChecksumWriter`) produces the identical CRC — `ChecksumWriter` is shown in full
because it's the more broadly useful Rust idiom (works with unbounded streaming writes, not just
"build the whole thing in memory first"), and every `.fdt` chunk write in `es-codec` uses it.

### 17. Generic write functions and monomorphization

Every helper above is written as `fn write_x<W: Write>(w: &mut W, ...)` or the equivalent
`impl Write` sugar (`fn write_x(w: &mut impl Write, ...)` — identical meaning, different spelling;
this tutorial uses `impl Write` in short signatures and the explicit `<W: Write>` form when a
concrete `W` needs naming elsewhere in the signature, as in `ChecksumWriter<W>` above). The
Rust compiler generates a separate, fully specialized copy of each such function for every
concrete type it's called with (`File`, `BufWriter<File>`, `Cursor<Vec<u8>>`,
`ChecksumWriter<Cursor<Vec<u8>>>`, ...) at compile time — **monomorphization**. No runtime
dispatch, no vtable lookup per call, the same reason Tutorial 11 chose a closed `enum Query`
over `dyn Trait` where the call site was hot and the set of implementations was fixed. Contrast
with `Box<dyn Write>`, which *would* incur a vtable indirection per call — not used anywhere in
this tutorial's write path, since every writer type is known concretely at each call site.

---

## Part 4: Testing write correctness

### 18. The round-trip test — the core DoD for writes

Task 19's own DoD statement is exactly this test: "Rust writes, Rust reads back identical data."
Everything else in this tutorial is in service of making this pass.

```rust
// es-codec/tests/write_roundtrip.rs
use es_codec::{write_si_file, read_si_file, write_stored_fields_file, read_stored_fields_file}; // .si read side (`read_si_file`) wraps Tutorial 14's `read_segment_info`

#[test]
fn si_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let id = [7u8; 16];
    let diagnostics = std::collections::BTreeMap::from([
        ("source".to_string(), "es-mini-write".to_string()),
    ]);
    let files = std::collections::BTreeSet::from(["seg1.fdt".to_string(), "seg1.si".to_string()]);

    es_codec::write_si_file(dir.path(), "seg1", &id, (10, 1, 0), 3, &diagnostics, &files)
        .expect("write .si");

    let read_back = es_codec::read_si_file(dir.path(), "seg1", &id).expect("read .si");

    assert_eq!(read_back.doc_count, 3);
    assert_eq!(read_back.version, (10, 1, 0));
    assert_eq!(read_back.diagnostics, diagnostics);
    assert_eq!(read_back.files, files);
}

#[test]
fn stored_fields_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let id = [9u8; 16];
    let sources = vec![
        br#"{"title":"hello"}"#.to_vec(),
        br#"{"title":"world"}"#.to_vec(),
    ];

    es_codec::write_stored_fields_file(dir.path(), "seg1", &id, &sources).expect("write stored fields");
    let read_back = es_codec::read_stored_fields_file(dir.path(), "seg1", &id, sources.len())
        .expect("read stored fields");

    assert_eq!(read_back, sources);
}

#[test]
fn full_segment_write_then_read_via_segments_file() {
    let dir = tempfile::tempdir().unwrap();
    // ... build a SegmentCommitInfo, write_segments_file, then drive the whole thing through
    // es_codec's segments_N reader (Tutorial 14) end to end — the integration-level version of
    // the two unit tests above, exercising generation numbering (§14) too.
}
```

This is the pattern established across the whole series (Tutorial 05's unit tests, Tutorial 07's
integration tests, Tutorial 13's YAML harness): the write side of a format is only trustworthy
once something automated reads its own output back and asserts equality. A hand-inspected hex
dump that "looks right" is not a substitute — get a machine to check it, every time this repo's
own `cargo test` runs.

### 19. Cross-engine verification: `cargo xtask verify-writes`

Round-trip against *our own* reader (§18) proves internal consistency. It does not prove a real
Java Elasticsearch can open what Rust wrote — that needs the actual other engine in the loop,
same reasoning Tutorial 14 used in the other direction (reading) with its `gen-fixtures`
xtask. New task, same `xtask` crate (Tutorial 13 built the dispatcher, Tutorial 14 added
`gen-fixtures` alongside it — this is a third `match` arm, not a new mechanism):

```rust
// xtask/src/verify_writes.rs
pub fn run() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;

    // 1. Boot the Rust server pointed at a fresh data directory.
    let mut server = spawn_es_mini(data_dir.path())?; // returns a Drop-guarded child (Tutorial 13 pattern)
    wait_for_healthy("http://127.0.0.1:9200")?;

    // 2. Create an index, ingest N documents through the real HTTP API — plain PUT/GET calls,
    //    not Tutorial 13's `ApiClient` (that type is async and shaped around YAML action
    //    dispatch; this xtask is sync end-to-end like `gen-fixtures`, so it talks to the server
    //    directly with `reqwest::blocking`, same as Tutorial 14's `wait_for_healthy`).
    let client = reqwest::blocking::Client::new();
    client.put("http://127.0.0.1:9200/verify-writes").send()?;
    for i in 0..20 {
        client
            .put(format!("http://127.0.0.1:9200/verify-writes/_doc/{i}"))
            .json(&serde_json::json!({"n": i, "text": format!("document {i}")}))
            .send()?;
    }

    // 3. Stop the Rust server cleanly — Drop on `server` does this; make it explicit here anyway
    //    since the very next step needs the data directory fully flushed and released first.
    drop(server);

    // 4. Boot the pinned Java ES image on the SAME data directory.
    let mut java_es = spawn_java_es_on(data_dir.path())?; // `docker run -v <data_dir>:/usr/share/elasticsearch/data ...`
    wait_for_healthy("http://127.0.0.1:9201")?;

    // 5. Query the Java instance, assert the Rust-written documents are found.
    let resp: serde_json::Value = client
        .get("http://127.0.0.1:9201/verify-writes/_doc/7")
        .send()?
        .json()?;
    anyhow::ensure!(resp["found"] == true, "Java ES did not find doc 7 written by Rust");
    anyhow::ensure!(resp["_source"]["n"] == 7);

    // 6. `docker stop` happens via java_es's own Drop guard.
    println!("verify-writes: PASS");
    Ok(())
}
```

Same `Drop`-guard discipline Tutorial 13 established for `ServerGuard` applies to both the Rust
server child process and the Docker container handle here — two guards, two independent
teardowns, neither relying on the other, both unconditional even if an assertion above panics
mid-test. Report is deliberately blunt: pass/fail printed line, non-zero exit on any `ensure!`
failure — this is the "ultimate correctness test" for the whole write path (Task 20's DoD,
verbatim), it doesn't need a fancier report format than that.

---

## Part 5: Known limitations

Every one of these has a `// ponytail:` comment at its call site in `es-codec`, not just here:

1. **No postings write (`.tim`/`.tip`)** — a segment written by this tutorial has no inverted
   index on disk at all. Search continues to run against the in-memory `InvertedIndex`
   regardless of storage backend (§7). Add when `_search`/`_count` need to work against a
   disk-only backend with the in-memory store absent.
2. **No doc values write (`.dvd`/`.dvm`)** — no fast sort/aggregation path from disk. Sort still
   runs against in-memory `Document` values.
3. **No points/BKD tree write** — numeric `range` queries against a disk-only backend would need
   a full doc-count scan (mirrors Tutorial 11's own in-memory `Range` query cost, §"Known
   performance limitations" there — not a new gap, just not yet paid down on the disk side
   either).
4. **No compound file write (`.cfs`/`.cfe`)** — every segment writes as loose per-extension
   files, matching the fixture-generation workaround Tutorial 14 already used on the read side
   (`no_cfs_ratio: 0`) rather than introducing an asymmetry between what this writer produces and
   what Tutorial 14's reader (and its own round-trip test, §18) expects.
5. **Single chunk per segment (§10)** — no `LZ4WithPresetDict` chaining across chunks. Add when
   ingest batches regularly exceed ~1000 docs or ~80KB of serialized `_source` per segment.
6. **Simplified, non-standard `.fdx` (§11)** — valid only for the single-chunk case this writer
   always produces; not the real `DirectMonotonicWriter` block format. A real Java Elasticsearch
   reading a segment this tutorial wrote needs the genuine format here specifically — this is the
   sharpest of all the limitations in this list, see §11 for the full explanation.
7. **No merges** — every flush produces a new segment; segments are never combined. A long-running
   index accumulates one segment per commit indefinitely. Real Lucene's `MergePolicy` is a
   substantial subsystem on its own; out of scope entirely for this MVP.
8. **No soft deletes / no update-in-place on disk** — `DELETE`/`_update` against a disk-backed
   index would need a new segment (or a liveDocs bitset) layered on top of what's here; not wired
   up.
9. **No index-time field-schema enforcement written to `.fnm`** — the MVP `.fnm` write is
   name/number only, mirroring exactly what Tutorial 14's reader retains and nothing more (no
   `IndexOptions`/`DocValuesType` beyond placeholder zero values).
10. **`verify-writes` covers one query shape** — `GET /{index}/_doc/{id}` by ID. It does not
    attempt a Java-side `_search` against Rust-written segments, since that would require the
    postings write this tutorial explicitly defers (#1).

---

## Definition of Done

1. `write_vint`/`write_vlong`/`write_string` exist in `es-codec`, and a unit test round-trips
   each against Tutorial 14's `read_vint`/`read_vlong`/`read_string` for at least the `300`
   worked example (§3) plus a handful of edge values (`0`, `127`, `128`, `u32::MAX`).
2. `ChecksumWriter<W>` is implemented, its `write`/`flush` forward correctly to an inner writer,
   and a unit test confirms `ChecksumWriter::finish().1` matches `crc32fast::hash` computed
   independently over the same bytes.
3. `write_atomic` (or the `NamedTempFile`-based version) is used for every file write in
   `es-codec` — no direct `File::create` on a final filename anywhere in the write path. Grep
   confirms this.
4. `.si` files written by `es-codec` are read back correctly by Tutorial 14's `read_segment_info`
   with all fields (`doc_count`, `version`, `diagnostics`, `files`) equal to what was written.
5. `.fdt`/`.fdm`/`.fdx` files written by `es-codec` are read back correctly by the (updated)
   Tutorial 14 reader, recovering the exact `_source` bytes for every document in the segment.
6. `segments_N` is written with the correct base-36 generation suffix (§14), and
   `next_generation(dir)` correctly returns `1` for an empty directory and `max + 1` after
   existing `segments_*` files.
7. `cargo test -p es-codec` passes, including the round-trip tests from §18, in CI.
8. `cargo xtask verify-writes` exists, runs the full boot → ingest → stop → Java-ES-boot → query
   → assert → teardown sequence (§19), and passes locally against the pinned ES/Lucene version
   (`COMPATIBILITY.md`).
9. `cargo xtask verify-writes` is wired into CI as a gated (slower) job, per `EXECUTION.md` Task
   20/21's "runs in CI (may be a slower/gated job)" requirement — same precedent as Tutorial 14's
   YAML harness job.
10. Every limitation in Part 5 has a corresponding `// ponytail:` comment at its call site in
    `es-codec` — not just prose in this tutorial. `grep -rn "ponytail:" es-codec/src` surfaces all
    of them.
11. `COMPATIBILITY.md` is updated to state plainly, in one sentence, what "write compatible"
    means for this project: single-segment-per-commit, no merges, no compound files, stored
    fields + segment metadata only (no postings/doc values/points on disk).
12. `EXECUTION.md` Tasks 19–20 are marked satisfied against this tutorial's DoD; Task 21 (the
    Ralph loop driving remaining write-compatibility gaps to green) is explicitly scoped as
    future work, not attempted by this tutorial — the loop needs a red/green signal to iterate
    against, and `verify-writes`'s single pass/fail assertion (§19) is exactly that signal, ready
    to hand to a Ralph loop as-is.

---

## Where to go from here

This tutorial — and the series — stops at a genuinely useful, honestly-scoped MVP: a Rust
Elasticsearch clone that speaks the real HTTP contract, passes real YAML compatibility tests,
and can read and write real Lucene segments well enough for a document-ID-level round trip
between two independent engines. Real Elasticsearch is a vastly larger system than what's built
here; the gaps in Part 5 are the honest map of the distance remaining, not a hidden one:

- **Postings write** (`.tim`/`.tip`) — the single biggest remaining gap; unlocks disk-backed
  `_search`/`_count` and is the natural next tutorial-sized project, following the same
  "read first (already partially scoped out in Tutorial 14), then write, then round-trip test"
  arc this tutorial and its predecessor already established.
- **Doc values** (`.dvd`/`.dvm`) — needed for fast sort/aggregations without loading `_source`.
- **Compound files** (`.cfs`/`.cfe`) — needed once merges exist and small segments should be
  packed into one file handle instead of many.
- **BKD trees** — needed for efficient numeric/geo range queries against disk-only data;
  currently a full scan on both the in-memory and (nonexistent) disk-backed path alike.
- **Merges** — the subsystem that turns "one segment per commit forever" into a bounded,
  amortized-cost segment count; genuinely one of the largest and most performance-sensitive parts
  of real Lucene, deliberately last on this list.
- **Performance profiling** — nothing in this series has been benchmarked under load; `cargo
  flamegraph`/`criterion` against the YAML/verify-writes harnesses already in place would be the
  natural entry point, once there's a write-heavy or search-heavy path worth profiling in the
  first place.
