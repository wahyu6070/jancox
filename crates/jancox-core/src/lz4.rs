//! LZ4 block format decoder (no frame), as EROFS stores it. It can stop
//! after the first `want` bytes, like `LZ4_decompress_safe_partial`, which
//! EROFS needs for extents that use only the start of a pcluster.

use std::io;

use crate::fs::invalid;

fn bad() -> io::Error {
    invalid("corrupt lz4 data")
}

/// Reads an LZ4 length extension (bytes of 255 until a smaller one).
fn more_len(src: &[u8], i: &mut usize, mut n: usize) -> io::Result<usize> {
    loop {
        let b = *src.get(*i).ok_or_else(bad)?;
        *i += 1;
        n = n.checked_add(b as usize).ok_or_else(bad)?;
        if b != 255 {
            return Ok(n);
        }
    }
}

/// Decodes the LZ4 block `src` up to `want` bytes of output. Fails when the
/// block is malformed or holds fewer than `want` bytes.
pub fn decompress(src: &[u8], want: usize) -> io::Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(want);
    let mut i = 0;
    while out.len() < want {
        let token = *src.get(i).ok_or_else(bad)?;
        i += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            lit = more_len(src, &mut i, lit)?;
        }
        let end = i.checked_add(lit).ok_or_else(bad)?;
        let literals = src.get(i..end).ok_or_else(bad)?;
        let take = lit.min(want - out.len());
        out.extend_from_slice(&literals[..take]);
        i = end;
        if out.len() == want || i == src.len() {
            // the last sequence has literals only
            break;
        }
        let off = src.get(i..i + 2).ok_or_else(bad)?;
        let off = u16::from_le_bytes([off[0], off[1]]) as usize;
        i += 2;
        if off == 0 || off > out.len() {
            return Err(bad());
        }
        let mut len = (token & 15) as usize;
        if len == 15 {
            len = more_len(src, &mut i, len)?;
        }
        let len = (len + 4).min(want - out.len());
        let start = out.len() - off;
        if off >= len {
            out.extend_from_within(start..start + len);
        } else {
            // overlapping match: repeats the last `off` bytes
            for k in 0..len {
                out.push(out[start + k]);
            }
        }
    }
    if out.len() != want {
        return Err(invalid(format!(
            "lz4 data ends after {} of {} bytes",
            out.len(),
            want
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks() {
        // "abcd" literals, then a match of 8 at offset 4, then "xy"
        let block = [0x44, b'a', b'b', b'c', b'd', 4, 0, 0x20, b'x', b'y'];
        assert_eq!(decompress(&block, 14).unwrap(), b"abcdabcdabcdxy");
        // partial: stops in the middle of the match
        assert_eq!(decompress(&block, 6).unwrap(), b"abcdab");
        assert!(decompress(&block, 15).is_err());
        // offset before the start
        assert!(decompress(&[0x10, b'a', 2, 0], 6).is_err());
        // run of one byte (overlapping match)
        assert_eq!(
            decompress(&[0x1f, b'z', 1, 0, 6], 26).unwrap(),
            vec![b'z'; 26]
        );
        // long literal length
        let mut long = vec![0xf0, 5];
        long.extend(std::iter::repeat_n(b'q', 20));
        assert_eq!(decompress(&long, 20).unwrap(), vec![b'q'; 20]);
        assert!(decompress(&long[..10], 20).is_err());
    }
}
