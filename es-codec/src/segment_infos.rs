//! `segments_N` (SegmentInfos) and per-segment `.si` (Lucene99SegmentInfoFormat).

use crate::{
    Result,
    codec_util::{self, ID_LEN},
    err,
    input::DataInput,
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
        let file =
            std::fs::read(dir.join(format!("segments_{}", radix36(generation)))).map_err(io)?;
        Self::parse(dir, &file, generation)
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
    fn radix36_matches_java() {
        assert_eq!(super::radix36(0), "0");
        assert_eq!(super::radix36(35), "z");
        assert_eq!(super::radix36(36 * 2 + 9), "29");
    }
}
