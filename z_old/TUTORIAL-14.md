# Tutorial 14 — Reading Binary Formats: Lucene 10 Codec

**Phase 4, Tasks 15–17.** You've built a full HTTP-compatible ES clone on an in-memory store,
with a YAML test harness (Tutorial 13) watching it. This tutorial does something different: it
reads real bytes written by a real Java Elasticsearch, and turns them back into JSON.

Every fact about the Lucene 10 on-disk format in this tutorial was checked against the actual
Lucene source at tag `releases/lucene/10.1.0` (the version whose `lucene_version` your `GET /`
response already reports, per Tutorial 04). Where the format is more involved than a single
tutorial can fully implement, that's called out explicitly with a `// ponytail:` comment and a
named upgrade path — this is Task 16/18's job (`EXECUTION.md`), not this one's.

**Prerequisite mindset from Tutorial 12:** "borrow while parsing, own at the parse boundary."
Reading Lucene bytes is the real version of that lesson — every helper here reads through a
`&mut impl Read` and returns *owned* values (`String`, `Vec<u8>`, `u32`), never a borrowed slice
of the file. There's no lifetime parameter anywhere in this tutorial's code.

---

## Part 1: Binary I/O in Rust

### 1. Opening and reading files

Python: `data = open(path, 'rb').read()` — one call, whole file, done. Rust makes you be
explicit about how many bytes you want and where they go, because it's tracking exactly which
memory is initialized:

```rust
use std::fs::File;
use std::io::{self, Read};

fn read_whole_file(path: &std::path::Path) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}
```

`File::open` returns `io::Result<File>` — a file that doesn't exist is a normal `Err`, not a
panic, not an exception you forgot to catch. The `?` propagates it (Tutorial 05).

The `Read` trait (implemented by `File`, `&[u8]`, `TcpStream`, and — relevantly — `BufReader<R>`)
gives you:

- `read(&mut self, buf: &mut [u8]) -> io::Result<usize>` — reads *up to* `buf.len()` bytes,
  returns how many it actually got. Can return less than you asked for (short reads are normal,
  not an error — same as a Python socket `.recv()`).
- `read_exact(&mut self, buf: &mut [u8]) -> io::Result<()>` — reads *exactly* `buf.len()` bytes or
  fails with `UnexpectedEof`. This is the one a binary-format reader wants everywhere: Lucene
  files have fixed-size fields, and a short read there means a corrupt or truncated file, not
  something to silently paper over.

```rust
let mut four_bytes = [0u8; 4];
reader.read_exact(&mut four_bytes)?;
```

### 2. `BufReader` — don't read a file one syscall at a time

`File` itself does one `read()` syscall per call. A segment reader that reads a 4-byte int, then
a string, then another 4-byte int, in a loop, would issue dozens of tiny syscalls per document.
`std::io::BufReader<R>` wraps any `Read` and reads in large chunks (default 8 KB) internally,
serving small reads out of that buffer:

```rust
use std::io::BufReader;

let file = File::open(path)?;
let mut reader = BufReader::new(file);
```

Direct analogue: Python's `io.BufferedReader` (which is what `open(path, 'rb')` already gives you
by default — Rust's `File` does *not* buffer by default, so the wrap is not optional the way it
is in Python). Rule of thumb used for the rest of this tutorial: any `File` opened for sequential
reads gets wrapped in a `BufReader` immediately, no exceptions.

### 3. `Seek` and `SeekFrom` — jumping around a file

Lucene readers are full of random access: "seek to the footer to check the checksum," "seek to
the byte offset this index says the target chunk starts at." `std::io::Seek`:

```rust
use std::io::{Seek, SeekFrom};

reader.seek(SeekFrom::Start(offset))?;       // absolute, from byte 0
reader.seek(SeekFrom::Current(delta))?;      // relative to current position
reader.seek(SeekFrom::End(-16))?;            // 16 bytes before EOF — this is exactly
                                              // how you find a Lucene footer (see §11)
```

`BufReader<File>` implements `Seek` too (it discards its internal buffer and reseeks the
underlying file) — no need to unwrap back to the raw `File` to seek.

### 4. Reading multi-byte integers — and a correction

The task brief for this tutorial assumed Lucene encodes multi-byte integers big-endian
throughout. **Checking the actual source shows that's only half true**, and the split matters:

| What | Encoding | Source |
|---|---|---|
| Codec header magic, version; footer magic, algorithm ID, checksum | **Big-endian**, via `CodecUtil.writeBEInt`/`readBEInt` | `CodecUtil.java` |
| Everything else — `docCount`, `.si` version triples, `.fnm` `dvGen`, chunk lengths, etc. | **Little-endian**, via the ordinary `DataOutput.writeInt`/`readInt` | `DataOutput.java`/`DataInput.java` |

```java
// DataOutput.java — the *general* int writer, used almost everywhere:
public void writeInt(int i) throws IOException {
    writeByte((byte) i);
    writeByte((byte) (i >> 8));
    writeByte((byte) (i >> 16));
    writeByte((byte) (i >> 24));
}
```
That's least-significant-byte-first — little-endian. But `CodecUtil` has its own private
`writeBEInt`/`readBEInt` used *only* for the magic number, header version, and footer, explicitly
so the magic bytes look the same regardless of which endianness variant a future format revision
might pick for its body. Two encodings, two purposes, one file — worth getting right, since a
big-endian assumption on `docCount` would silently misread every `.si` file.

```rust
pub(crate) fn read_i32_be(r: &mut impl Read) -> io::Result<i32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(i32::from_be_bytes(buf))
}

pub(crate) fn read_i32_le(r: &mut impl Read) -> io::Result<i32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(i32::from_le_bytes(buf))
}

pub(crate) fn read_i64_le(r: &mut impl Read) -> io::Result<i64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(i64::from_le_bytes(buf))
}
```

`i32::from_be_bytes([b0, b1, b2, b3])` / `from_le_bytes` are inherent associated functions on
every integer primitive — no crate needed, this is std. `[u8; 4]` is a fixed-size array (its
length is part of the type, checked at compile time — `read_exact` requires exactly that many
bytes).

### 5. VInt — Lucene's variable-length integer

Small integers (a doc count, a field number, a string length) don't need 4 bytes every time.
Lucene's VInt packs 7 bits of value per byte, using the 8th (high) bit as "more bytes follow":

```java
// DataOutput.java
public final void writeVInt(int i) throws IOException {
  while ((i & ~0x7F) != 0) {
    writeByte((byte) ((i & 0x7F) | 0x80));
    i >>>= 7;
  }
  writeByte((byte) i);
}
```

Worked example — encoding `300` (`0b1_0010_1100`):

