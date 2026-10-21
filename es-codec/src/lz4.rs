//! Lucene's LZ4 block format, shared by stored fields, the terms dictionary, and doc values.

use crate::{Result, err, input::DataInput, output::Output};

/// Lucene `LZ4.decompress`: reads until `out` has grown by `len` bytes. Matches may reference
/// earlier bytes of `out` (the preset dictionary).
pub(crate) fn decompress(inp: &mut DataInput, len: usize, out: &mut Vec<u8>) -> Result<()> {
    let end = out.len() + len;
    let extra = |inp: &mut DataInput, mut n: usize| -> Result<usize> {
        loop {
            let b = inp.read_byte()?;
            n += b as usize;
            if b != 0xff {
                return Ok(n);
            }
        }
    };
    loop {
        let token = inp.read_byte()? as usize;
        let mut lit = token >> 4;
        if lit == 0xf {
            lit = extra(inp, lit)?;
        }
        out.extend_from_slice(inp.read_bytes(lit)?);
        if out.len() >= end {
            break;
        }
        let dec = inp.read_byte()? as usize | (inp.read_byte()? as usize) << 8;
        let mut m = token & 0xf;
        if m == 0xf {
            m = extra(inp, m)?;
        }
        if dec == 0 || dec > out.len() {
            return err("corrupt LZ4 match offset");
        }
        for _ in 0..m + 4 {
            out.push(out[out.len() - dec]);
        }
        if out.len() >= end {
            break;
        }
    }
    if out.len() != end {
        return err("corrupt LZ4 block length");
    }
    Ok(())
}

/// Literal-only LZ4 stream, which any LZ4 decoder accepts.
pub(crate) fn literals(out: &mut Output, lits: &[u8]) {
    out.byte((lits.len().min(15) << 4) as u8);
    if lits.len() >= 15 {
        let mut rest = lits.len() - 15;
        while rest >= 255 {
            out.byte(255);
            rest -= 255;
        }
        out.byte(rest as u8);
    }
    out.bytes(lits);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_and_matches() {
        // 5 literals "abcde", then a match of 4 + 2 bytes at offset 3: "cdecde".
        let block = [0x52, b'a', b'b', b'c', b'd', b'e', 3, 0];
        let mut out = Vec::new();
        decompress(&mut DataInput::new(&block), 11, &mut out).unwrap();
        assert_eq!(out, b"abcdecdecde");
        assert!(decompress(&mut DataInput::new(&[0x10, b'a', 9, 0]), 10, &mut vec![]).is_err());
    }
}
