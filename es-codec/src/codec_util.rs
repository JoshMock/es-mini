//! Lucene `CodecUtil`: file headers and checksummed footers.

use crate::{Result, err, input::DataInput};

pub const CODEC_MAGIC: i32 = 0x3fd7_6c17;
pub const FOOTER_MAGIC: i32 = !CODEC_MAGIC;
pub const FOOTER_LEN: usize = 16;
pub const ID_LEN: usize = 16;

/// Reads `magic, codec name, version` and returns the version.
pub fn check_header(input: &mut DataInput, codec: &str, min: i32, max: i32) -> Result<i32> {
    let magic = input.read_be_int()?;
    if magic != CODEC_MAGIC {
        return err(format!("bad codec magic {magic:#x}"));
    }
    check_header_no_magic(input, codec, min, max)
}

pub fn check_header_no_magic(
    input: &mut DataInput,
    codec: &str,
    min: i32,
    max: i32,
) -> Result<i32> {
    let actual = input.read_string()?;
    if actual != codec {
        return err(format!("codec mismatch: expected {codec}, got {actual}"));
    }
    let version = input.read_be_int()?;
    if !(min..=max).contains(&version) {
        return err(format!(
            "unsupported {codec} version {version} (supported {min}..={max})"
        ));
    }
    Ok(version)
}

/// A header plus the segment id and suffix that tie a file to its segment.
pub fn check_index_header(
    input: &mut DataInput,
    codec: &str,
    min: i32,
    max: i32,
    id: &[u8; ID_LEN],
    suffix: &str,
) -> Result<i32> {
    let version = check_header(input, codec, min, max)?;
    check_index_header_id(input, id)?;
    check_index_header_suffix(input, suffix)?;
    Ok(version)
}

pub fn check_index_header_id(input: &mut DataInput, id: &[u8; ID_LEN]) -> Result<()> {
    if input.read_bytes(ID_LEN)? != id {
        return err("segment id mismatch");
    }
    Ok(())
}

pub fn check_index_header_suffix(input: &mut DataInput, suffix: &str) -> Result<()> {
    let n = input.read_byte()? as usize;
    if input.read_bytes(n)? != suffix.as_bytes() {
        return err(format!("header suffix mismatch: expected {suffix}"));
    }
    Ok(())
}

/// Validates the footer magic, algorithm id, and CRC32 of everything before the checksum.
pub fn check_footer(file: &[u8]) -> Result<()> {
    let Some(start) = file.len().checked_sub(FOOTER_LEN) else {
        return err("file too short for a footer");
    };
    let expected = check_footer_structure(&file[start..])?;
    let actual = crc32(&file[..file.len() - 8]) as i64;
    if expected != actual {
        return err(format!(
            "checksum mismatch: expected {expected:#x}, got {actual:#x}"
        ));
    }
    Ok(())
}

/// Validates a footer's magic and algorithm id (`CodecUtil.retrieveChecksum`) and returns its
/// checksum.
pub fn check_footer_structure(footer: &[u8]) -> Result<i64> {
    let mut f = DataInput::new(footer);
    if f.read_be_int()? != FOOTER_MAGIC {
        return err("bad footer magic");
    }
    let algorithm = f.read_be_int()?;
    if algorithm != 0 {
        return err(format!("unknown checksum algorithm {algorithm}"));
    }
    let checksum = f.read_be_long()?;
    if checksum as u64 >> 32 != 0 {
        return err(format!("illegal checksum {checksum:#x}"));
    }
    Ok(checksum)
}

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = (c >> 1) ^ (0xedb8_8320 & (c & 1).wrapping_neg());
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// CRC-32 (IEEE), as java.util.zip.CRC32.
pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0, data)
}

/// The CRC-32 of the bytes `crc` covers followed by `data`.
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut crc = !crc;
    for &b in data {
        crc = (crc >> 8) ^ CRC_TABLE[((crc ^ b as u32) & 0xff) as usize];
    }
    !crc
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc32_matches_java() {
        assert_eq!(super::crc32(b"123456789"), 0xcbf4_3926);
    }
}