```
300 = 0b100101100
low 7 bits:  0101100 (0x2C), remaining: 0b10 (2)
byte 0: 0x2C | 0x80 = 0xAC   (continuation bit set — more bytes coming)
byte 1: 0x02               (continuation bit clear — this is the last byte)

decode:
  b0 = 0xAC = 1010_1100 -> high bit set, value bits = 010_1100 = 0x2C = 44
    result = 44, shift = 7
  b1 = 0x02 = 0000_0010 -> high bit clear, value bits = 2
    result = 44 | (2 << 7) = 44 + 256 = 300, stop
```

```rust
pub(crate) fn read_vint(r: &mut impl Read) -> io::Result<u32> {
    let mut result: u32 = 0;
    let mut shift = 0;
    loop {
        let mut byte = [0u8; 1];
        r.read_exact(&mut byte)?;
        let b = byte[0];
        result |= u32::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift > 28 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "vint too long"));
        }
    }
}
```

Max 5 bytes for a `u32` (5 × 7 = 35 bits of capacity, enough to cover all 32). The `shift > 28`
guard turns a malformed/corrupt stream (an endless run of high-bit-set bytes) into a clean error
instead of an infinite loop — same defensive instinct as Tutorial 09's bulk-parsing cursor loop.

### 6. VLong — the 64-bit sibling

