//! `segments_N` (SegmentInfos) and per-segment `.si` (Lucene99SegmentInfoFormat).

use crate::{
    Result,
    codec_util::{self, ID_LEN},
    err,
    input::DataInput,
    output::Output,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// segments_N format versions accepted (Lucene 9.0+); this crate targets Lucene 10 only.
const SEGMENTS_VERSION_74: i32 = 9;
const SEGMENTS_VERSION_CURRENT: i32 = 10;
const MIN_SUPPORTED_MAJOR: i32 = 10;

/// (major, minor, bugfix)
pub type Version = (i32, i32, i32);

/// Lucene version stamped on everything es-mini writes (the 10.5.1 fixture).
pub const LUCENE_VERSION: Version = (10, 5, 1);

#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub id: [u8; ID_LEN],
    pub version: Version,
    pub min_version: Option<Version>,
    pub max_doc: i32,
    pub is_compound: bool,
    pub has_blocks: bool,
    pub diagnostics: BTreeMap<String, String>,
    pub files: BTreeSet<String>,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct SegmentCommitInfo {
    pub name: String,
    pub codec: String,
    pub info: SegmentInfo,
    pub del_gen: i64,
    pub del_count: i32,
    pub field_infos_gen: i64,
    pub dv_gen: i64,
    pub soft_del_count: i32,
    pub field_infos_files: BTreeSet<String>,
    pub dv_update_files: BTreeMap<i32, BTreeSet<String>>,
}

#[derive(Debug, Clone)]
pub struct SegmentInfos {
    pub id: [u8; ID_LEN],
    pub generation: u64,
    pub lucene_version: Version,
    pub index_created_major: i32,
    pub version: i64,
    pub counter: i64,
    pub segments: Vec<SegmentCommitInfo>,
    pub user_data: BTreeMap<String, String>,
}

fn read_id(input: &mut DataInput) -> Result<[u8; ID_LEN]> {
    Ok(input.read_bytes(ID_LEN)?.try_into().expect("ID_LEN bytes"))
}

fn read_version(input: &mut DataInput, vint: bool) -> Result<Version> {
    let mut next = || {
        if vint {
            input.read_vint()
        } else {
            input.read_int()
        }
    };
    Ok((next()?, next()?, next()?))
}

/// `index.version.created` / `es_version` / oldest index version of Elasticsearch 9.5.5 data (from the fixture).
pub const INDEX_VERSION_ID: &str = "9111000";
/// Node version id of Elasticsearch 9.5.5.
pub const NODE_VERSION_ID: &str = "9050599";

/// Commit user data Elasticsearch 9.5.5 requires on a shard's Lucene commit.
pub fn shard_user_data(
    history_uuid: &str,
    translog_uuid: &str,
    max_seq_no: i64,
    local_checkpoint: i64,
) -> BTreeMap<String, String> {
    [
        ("history_uuid", history_uuid.to_string()),
        ("translog_uuid", translog_uuid.to_string()),
        ("es_version", INDEX_VERSION_ID.into()),
        ("max_seq_no", max_seq_no.to_string()),
        ("local_checkpoint", local_checkpoint.to_string()),
        ("min_retained_seq_no", (local_checkpoint + 1).to_string()),
        ("max_unsafe_auto_id_timestamp", "-1".into()),
    ]
    .map(|(k, v)| (k.to_string(), v))
    .into()
}

impl SegmentInfos {
    /// Reads the commit with the highest generation in `dir`.
    pub fn read_latest(dir: &Path) -> Result<Self> {
        let io = |e: std::io::Error| crate::Error(format!("{}: {e}", dir.display()));
        let mut latest = None;
        for entry in std::fs::read_dir(dir).map_err(io)? {
            let name = entry
                .map_err(io)?
                .file_name()
                .to_string_lossy()
                .into_owned();
            if let Some(g) = name
                .strip_prefix("segments_")
                .and_then(|g| u64::from_str_radix(g, 36).ok())
            {
                latest = latest.max(Some(g));
            }
        }
        let Some(generation) = latest else {
            return err(format!("no segments_N in {}", dir.display()));
        };
        let file = crate::read_file(&dir.join(format!("segments_{}", radix36(generation))))?;
        Self::parse(dir, &file, generation)
    }

    /// Writes each segment's `.si` and `segments_<generation>` into `dir`.
    pub fn write(&self, dir: &Path) -> Result<()> {
        let mut out = Output::default();
        out.index_header(
            "segments",
            SEGMENTS_VERSION_CURRENT,
            &self.id,
            &radix36(self.generation),
        );
        write_version(&mut out, self.lucene_version, true);
        out.vint(self.index_created_major);
        out.be_long(self.version);
        out.vlong(self.counter);
        out.be_int(self.segments.len() as i32);
        if let Some(min) = self.segments.iter().map(|s| s.info.version).min() {
            write_version(&mut out, min, true);
        }
        for s in &self.segments {
            write_si(dir, &s.name, &s.info)?;
            out.string(&s.name);
            out.bytes(&s.info.id);
            out.string(&s.codec);
            out.be_long(s.del_gen);
            out.be_int(s.del_count);
            out.be_long(s.field_infos_gen);
            out.be_long(s.dv_gen);
            out.be_int(s.soft_del_count);
            out.byte(0); // no SegmentCommitInfo id
            out.set_of_strings(&s.field_infos_files);
            out.be_int(s.dv_update_files.len() as i32);
            for (field, files) in &s.dv_update_files {
                out.be_int(*field);
                out.set_of_strings(files);
            }
        }
        out.map_of_strings(&self.user_data);
        let path = dir.join(format!("segments_{}", radix36(self.generation)));
        crate::write_file_atomic(&path, &out.finish())
    }

    fn parse(dir: &Path, file: &[u8], generation: u64) -> Result<Self> {
        codec_util::check_footer(file)?;
        let input = &mut DataInput::new(file);
        let format = codec_util::check_header(
            input,
            "segments",
            SEGMENTS_VERSION_74,
            SEGMENTS_VERSION_CURRENT,
        )?;
        let id = read_id(input)?;
        codec_util::check_index_header_suffix(input, &radix36(generation))?;
        let lucene_version = read_version(input, true)?;
        let index_created_major = input.read_vint()?;
        if index_created_major < MIN_SUPPORTED_MAJOR {
            return err(format!(
                "index created by Lucene {index_created_major}; only Lucene 10 is supported"
            ));
        }
        let version = input.read_be_long()?;
        let counter = input.read_vlong()?;
        let n = input.read_be_int()?;
        let mut segments = Vec::new();
        if n > 0 {
            read_version(input, true)?; // min segment Lucene version
        }
        for _ in 0..n {
            let name = input.read_string()?;
            let seg_id = read_id(input)?;
            let codec = input.read_string()?;
            let info = read_si(dir, &name, &seg_id)?;
            let del_gen = input.read_be_long()?;
            let del_count = input.read_be_int()?;
            let field_infos_gen = input.read_be_long()?;
            let dv_gen = input.read_be_long()?;
            let soft_del_count = input.read_be_int()?;
            if del_count < 0 || soft_del_count < 0 || del_count + soft_del_count > info.max_doc {
                return err(format!("invalid deletion counts in segment {name}"));
            }
            if format > SEGMENTS_VERSION_74 {
                match input.read_byte()? {
                    0 => {}
                    1 => {
                        read_id(input)?;
                    }
                    m => return err(format!("invalid SegmentCommitInfo id marker {m}")),
                }
            }
            let field_infos_files = input.read_set_of_strings()?;
            let mut dv_update_files = BTreeMap::new();
            for _ in 0..input.read_be_int()? {
                dv_update_files.insert(input.read_be_int()?, input.read_set_of_strings()?);
            }
            segments.push(SegmentCommitInfo {
                name,
                codec,
                info,
                del_gen,
                del_count,
                field_infos_gen,
                dv_gen,
                soft_del_count,
                field_infos_files,
                dv_update_files,
            });
        }
        let user_data = input.read_map_of_strings()?;
        Ok(Self {
            id,
            generation,
            lucene_version,
            index_created_major,
            version,
            counter,
            segments,
            user_data,
        })
    }
}

fn write_version(out: &mut Output, (a, b, c): Version, vint: bool) {
    for v in [a, b, c] {
        if vint {
            out.vint(v);
        } else {
            out.bytes(&v.to_le_bytes());
        }
    }
}

fn write_si(dir: &Path, name: &str, si: &SegmentInfo) -> Result<()> {
    let mut out = Output::default();
    out.index_header("Lucene90SegmentInfo", 0, &si.id, "");
    write_version(&mut out, si.version, false);
    match si.min_version {
        None => out.byte(0),
        Some(v) => {
            out.byte(1);
            write_version(&mut out, v, false);
        }
    }
    out.bytes(&si.max_doc.to_le_bytes());
    out.byte(si.is_compound as u8);
    out.byte(si.has_blocks as u8);
    out.map_of_strings(&si.diagnostics);
    out.set_of_strings(&si.files);
    out.map_of_strings(&si.attributes);
    out.vint(0); // no index sort
    let path = dir.join(format!("{name}.si"));
    crate::write_file_atomic(&path, &out.finish())
}

fn read_si(dir: &Path, name: &str, id: &[u8; ID_LEN]) -> Result<SegmentInfo> {
    let file = crate::read_file(&dir.join(format!("{name}.si")))?;
    codec_util::check_footer(&file)?;
    let input = &mut DataInput::new(&file);
    codec_util::check_index_header(input, "Lucene90SegmentInfo", 0, 0, id, "")?;
    let version = read_version(input, false)?;
    let min_version = match input.read_byte()? {
        0 => None,
        1 => Some(read_version(input, false)?),
        b => return err(format!("illegal boolean {b}")),
    };
    let max_doc = input.read_int()?;
    if max_doc < 0 {
        return err(format!("invalid docCount {max_doc}"));
    }
    let is_compound = input.read_byte()? == 1;
    let has_blocks = input.read_byte()? == 1;
    let diagnostics = input.read_map_of_strings()?;
    let files = input.read_set_of_strings()?;
    let attributes = input.read_map_of_strings()?;
    // ponytail: index sorting is unsupported; ES only sorts indices that configure index.sort.*
    if input.read_vint()? != 0 {
        return err(format!(
            "segment {name} uses index sorting, which is unsupported"
        ));
    }
    Ok(SegmentInfo {
        id: *id,
        version,
        min_version,
        max_doc,
        is_compound,
        has_blocks,
        diagnostics,
        files,
        attributes,
    })
}

pub(crate) fn radix36(mut n: u64) -> String {
    let mut s = Vec::new();
    loop {
        s.push(char::from_digit((n % 36) as u32, 36).expect("digit < 36"));
        n /= 36;
        if n == 0 {
            return s.iter().rev().collect();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn write_then_read_latest() {
        use super::*;
        let dir = std::env::temp_dir().join(format!("esm-si-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let info = SegmentInfo {
            id: [7; ID_LEN],
            version: (10, 3, 0),
            min_version: Some((10, 3, 0)),
            max_doc: 5,
            is_compound: false,
            has_blocks: false,
            diagnostics: [("source".to_string(), "flush".to_string())].into(),
            files: ["_0.fnm".to_string()].into(),
            attributes: BTreeMap::new(),
        };
        let seg = SegmentCommitInfo {
            name: "_0".into(),
            codec: "Lucene103".into(),
            info,
            del_gen: -1,
            del_count: 0,
            field_infos_gen: -1,
            dv_gen: -1,
            soft_del_count: 0,
            field_infos_files: BTreeSet::new(),
            dv_update_files: BTreeMap::new(),
        };
        let infos = SegmentInfos {
            id: [9; ID_LEN],
            generation: 40,
            lucene_version: (10, 3, 0),
            index_created_major: 10,
            version: 12,
            counter: 1,
            segments: vec![seg],
            user_data: [("k".to_string(), "v".to_string())].into(),
        };
        infos.write(&dir).unwrap();
        let back = SegmentInfos::read_latest(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(back.generation, 40);
        assert_eq!(back.counter, 1);
        assert_eq!(back.user_data, infos.user_data);
        let (a, b) = (&back.segments[0], &infos.segments[0]);
        assert_eq!((a.name.as_str(), a.info.max_doc), ("_0", 5));
        assert_eq!(a.info.files, b.info.files);
        assert_eq!(a.info.diagnostics, b.info.diagnostics);
    }

    #[test]
    fn radix36_matches_java() {
        assert_eq!(super::radix36(0), "0");
        assert_eq!(super::radix36(35), "z");
        assert_eq!(super::radix36(36 * 2 + 9), "29");
    }

    #[test]
    fn shard_user_data_has_the_keys_of_an_es_shard_commit() {
        use super::*;
        let fixture = std::fs::read_dir(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../fixtures/es-9.5.5/data/indices"
        ))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("0/index");
        let real = SegmentInfos::read_latest(&fixture).unwrap().user_data;
        let ours = shard_user_data("h", "t", 0, 0);
        assert_eq!(
            real.keys().collect::<Vec<_>>(),
            ours.keys().collect::<Vec<_>>()
        );
        assert_eq!(real["es_version"], ours["es_version"]);
    }
}
