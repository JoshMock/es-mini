//! Lucene `DataInput`: little-endian fixed-width ints, variable-length ints, and strings.

use crate::{Result, err};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub struct DataInput<'a> {
    buf: &'a [u8],
    pub pos: usize,
}

impl<'a> DataInput<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.buf.len());
        let Some(end) = end else {
            return err(format!(
                "read past EOF: {n} bytes at {} of {}",
                self.pos,
                self.buf.len()
            ));
        };
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self
            .read_bytes(N)?
            .try_into()
            .expect("read_bytes returned N bytes"))
    }

    pub fn read_byte(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub fn read_int(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    pub fn read_long(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    /// Codec headers and footers are big-endian.
    pub fn read_be_int(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    pub fn read_be_long(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    pub fn read_vlong(&mut self) -> Result<i64> {
        let mut out = 0i64;
        for shift in (0..64).step_by(7) {
            let b = self.read_byte()?;
            out |= ((b & 0x7f) as i64) << shift;
            if b & 0x80 == 0 {
                return Ok(out);
            }
        }
        err("malformed vlong")
    }

    pub fn read_vint(&mut self) -> Result<i32> {
        let v = self.read_vlong()?;
        // Java writes negative vints as 5 bytes; keep the low 32 bits like Java's int shifts.
        Ok(v as i32)
    }

    pub fn read_len(&mut self) -> Result<usize> {
        let n = self.read_vint()?;
        usize::try_from(n).or_else(|_| err(format!("negative length {n}")))
    }

    pub fn read_string(&mut self) -> Result<String> {
        let n = self.read_len()?;
        String::from_utf8(self.read_bytes(n)?.to_vec()).or_else(|_| err("invalid UTF-8 string"))
    }

    pub fn read_set_of_strings(&mut self) -> Result<BTreeSet<String>> {
        (0..self.read_len()?).map(|_| self.read_string()).collect()
    }

    pub fn read_map_of_strings(&mut self) -> Result<BTreeMap<String, String>> {
        (0..self.read_len()?)
            .map(|_| Ok((self.read_string()?, self.read_string()?)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vints_and_strings() {
        let mut i = DataInput::new(&[
            0x7f, 0x80, 0x01, 0xff, 0xff, 0xff, 0xff, 0x0f, 2, b'h', b'i',
        ]);
        assert_eq!(i.read_vint().unwrap(), 127);
        assert_eq!(i.read_vint().unwrap(), 128);
        assert_eq!(i.read_vint().unwrap(), -1);
        assert_eq!(i.read_string().unwrap(), "hi");
        assert!(i.read_byte().is_err());
    }

    #[test]
    fn endianness() {
        let b = [1, 0, 0, 0, 0, 0, 0, 1];
        assert_eq!(DataInput::new(&b).read_int().unwrap(), 1);
        assert_eq!(DataInput::new(&b).read_be_int().unwrap(), 0x0100_0000);
    }
}