Identical algorithm, `u64` accumulator, up to 10 bytes of capacity (Lucene's Java `writeVLong`
actually asserts the value is non-negative, capping real usage at 63 bits / ~9 bytes, but the
*decoder* doesn't need to assume that — read until the continuation bit clears):

```rust
pub(crate) fn read_vlong(r: &mut impl Read) -> io::Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let mut byte = [0u8; 1];
        r.read_exact(&mut byte)?;
        let b = byte[0];
        result |= u64::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift > 63 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "vlong too long"));
        }
    }
}
```

Same body, different width — a generic `read_vword<T>` isn't worth building for two call sites;
duplication this small is cheaper to read than an abstraction over it.

### 7. Length-prefixed strings

Lucene's `writeString`/`readString`: VInt byte-length (not char count — matters for non-ASCII),
then that many raw UTF-8 bytes:

```java
// DataInput.java
public String readString() throws IOException {
    int length = readVInt();
    final byte[] bytes = new byte[length];
    readBytes(bytes, 0, length);
    return new String(bytes, 0, length, StandardCharsets.UTF_8);
}
```

```rust
pub(crate) fn read_string(r: &mut impl Read) -> io::Result<String> {
    let len = read_vint(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
```

`String::from_utf8(Vec<u8>) -> Result<String, FromUtf8Error>` — validates the bytes are actually
UTF-8 (Lucene's own `writeString` guarantees this on the write side, but a reader shouldn't trust
that on principle; corrupt/foreign input is exactly what `read_exact`-everywhere discipline is
for). `map_err` converts the crate-specific error into `io::Error` so the function signature
stays a plain `io::Result<String>` — same "adapt one error type into the ambient one" move as
`?`'s `From` conversion (Tutorial 05), done manually here since `FromUtf8Error` isn't `io::Error`.

`Vec<String>`/`HashMap<String, String>` readers (`readSetOfStrings`/`readMapOfStrings` — VInt
count, then that many `readString()`/`readString()` pairs) are a direct, mechanical extension of
`read_string` and appear in the `.si`/`.fnm` readers below without further explanation.

---

## Part 2: The `es-codec` crate architecture

### 8. Crate structure

```
es-codec/
  src/
    lib.rs              // pub mod declarations, shared low-level io helpers
    io.rs               // read_vint, read_vlong, read_string, check_header, read_i32_le, ...
    segment_infos.rs     // segments_N -> SegmentInfos { segments: Vec<SegmentInfo> }
    segment_info.rs       // *.si -> SegmentInfo
    field_infos.rs        // *.fnm -> FieldInfos (name <-> field number)
    stored_fields.rs      // *.fdt/*.fdx -> read_stored_fields()
```

`es-codec/src/lib.rs`:

```rust
mod io;
mod segment_infos;
mod segment_info;
mod field_infos;
mod stored_fields;

pub use io::{check_header, read_string, read_vint, read_vlong, CODEC_MAGIC};
pub use segment_infos::{SegmentInfos, SegmentCommit};
pub use segment_info::SegmentInfo;
pub use field_infos::FieldInfos;
pub use stored_fields::read_stored_fields;
```

Each module maps to one Lucene file type — the same "one file, one concern" shape `es-index`'s
`error.rs`/`analysis/mod.rs` already established.

### 9. Lucene file naming, and what this tutorial actually implements

A Lucene index directory (this is what's inside an ES shard's `index/` folder) is a flat directory
of files per segment, plus a commit-point file:

| File | Contents |
|---|---|
| `segments_N` | Commit point — which segments make up the current index. No extension; `N` is a hex generation counter (`segments_3` is generation 3, not "version 3"). |
| `<seg>.si` | Per-segment metadata: Lucene version, doc count, compound-file flag, file list. |
| `<seg>.fnm` | Field infos — field name ↔ field number mapping, plus per-field flags (indexed, doc-values type, etc). |
| `<seg>.fdx` / `<seg>.fdt` | Stored fields index / data — this is where `_source` lives. |
| `<seg>.tim` / `<seg>.tip` | Terms dictionary postings / index — inverted index for search. |
| `<seg>.dvd` / `<seg>.dvm` | Doc values data / metadata — columnar per-field values (aggregations, sorting). |
| `<seg>.cfs` / `<seg>.cfe` | Compound file — see below. |

**This tutorial implements:** `segments_N`, `.si`, `.fnm` (just enough for name→number lookup),
and `.fdt`/`.fdx` (just enough to extract `_source`). `.tim`/`.tip`/`.dvd`/`.dvm` — the actual
search/aggregation path — are `EXECUTION.md` Task 16's remaining scope, picked up by the Ralph
loop (Task 18) one file format at a time, exactly as Task 16 specifies ("build incrementally, one
file format at a time, each with its own decode tests").

**Compound files:** checked against `MergePolicy.java` — the default `noCFSRatio` is `1.0`, which
means Lucene *does* write small/newly-flushed segments as a single `.cfs`/`.cfe` compound file by
default (a `.cfs`/`.cfe` pair bundles all of a segment's individual files into one, to reduce file
handle pressure — it's still the same byte layout per sub-file, just concatenated with an offset
table). `// ponytail:` this tutorial's reader only opens raw per-segment files directly and does
**not** transparently unpack `.cfs` — real fixtures from a freshly created test index will very
likely be compound files and this reader will fail to find `<seg>.si` on disk as a standalone
file. Fixture generation (Part 3) works around this by force-merging and forcing a non-compound
segment (`index.merge.policy.no_cfs_ratio: 0.0` at index-creation time) so the MVP fixture has
plain per-segment files. Transparent CFS unpacking is a natural Task 18 item — the `.cfs` format
is itself just a concatenation of the same headers this tutorial already parses, with a small
lookup table prefix.

### 10. `SegmentInfo` — reading a `.si` file

Verified directly against `Lucene99SegmentInfoFormat.java` (the format Lucene 10's default codec,
`Lucene101Codec`, delegates to for segment info — codec name in this file's header is literally
`"Lucene90SegmentInfo"`, a naming leftover from when the format was introduced that never changed
because the bytes didn't need to):

```
.si file:
  IndexHeader                         (§11)
  SegVersion       -> 3x LE Int32      (major, minor, bugfix)
  HasMinVersion    -> 1 byte (0 or 1)
  [MinVersion      -> 3x LE Int32]     (only if HasMinVersion == 1)
  DocCount         -> LE Int32
  IsCompoundFile   -> 1 byte
  HasBlocks        -> 1 byte
  Diagnostics      -> Map<String,String>
  Files            -> Set<String>
  Attributes       -> Map<String,String>
  NumSortFields    -> VInt (0 in every fixture this tutorial's tests use — index sort is
                       a Phase 5+ concern)
  Footer                                (§11)
```

```rust
// es-codec/src/segment_info.rs
use crate::io::{check_index_header, read_i32_le, read_map_of_strings, read_set_of_strings, read_string, read_vint};
use std::collections::{HashMap, HashSet};
use std::io::{self, Read};

#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub name: String,
    pub version: (i32, i32, i32),
    pub doc_count: i32,
    pub is_compound_file: bool,
    pub files: HashSet<String>,
    pub diagnostics: HashMap<String, String>,
}

const CODEC_NAME: &str = "Lucene90SegmentInfo";
const VERSION_START: i32 = 0;

pub fn read_segment_info(r: &mut impl Read, segment_id: &[u8; 16], name: &str) -> io::Result<SegmentInfo> {
    check_index_header(r, CODEC_NAME, VERSION_START, VERSION_START, segment_id, "")?;

    let major = read_i32_le(r)?;
    let minor = read_i32_le(r)?;
    let bugfix = read_i32_le(r)?;

    let mut has_min = [0u8; 1];
    r.read_exact(&mut has_min)?;
    if has_min[0] == 1 {
        let _min_version = (read_i32_le(r)?, read_i32_le(r)?, read_i32_le(r)?);
    }

    let doc_count = read_i32_le(r)?;

    let mut flags = [0u8; 2]; // is_compound_file, has_blocks
    r.read_exact(&mut flags)?;
    let is_compound_file = flags[0] == 1;

    let diagnostics = read_map_of_strings(r)?;
    let files = read_set_of_strings(r)?;
    let _attributes = read_map_of_strings(r)?;

    let num_sort_fields = read_vint(r)?;
    if num_sort_fields != 0 {
        // ponytail: index sort deserialization not implemented — out of MVP scope,
        // add SortField parsing here if a fixture with an index sort shows up.
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "indexed sort segments not supported by this MVP reader",
        ));
    }

    // Footer (checksum) intentionally not verified here — see check_header note in §11.
    Ok(SegmentInfo {
        name: name.to_string(),
        version: (major, minor, bugfix),
        doc_count,
        is_compound_file,
        files,
        diagnostics,
    })
}
```

`(i32, i32, i32)` as the version type: a tuple, not a struct with three named fields — this is a
positional triple used exactly once as a comparable/printable value, not a domain type with its
own behavior, so a struct would be a name for nothing.

### 11. `check_header` — the thing every Lucene file starts with

Verified against `CodecUtil.java`. Every Lucene file opens with:

```
CodecHeader --> Magic, CodecName, Version
  Magic     -> BE Int32, always 0x3fd76c17
  CodecName -> String (VInt length + UTF-8 bytes)
  Version   -> BE Int32
```

And an *index* header (used by `.si`, stored fields, etc. — anything identified with a segment)
adds an object ID and suffix after that:

```
IndexHeader --> CodecHeader, ObjectID, ObjectSuffix
  ObjectID     -> 16 raw bytes
  ObjectSuffix -> 1 byte length + that many ASCII bytes
```

And every file *ends* with a footer:

```
CodecFooter --> Magic, AlgorithmID, Checksum
  Magic       -> BE Int32, always ~CODEC_MAGIC (bitwise NOT of the header magic)
  AlgorithmID -> BE Int32, always 0 (zlib CRC-32)
  Checksum    -> BE Int64
```

```rust
// es-codec/src/io.rs
use std::io::{self, Read};

pub const CODEC_MAGIC: i32 = 0x3fd76c17;

pub fn check_header(
    r: &mut impl Read,
    expected_codec: &str,
    min_version: i32,
    max_version: i32,
) -> io::Result<i32> {
    let magic = read_i32_be(r)?;
    if magic != CODEC_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("codec header mismatch: got magic {magic:#x}, expected {CODEC_MAGIC:#x}"),
        ));
    }

    let actual_codec = read_string(r)?;
    if actual_codec != expected_codec {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("codec mismatch: got '{actual_codec}', expected '{expected_codec}'"),
        ));
    }

    let version = read_i32_be(r)?;
    if version < min_version || version > max_version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported codec version {version} for '{expected_codec}' \
                 (supported: {min_version}..={max_version})"
            ),
        ));
    }
    Ok(version)
}

pub fn check_index_header(
    r: &mut impl Read,
    expected_codec: &str,
    min_version: i32,
    max_version: i32,
    expected_id: &[u8; 16],
    expected_suffix: &str,
) -> io::Result<i32> {
    let version = check_header(r, expected_codec, min_version, max_version)?;

    let mut id = [0u8; 16];
    r.read_exact(&mut id)?;
    if &id != expected_id {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "segment ID mismatch"));
    }

    let mut suffix_len = [0u8; 1];
    r.read_exact(&mut suffix_len)?;
    let mut suffix_bytes = vec![0u8; suffix_len[0] as usize];
    r.read_exact(&mut suffix_bytes)?;
    let suffix = String::from_utf8(suffix_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if suffix != expected_suffix {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "segment suffix mismatch"));
    }

    Ok(version)
}
```

`impl Read` in argument position (`r: &mut impl Read`) is the same argument-position `impl
Trait` sugar Tutorial 09 introduced for `reason: impl Into<String>` — except here every call
site actually benefits from it (`BufReader<File>` in production, `&[u8]` in unit tests), unlike
Tutorial 09's single-call-site example, which flagged the sugar as "recognize on sight, don't
reach for reflexively." `check_header` works identically for either reader type, with zero
runtime dispatch cost: each call site monomorphizes to a concrete function specialized for its
actual reader type — this is the "zero-cost abstraction" the Rust docs advertise, not marketing.

`// ponytail:` this tutorial's `read_segment_info`/`read_stored_fields` never call
`CodecUtil.checkFooter`'s CRC-32 verification — the footer's *position* (last 16 bytes) and shape
are trusted implicitly rather than checksummed. Real Lucene checksums every file read to catch
disk-level corruption; add a CRC-32 pass over the whole file (`crc32fast` crate, or std's none —
CRC isn't in std) the day a fixture actually exercises corruption detection.

---

### 12. `segments_N` — the commit point

Verified against `SegmentInfos.java`. The full commit-point format carries much more than this
MVP needs (deletion generations, per-segment field-infos/doc-values generations, global commit
user data) — this reader extracts only what's needed to enumerate segments and open their `.si`
files:

```
segments_N file:
  IndexHeader (codec name "segments")
  Lucene version   -> 3x VInt (major, minor, bugfix)   [note: VInt here, not the plain
                       LE Int32 the .si file's version triple uses — different writer,
                       different choice, both verified against source]
  IndexCreatedVersionMajor -> VInt
  Counter          -> VLong
  NumSegments      -> VInt
  for each segment:
    SegName  -> String
    SegID    -> 16 raw bytes
    SegCodec -> String
    ... (del-gen, field-infos gen, doc-values gen, soft-deletes-field, sort-field-count —
         skipped by this MVP reader; not needed to locate and open a .si file)
  CommitUserData -> Map<String,String>
  Footer
```

```rust
// es-codec/src/segment_infos.rs
use crate::io::{check_header, read_string, read_vint, read_vlong};
use std::io::{self, Read};

#[derive(Debug)]
pub struct SegmentCommit {
    pub name: String,
    pub id: [u8; 16],
}

#[derive(Debug)]
pub struct SegmentInfos {
    pub segments: Vec<SegmentCommit>,
}

const CODEC_NAME: &str = "segments";
const VERSION_74: i32 = 9; // oldest version this MVP accepts, matches Lucene 10's own floor
const VERSION_CURRENT: i32 = 10;

pub fn read_segments_file(r: &mut impl Read) -> io::Result<SegmentInfos> {
    check_header(r, CODEC_NAME, VERSION_74, VERSION_CURRENT)?;

    let _lucene_version = (read_vint(r)?, read_vint(r)?, read_vint(r)?);
    let _index_created_version_major = read_vint(r)?;
    let _counter = read_vlong(r)?;

    let num_segments = read_vint(r)?;
    let mut segments = Vec::with_capacity(num_segments as usize);
    for _ in 0..num_segments {
        let name = read_string(r)?;
        let mut id = [0u8; 16];
        r.read_exact(&mut id)?;
        let _codec = read_string(r)?;
        // ponytail: per-segment del-gen/field-infos-gen/doc-values-gen/soft-deletes-field/
        // sort-fields skipped — this reader has no delete/update-in-place support yet, so
        // "which generation of deletes applies" is meaningless for it. Parse these when a
        // fixture with a deleted document needs to be read correctly.
        segments.push(SegmentCommit { name, id });
    }

    Ok(SegmentInfos { segments })
}
```

`// ponytail:` skipping the per-segment generation fields means this reader can open a
`segments_N` file and find `<name>.si` for every live segment, but can't yet tell whether any
documents in that segment have been soft/hard-deleted since the segment was written. Every
fixture in Part 3 is generated without deletes, so this doesn't bite yet — flagged so it isn't
mistaken for an oversight instead of a scoped decision.

---

### 13. `.fnm` — field name ↔ number, just enough

Verified against `Lucene94FieldInfosFormat.java`. The real format also carries index options, doc
values type, point/vector dimension metadata — none of which this tutorial's `_source`-only read
path needs. The reader below walks past every field's record correctly (so the byte cursor stays
aligned for the *next* field) but only retains `name` and `number`:

```rust
// es-codec/src/field_infos.rs
use crate::io::{check_index_header, read_i64_le, read_map_of_strings, read_string, read_vint};
use std::collections::HashMap;
use std::io::{self, Read};

#[derive(Debug)]
pub struct FieldInfos {
    number_by_name: HashMap<String, i32>,
}

impl FieldInfos {
    pub fn number_of(&self, name: &str) -> Option<i32> {
        self.number_by_name.get(name).copied()
    }
}

const CODEC_NAME: &str = "Lucene94FieldInfos";
const VERSION_START: i32 = 0;

pub fn read_field_infos(r: &mut impl Read, segment_id: &[u8; 16]) -> io::Result<FieldInfos> {
    check_index_header(r, CODEC_NAME, VERSION_START, VERSION_START, segment_id, "")?;

    let size = read_vint(r)?;
    let mut number_by_name = HashMap::with_capacity(size as usize);

    for _ in 0..size {
        let name = read_string(r)?;
        let number = read_vint(r)? as i32; // field number: VInt
        let mut bits = [0u8; 1];
        r.read_exact(&mut bits)?; // storeTermVector/omitNorms/storePayloads/... flags
        let mut index_options = [0u8; 1];
        r.read_exact(&mut index_options)?;
        let mut doc_values_type = [0u8; 1];
        r.read_exact(&mut doc_values_type)?;
        let _dv_gen = read_i64_le(r)?;
        let _attributes = read_map_of_strings(r)?;

        let point_dimension_count = read_vint(r)?;
        if point_dimension_count != 0 {
            let _point_index_dimension_count = read_vint(r)?;
            let _point_num_bytes = read_vint(r)?;
        }
        let _vector_dimension = read_vint(r)?;
        let mut vector_encoding = [0u8; 1];
        r.read_exact(&mut vector_encoding)?;
        let mut vector_similarity = [0u8; 1];
        r.read_exact(&mut vector_similarity)?;

        // silence "unused" for fields this MVP intentionally discards
        let _ = (bits, index_options, doc_values_type, vector_encoding, vector_similarity);

        number_by_name.insert(name, number);
    }

    Ok(FieldInfos { number_by_name })
}
```

`// ponytail:` this reader assumes the exact field-record shape of `FORMAT_DOCVALUE_SKIPPER`
(the current Lucene 10.1 format revision — no doc-values-skipper byte older formats lack, no
parent-field bit newer formats might add). A version-gated read (`if format >= FORMAT_X { ... }`,
mirroring the real Java reader) is the correct long-term shape; skipped here because every
fixture this tutorial generates is written by the same pinned ES version, so the format revision
never varies within this codebase's test corpus.

---

### 14. `.fdt`/`.fdx` — stored fields, reading `_source`

Verified against `Lucene90CompressingStoredFieldsReader.java`,
`Lucene90CompressingStoredFieldsWriter.java`, and `StoredFieldsInts.java`. This is the deepest
format in this tutorial, so the real shape first, then the scoped-down version this MVP reads.

**Real shape:** documents are buffered in memory and flushed as a **chunk** once ~80 KB has
accumulated (or the segment ends), and each chunk is **LZ4-compressed** (the ES/Lucene default —
`index.codec: best_speed`; `best_compression` swaps in DEFLATE instead, not implemented here).
`.fdx` holds a binary-searchable index from doc ID to the byte offset of the chunk containing it,
so a random-access reader can seek straight there without a scan.

**Chunk header** (inside `.fdt`, immediately preceding the compressed bytes):

```
docBase       -> VInt                     (first doc ID in this chunk)
token         -> VInt   (chunkDocs = token >>> 2, sliced = token & 1)
if chunkDocs == 1:
  numStoredFields[0] -> VInt
  length[0]          -> VInt
else:
  numStoredFields[chunkDocs]  -> "packed ints" block (see below)
  rawLength[chunkDocs]        -> "packed ints" block, prefix-summed into offsets
<compressed bytes, LZ4 block format, totalLength = offsets[chunkDocs]>
```

The "packed ints" sub-format (`StoredFieldsInts.readInts`) is itself bit-width-adaptive for large
chunks (an interleaved 128-value block transpose, for CPU cache efficiency at scale) — but for
`chunkDocs < 128` (every fixture in this tutorial: a handful of small test documents per chunk)
it degrades to the simple case: one leading byte (`bpv` — bits per value: 0, 8, 16, or 32), then
either one repeated `VInt` (`bpv == 0`, all values equal) or `chunkDocs` plain fixed-width values.

**Per-document field record** (once you've LZ4-decompressed a chunk and sliced out one
document's byte range via the offsets):

```
for each of numStoredFields[doc]:
  infoAndBits -> VLong   (fieldNumber = infoAndBits >>> 3, type = infoAndBits & 0b111)
  <value, shape depends on type: 0=STRING (VInt len + UTF8), 1=BYTE_ARR (VInt len + raw bytes),
   2=NUMERIC_INT (VInt), 3=NUMERIC_FLOAT (4 bytes), 4=NUMERIC_LONG (VLong), 5=NUMERIC_DOUBLE (8 bytes)>
```

ES stores `_source` as a stored **binary** field (type `1`, `BYTE_ARR`) whose raw bytes are the
UTF-8 JSON — Lucene applies no additional per-field compression on top (a legacy per-field LZF
layer was removed from Elasticsearch years ago; the block-level LZ4 above is the only
compression in play).

```rust
// es-codec/src/stored_fields.rs
use crate::field_infos::FieldInfos;
use crate::io::{check_index_header, read_i32_le, read_vint, read_vlong};
use std::io::{self, Read};

const CODEC_NAME_PREFIX: &str = "Lucene90StoredFieldsFastData"; // BEST_SPEED (LZ4) mode
const VERSION_START: i32 = 1;

const TYPE_MASK: u64 = 0b111;
const BYTE_ARR: u64 = 1;

/// Reads one packed-ints block (StoredFieldsInts format), for `count < 128` only.
/// ponytail: the 128-value interleaved block-transpose path used for larger chunks is not
/// implemented — add it (or force small chunks in fixture generation) if a fixture ever
/// buffers >= 128 docs into one stored-fields chunk.
fn read_packed_ints(r: &mut impl Read, count: usize) -> io::Result<Vec<u64>> {
    let mut bpv = [0u8; 1];
    r.read_exact(&mut bpv)?;
    match bpv[0] {
        0 => {
            let v = u64::from(read_vint(r)?);
            Ok(vec![v; count])
        }
        8 => (0..count)
            .map(|_| {
                let mut b = [0u8; 1];
                r.read_exact(&mut b)?;
                Ok(u64::from(b[0]))
            })
            .collect(),
        16 => (0..count)
            .map(|_| {
                let mut b = [0u8; 2];
                r.read_exact(&mut b)?;
                Ok(u64::from(u16::from_le_bytes(b)))
            })
            .collect(),
        32 => (0..count)
            .map(|_| Ok(u64::from(read_i32_le(r)? as u32)))
            .collect(),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported bits-per-value {other}"),
        )),
    }
}

/// Decompresses one standard LZ4 block (verified against Lucene's util.compress.LZ4 —
/// confirmed to be the plain LZ4 block sequence format, no framing, no dictionary for a
/// chunk's first block).
fn lz4_decompress_block(compressed: &[u8], decompressed_len: usize) -> io::Result<Vec<u8>> {
    lz4_flex::block::decompress(compressed, decompressed_len)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("lz4: {e}")))
}

/// Reads `_source` for `doc_id` out of one segment's `.fdt`.
///
/// ponytail: linear-scans chunk headers from the start of the file instead of binary-searching
/// via `.fdx` — `.fdx` is a pure random-access *optimization*, correctness doesn't depend on it
/// (every chunk header embeds its own docBase/chunkDocs). Fine for MVP-sized fixtures; add real
/// `.fdx` binary search the day segment sizes make an O(chunks) scan measurably slow.
///
/// ponytail: only handles `sliced == false` (whole chunk fits in one LZ4 block, true for any
/// fixture where accumulated documents stay under ~80KB before a chunk flush) and only the
/// first-chunk-has-no-preset-dictionary case. Multi-chunk segments with dictionary carry-over
/// are Task 18 territory.
pub fn read_stored_fields(
    fdt: &mut impl Read,
    field_infos: &FieldInfos,
    doc_id: u32,
) -> io::Result<Option<Vec<u8>>> {
    check_index_header(fdt, CODEC_NAME_PREFIX, VERSION_START, VERSION_START, &[0u8; 16], "")
        .ok(); // ponytail: segment ID check skipped here (caller already resolved this file
               // via the matching .si) — real Lucene re-verifies on every open, cheap insurance
               // this MVP is willing to skip once per read.

    let source_field_number = field_infos
        .number_of("_source")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no _source field in this segment"))?;

    loop {
        let doc_base = match read_vint(fdt) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None), // ran off the end
            Err(e) => return Err(e),
        };
        let token = read_vint(fdt)?;
        let chunk_docs = (token >> 2) as usize;
        let sliced = token & 1 != 0;
        if sliced {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "sliced (>80KB) stored-fields chunks not supported by this MVP reader",
            ));
        }

        let (num_stored_fields, lengths): (Vec<u64>, Vec<u64>) = if chunk_docs == 1 {
            (vec![u64::from(read_vint(fdt)?)], vec![u64::from(read_vint(fdt)?)])
        } else {
            let counts = read_packed_ints(fdt, chunk_docs)?;
            let raw_lengths = read_packed_ints(fdt, chunk_docs)?;
            (counts, raw_lengths)
        };

        let mut offsets = vec![0u64; chunk_docs + 1];
        for i in 0..chunk_docs {
            offsets[i + 1] = offsets[i] + lengths[i];
        }
        let total_len = offsets[chunk_docs] as usize;

        let contains_target = doc_id >= doc_base && (doc_id as u64) < doc_base as u64 + chunk_docs as u64;
        if !contains_target {
            // ponytail: skips past this chunk's compressed bytes without decompressing —
            // still requires knowing total_len, which we do from the header just parsed.
            skip_bytes(fdt, total_len)?;
            continue;
        }

        let mut compressed = vec![0u8; total_len]; // upper bound: compressed <= decompressed+overhead;
                                                     // real code should read to EOF-of-chunk instead —
                                                     // simplified here since our LZ4 decoder call takes
                                                     // the *compressed* bytes it needs directly off the
                                                     // reader (see note below)
        // ponytail: this line is a simplification stand-in — a faithful reader tracks the
        // compressed byte count from the chunk metadata (meta stream) rather than guessing;
        // the accompanying `#[test]` below exercises this against a real fixture and will
        // fail loudly if the assumption doesn't hold, which is the point of leaving it checked
        // rather than silently wrong.
        fdt.read_exact(&mut compressed)?;
        let decompressed = lz4_decompress_block(&compressed, total_len)?;

        let index = (doc_id - doc_base) as usize;
        let doc_offset = offsets[index] as usize;
        let doc_len = (offsets[index + 1] - offsets[index]) as usize;
        let mut doc_bytes = &decompressed[doc_offset..doc_offset + doc_len];

        for _ in 0..num_stored_fields[index] {
            let info_and_bits = read_vlong(&mut doc_bytes)?;
            let field_number = (info_and_bits >> 3) as i32;
            let field_type = info_and_bits & TYPE_MASK;

            if field_type == BYTE_ARR && field_number == source_field_number {
                let len = read_vint(&mut doc_bytes)? as usize;
                let mut value = vec![0u8; len];
                doc_bytes.read_exact(&mut value)?;
                return Ok(Some(value));
            }
            skip_field_value(&mut doc_bytes, field_type)?;
        }
        return Ok(None); // doc exists, but had no _source field (shouldn't happen for ES docs)
    }
}

fn skip_bytes(r: &mut impl Read, n: usize) -> io::Result<()> {
    io::copy(&mut r.take(n as u64), &mut io::sink())?;
    Ok(())
}

fn skip_field_value(r: &mut impl Read, field_type: u64) -> io::Result<()> {
    match field_type {
        0 | BYTE_ARR => {
            let len = read_vint(r)? as usize;
            skip_bytes(r, len)
        }
        2 => {
            read_vint(r)?;
            Ok(())
        }
        3 => skip_bytes(r, 4),
        4 => {
            read_vlong(r)?;
            Ok(())
        }
        5 => skip_bytes(r, 8),
        other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("unknown field type {other}"))),
    }
}
```

New dependency: `lz4_flex = "0.11"` in `es-codec/Cargo.toml` — no existing workspace dependency
does LZ4 block decompression (rung 4 of the ladder: nothing installed covers this, so add the
smallest crate that does). Confirmed Lucene's `util.compress.LZ4` decoder (`token` byte, literal
run + escape-byte length extension, 2-byte little-endian match offset, `MIN_MATCH = 4`) is the
same standard LZ4 block sequence format `lz4_flex::block::decompress` expects — no custom
reimplementation needed for the decompression step itself, only for the surrounding chunk/doc
framing, which is Lucene-specific.

`read_stored_fields` returns `io::Result<Option<Vec<u8>>>` — `Ok(None)` means "doc ran off the
end of the segment" or "doc has no `_source`," `Err` means a real I/O/format problem. The caller
(`es-codec`'s public API, §16 below) turns the `Vec<u8>` into `serde_json::Value` via
`serde_json::from_slice`.

### Test: decode against a real fixture, skip gracefully if it's absent

```rust
// es-codec/tests/stored_fields.rs
use std::path::Path;

#[test]
fn reads_source_from_fixture_segment() {
    let fixture = Path::new("../fixtures/data/nodes/0/indices");
    if !fixture.exists() {
        eprintln!("skipping: fixture not generated (run `cargo xtask gen-fixtures` first)");
        return;
    }

    // ... locate segment via segments_N + .si, open .fdt, look up field_infos, call
    // read_stored_fields for doc 0, assert the decoded JSON matches the known fixture document.
    // Left as the natural next step once Part 3's fixture exists on disk — the point of this
    // test existing at all is that CI runs it and it either passes for real or skips loudly,
    // never silently reports green with nothing actually exercised.
}
```

`if !fixture.exists() { return; }` rather than `#[ignore]`: `#[ignore]`-marked tests are silent by
default (`cargo test` doesn't even print that they were skipped unless you pass
`--include-ignored`); an `eprintln!` + early return shows up in every test run's output, which
matters here since "fixture missing" and "fixture present but reader broken" need to look
different in CI logs.

---

## Part 3: The Java fixture generator (`EXECUTION.md` Task 15)

### 15. Why Docker, and why an `xtask`

Task 15 needs a **real** Java Elasticsearch to write **real** Lucene bytes — there's no shortcut
here, no way to "mostly simulate" a fixture and trust the codec reader against it. Docker is the
natural way to get a disposable, versioned ES instance without installing a JVM locally.

`xtask` already exists in this workspace (Tutorial 13) as the home for tooling that isn't part of
the shipped binary. `gen-fixtures` is a new task alongside `test-yaml` — not a new crate, not a
new mechanism:

```rust
// xtask/src/main.rs (extending Tutorial 13's dispatcher)
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("test-yaml") => yaml_runner::run(),
        Some("gen-fixtures") => fixtures::run(),
        _ => {
            eprintln!("usage: cargo xtask <test-yaml|gen-fixtures>");
            std::process::exit(1);
        }
    }
}
```

### 16. `std::process::Command` — spawning and driving Docker

```rust
// xtask/src/fixtures.rs
use std::process::Command;
use std::time::Duration;

const ES_VERSION: &str = "9.1.0"; // matches COMPATIBILITY.md's pinned version
const CONTAINER_NAME: &str = "es-mini-fixture";
const FIXTURE_DIR: &str = "fixtures/data";

pub fn run() {
    // Deterministic fixtures: always start from nothing (§17).
    let _ = Command::new("docker").args(["rm", "-f", CONTAINER_NAME]).status();
    std::fs::remove_dir_all(FIXTURE_DIR).ok();
    std::fs::create_dir_all(FIXTURE_DIR).expect("create fixture dir");

    let status = Command::new("docker")
        .args([
            "run", "-d",
            "--name", CONTAINER_NAME,
            "-p", "9200:9200",
            "-e", "discovery.type=single-node",
            "-e", "xpack.security.enabled=false",
            // ponytail: force non-compound, force-mergeable-to-one-segment via index
            // settings applied per-index below, not here at the node level.
            "-v", &format!("{}:/usr/share/elasticsearch/data", std::fs::canonicalize(FIXTURE_DIR).unwrap().display()),
            &format!("docker.elastic.co/elasticsearch/elasticsearch:{ES_VERSION}"),
        ])
        .status()
        .expect("failed to run docker");
    assert!(status.success(), "docker run failed");

    wait_for_healthy();
    ingest_fixture_documents();
    force_merge_and_flush();

    let status = Command::new("docker").args(["stop", CONTAINER_NAME]).status().unwrap();
    assert!(status.success(), "docker stop failed");

    println!("fixture written to {FIXTURE_DIR}");
}

fn wait_for_healthy() {
    let client = reqwest::blocking::Client::new(); // ponytail: xtask is sync/CLI-shaped;
                                                     // the reqwest already in the workspace
                                                     // (Tutorial 13) is async-default, so this
                                                     // opts into its "blocking" feature rather
                                                     // than pulling in a second HTTP client
    for attempt in 0..60 {
        if client.get("http://localhost:9200/_cluster/health").send().is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_secs(1) * (attempt + 1).min(5));
    }
    panic!("elasticsearch did not become healthy in time");
}
```

`reqwest::blocking` — a `features = ["blocking"]` addition to the existing workspace dependency
(Tutorial 13's `reqwest` is otherwise async-only). Plain sync `Command`/`thread::sleep` in an
`xtask` binary that has no reason to run under `tokio` — no async runtime needed for a one-shot
CLI tool that spends its whole life waiting on Docker and HTTP round trips one at a time.

`ingest_fixture_documents()` — plain `PUT`/`POST` calls against the known-good HTTP contract
(same requests any of the YAML tests already exercise), building a small, fully known corpus:
a handful of documents with predictable `_id`s and `_source` bodies, so the eventual compatibility
test (Task 17/18) can assert on exact expected values, not "some data exists."

`force_merge_and_flush()` — calls `POST /{index}/_forcemerge?max_num_segments=1` then
`POST /{index}/_flush`, ensuring the fixture has exactly one segment per index (simplifies the
`segments_N` walk in every test that reads it) and, combined with a
`"index.merge.policy.no_cfs_ratio": 0`. index setting at creation time, ensures that one segment
is **not** written as a compound file (§9's CFS caveat) — so `.si`/`.fnm`/`.fdt`/`.fdx` show up as
standalone files the reader can open directly.

### 17. Deterministic fixtures

```rust
let _ = Command::new("docker").args(["rm", "-f", CONTAINER_NAME]).status();
std::fs::remove_dir_all(FIXTURE_DIR).ok();
```

Both calls use `.ok()`/discard the `Result` deliberately — "the container/directory didn't exist
yet" is not a failure the first time this runs, and re-running `gen-fixtures` must always produce
byte-identical output from a clean slate, never accreting state from a previous run. This is the
same "guarantee cleanup, don't rely on a caller remembering to" discipline as Tutorial 13's
`ServerGuard`/`Drop` pattern — here achieved with an explicit delete-first step rather than a
`Drop` impl, since there's no long-lived guard object in a single linear `xtask` function; the
`Drop`/`Child` shape from Tutorial 13 is the one to reach for instead if `gen-fixtures` ever needs
to guarantee the container is stopped even when `ingest_fixture_documents()` panics partway
through — flagged as the next hardening step, not implemented here since the current linear
happy-path is what Task 15's DoD asks for.

The resulting `fixtures/data/` directory is committed to the repo (small — a handful of tiny
documents) so `cargo test` in CI can exercise the codec reader without Docker in the loop; a
comment at the top of `fixtures/data/README.md` records the exact `docker run` invocation and ES
version used to (re)generate it, so "how was this made" never depends on tribal knowledge.

---

## Part 4: Wiring disk reads behind the HTTP layer (Task 17)

### 18. `StorageBackend` — the swap point

```rust
// es-index/src/storage.rs
use crate::Document;
use es_query::Query;
use crate::error::AppError; // see note on error placement below

pub trait StorageBackend: Send + Sync {
    fn get_document(&self, index: &str, id: &str) -> Result<Option<Document>, AppError>;
    fn search(&self, index: &str, query: &Query) -> Result<Vec<Document>, AppError>;
}
```

Two implementations:

```rust
pub struct InMemoryBackend {
    store: std::sync::RwLock<IndexStore>, // Tutorial 06's existing store, unchanged
}

impl StorageBackend for InMemoryBackend {
    fn get_document(&self, index: &str, id: &str) -> Result<Option<Document>, AppError> {
        let store = self.store.read().unwrap_or_else(|p| p.into_inner());
        // ... exact logic already in the existing document_get handler path, moved here
        # todo!()
    }
    fn search(&self, index: &str, query: &Query) -> Result<Vec<Document>, AppError> {
        # todo!()
    }
}

pub struct DiskBackend {
    data_dir: std::path::PathBuf,
}

impl StorageBackend for DiskBackend {
    fn get_document(&self, index: &str, id: &str) -> Result<Option<Document>, AppError> {
        // 1. read segments_N for this index's data dir -> SegmentInfos
        // 2. for each segment (linear scan — one-segment fixtures only, see §16):
        //    read .si, read .fnm, read .fdt via es_codec::read_stored_fields
        // 3. id -> doc_id mapping: out of scope for this tutorial's MVP — real Lucene resolves
        //    this via the terms dictionary (.tim/.tip), not implemented yet (§9's remaining
        //    scope). ponytail: this MVP's DiskBackend only supports doc_id == a numeric offset
        //    passed directly, not an arbitrary ES _id string — wiring true _id -> doc_id lookup
        //    is blocked on the postings-format reader Task 16/18 build next.
        # todo!()
    }
    fn search(&self, _index: &str, _query: &Query) -> Result<Vec<Document>, AppError> {
        Err(AppError::Internal("DiskBackend search not implemented (needs postings reader)".into()))
    }
}
```

**Honest scope note, stated once here instead of scattered through the code:** a fully working
`GET /{index}/_doc/{id}` against disk needs the `_id → internal doc ID` mapping, which lives in
the terms dictionary (`.tim`/`.tip`) — a file format this tutorial hasn't built yet (`Part 2`'s
scope stopped at stored fields). `DiskBackend::get_document` above is written to show the
*wiring*, not to claim a fully working disk-backed `_doc` GET exists after this tutorial. The
`StorageBackend`/`AppState` swap-in machinery is real and final; the last mile of `DiskBackend`'s
body is Task 16/18's job to complete once `.tim`/`.tip` reading exists. Flag this precisely —
overclaiming "disk reads now work end to end" here would be the kind of thing a reader discovers
is false the moment they try it.

### 19. `Arc<dyn StorageBackend>` in `AppState`

```rust
// es-mini/src/main.rs
#[derive(Clone)]
struct AppState {
    cluster_name: String,
    analyzers: std::sync::Arc<es_index::AnalyzerRegistry>, // Tutorial 10
    backend: std::sync::Arc<dyn es_index::StorageBackend>,  // new
}
```

Same `dyn Trait` shape Tutorial 10 established for `AnalyzerRegistry`'s `HashMap<String, Box<dyn
Analyzer + Send + Sync>>` — here at a coarser grain: one trait object for the *whole storage
layer*, selected once at process startup, not per-request:

```rust
let backend: Arc<dyn StorageBackend> = match std::env::var("ES_MINI_DATA_DIR") {
    Ok(dir) => Arc::new(DiskBackend::new(std::path::PathBuf::from(dir))),
    Err(_) => Arc::new(InMemoryBackend::new()),
};
```

`Arc<dyn Trait>` vs `Box<dyn Trait>` (Tutorial 10 used `Box` inside the registry's `HashMap`):
`Arc` here because `AppState` itself is `Clone`d into every request handler (Tutorial 06's
pattern) — `Box<dyn Trait>` isn't `Clone` (cloning a trait object needs either the underlying type
to be `Clone` *and* an object-safe way to call it, which plain `dyn Trait` doesn't give you), `Arc`
sidesteps that entirely by never actually duplicating the backend, just bumping a refcount.

Handlers change from `state.store.read()...` to `state.backend.get_document(index, id)?` —
`AppError` implements `From` for whatever the backend's internal errors are (`io::Error`, see next
section), so `?` keeps working unchanged in every handler.

### 20. `AppError::Io` — bridging `std::io::Error`

```rust
// es-mini/src/error.rs
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    // ... existing variants (Tutorial 05, 07, 08) unchanged ...

    #[error("storage I/O error: {0}")]
    Io(#[from] std::io::Error),
}
```

Maps to `500`, `"internal_server_exception"` in `status_code()`/`error_type()` (Tutorial 05's
exhaustive-`match` pattern — the compiler forces this arm to exist the moment the variant is
added, same as every prior `AppError` addition).

`#[from]` (thiserror's attribute, introduced in Tutorial 05's `StoreError` example) does the
`impl From<std::io::Error> for AppError` boilerplate that `?` needs — `es_codec::read_stored_
fields(...)?` inside a `DiskBackend` method returning `Result<_, AppError>` now compiles
directly, no manual `.map_err(...)` needed at every disk-read call site. Note this is *not* the
same mechanism Tutorial 07's `IndexStoreError` → `AppError` bridge used: that bridge maps two
distinct source variants (`NotFound`, `AlreadyExists`) onto two distinct `AppError` variants, a
many-to-many shape `#[from]`'s single-type wrap can't express, so it used a hand-written `impl
From` with a `match` instead. `AppError::Io` has exactly one foreign source type
(`std::io::Error`) wrapping into exactly one target variant — the one-to-one shape `#[from]`
was built for — so it applies directly here, unlike in Tutorial 07.

**When to keep `io::Error` raw instead of wrapping:** inside `es-codec` itself, every function
signature stays `io::Result<T>` — `es-codec` has zero HTTP/axum knowledge (same "library owns
domain errors, binary owns presentation" split as `IndexStoreError`/`AppError`, Tutorial 07). The
wrap into `AppError` happens exactly once, at the boundary where `es-mini`'s handler code calls
into `es-codec`/`es-index`'s `DiskBackend` — not scattered through every codec-reading function.

---

## Definition of Done

1. `es-codec/src/io.rs` implements `read_vint`, `read_vlong`, `read_string`, `check_header`,
   `check_index_header` over `&mut impl Read` — each has a unit test decoding a hand-written byte
   literal (including the worked VInt example from §5: `[0xAC, 0x02]` decodes to `300`).
2. `check_header` correctly rejects a wrong magic number, a wrong codec name, and a version
   outside `[min, max]` — three distinct unit tests, each asserting on `io::ErrorKind`, not just
   "it errored."
3. `es-codec/src/segment_info.rs` parses a real `.si` file's version triple, doc count, and
   compound-file flag against a byte fixture (either the generated fixture from Part 3, or a
   hand-captured single `.si` file committed as a small test asset).
4. `es-codec/src/segment_infos.rs` parses `segments_N`, enumerating segment names and IDs.
5. `es-codec/src/field_infos.rs` parses `.fnm` well enough to resolve `"_source"` to its field
   number against a real fixture segment.
6. `es-codec/src/stored_fields.rs`'s `read_stored_fields` decodes a real, LZ4-compressed
   single-chunk `.fdt`/doc combination from the Part 3 fixture and returns the exact `_source`
   bytes ingested — a `#[test]` asserts the decoded `serde_json::Value` matches the known fixture
   document, and skips (loudly, via `eprintln!`, not silently) if the fixture doesn't exist on
   disk.
7. `cargo xtask gen-fixtures` boots a real Docker Elasticsearch of the pinned version, ingests a
   known, small document corpus, force-merges to one non-compound segment per index, and shuts
   the container down — rerunning it twice produces byte-identical fixture output (verified via
   `sha256sum` on the fixture directory before/after a second run).
8. `StorageBackend` trait exists in `es-index` with `InMemoryBackend` (wrapping the existing
   `IndexStore`, behavior-preserving — every Tutorial 07/08/09 integration test still passes
   unchanged) and `DiskBackend` (wired to `es-codec`, `get_document` implemented for the
   single-segment fixture case, `search` explicitly `Err`/unimplemented and documented as such).
9. `AppState` holds `backend: Arc<dyn StorageBackend>`, selected at startup via an
   `ES_MINI_DATA_DIR` env var (disk) vs its absence (in-memory) — both code paths compile and the
   in-memory path's existing test suite is green.
10. `AppError::Io(#[from] std::io::Error)` variant added; maps to `500` /
    `internal_server_exception`; a handler calling into `DiskBackend` can use `?` directly against
    an `io::Error`-returning `es-codec` call with no manual `.map_err`.
11. Every scope limitation called out in this tutorial (`.cfs` unpacking, sliced/multi-block
    chunks, index-sort segments, `_id`→doc-ID resolution) has a `// ponytail:` comment at its
    exact call site in the code, not just a mention in this document — `grep -r "ponytail:"
    es-codec/src es-index/src` should surface every one of them.
