//! LZ4 block format (no frame), as EROFS stores it. The decoder can stop
//! after the first `want` bytes, like `LZ4_decompress_safe_partial`, which
//! EROFS needs for extents that use only the start of a pcluster. The
//! encoder fills a fixed output size with as much input as fits, like
//! `LZ4_compress_destSize`, which EROFS pclusters need.

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

const MINMATCH: usize = 4;
/// The last 5 bytes of the input are always literals.
const LASTLITERALS: usize = 5;
/// A match may not start in the last 12 bytes of the input.
const MFLIMIT: usize = 12;
const HASH_BITS: u32 = 16;
const WINDOW: usize = 1 << 16;

/// Compresses the start of inputs into a fixed-size output. Keeps its
/// tables between calls.
pub struct Encoder {
    /// hash -> last position (+ 1; 0 = empty) of a 4-byte sequence
    head: Vec<u32>,
    /// position -> previous position with the same hash (ring of 64 KiB)
    chain: Vec<u16>,
    /// search depth of the hash chains (more = smaller, slower)
    depth: usize,
}

fn hash(b: &[u8]) -> usize {
    let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    (v.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

/// Bytes a length of `n` needs beyond the 4 bits in the token.
fn len_bytes(n: usize) -> usize {
    if n >= 15 {
        (n - 15) / 255 + 1
    } else {
        0
    }
}

fn put_len(out: &mut Vec<u8>, mut n: usize) {
    if n >= 15 {
        n -= 15;
        while n >= 255 {
            out.push(255);
            n -= 255;
        }
        out.push(n as u8);
    }
}

impl Encoder {
    pub fn new(depth: usize) -> Self {
        Encoder {
            head: vec![0; 1 << HASH_BITS],
            chain: vec![0; WINDOW],
            depth: depth.max(1),
        }
    }

    /// Compresses a prefix of `src` into at most `cap` bytes. Returns the
    /// compressed block and how many bytes of `src` it holds (0 when not
    /// even one literal fits). The block decodes on its own.
    pub fn compress_dest(&mut self, src: &[u8], cap: usize) -> (Vec<u8>, usize) {
        self.head.fill(0);
        let mut out = Vec::with_capacity(cap);
        let iend = src.len();
        let mflimit = iend.saturating_sub(MFLIMIT);
        let matchlimit = iend.saturating_sub(LASTLITERALS);
        let mut anchor = 0;
        let mut ip = 0;
        let mut next_insert = 0;
        while ip < mflimit {
            // index positions up to ip
            while next_insert <= ip {
                let h = hash(&src[next_insert..]);
                let prev = self.head[h];
                let dist = next_insert + 1 - prev as usize;
                self.chain[next_insert & (WINDOW - 1)] = if prev == 0 || dist >= WINDOW {
                    0
                } else {
                    dist as u16
                };
                self.head[h] = next_insert as u32 + 1;
                next_insert += 1;
            }
            // longest match among the chain
            let (mut best_len, mut best_pos) = (0usize, 0usize);
            // the head is ip itself: follow its links
            let mut depth = self.depth;
            let mut pos = ip;
            loop {
                let step = self.chain[pos & (WINDOW - 1)] as usize;
                // (a stale link of the ring can point anywhere: checked)
                if step == 0 || depth == 0 || step > pos {
                    break;
                }
                depth -= 1;
                pos -= step;
                if ip - pos >= WINDOW {
                    break;
                }
                if src[pos..pos + MINMATCH] == src[ip..ip + MINMATCH] {
                    let mut l = MINMATCH;
                    while ip + l < matchlimit && src[pos + l] == src[ip + l] {
                        l += 1;
                    }
                    if l > best_len {
                        best_len = l;
                        best_pos = pos;
                    }
                }
            }
            if best_len < MINMATCH {
                ip += 1;
                continue;
            }
            // extend backwards into the literals
            let (mut mstart, mut mref) = (ip, best_pos);
            while mstart > anchor && mref > 0 && src[mstart - 1] == src[mref - 1] {
                mstart -= 1;
                mref -= 1;
                best_len += 1;
            }
            let lit = mstart - anchor;
            let cost = 1 + len_bytes(lit) + lit + 2 + len_bytes(best_len - MINMATCH);
            // keep room for a final token and 8 literals, so the block ends
            // as full (non-partial) decoders require: the last match starts
            // 12 bytes or more before the end and ends 5 or more before it
            if out.len() + cost + 1 + (MFLIMIT - MINMATCH) > cap {
                break;
            }
            let ml = best_len - MINMATCH;
            out.push(((lit.min(15) as u8) << 4) | ml.min(15) as u8);
            put_len(&mut out, lit);
            out.extend_from_slice(&src[anchor..mstart]);
            out.extend_from_slice(&((mstart - mref) as u16).to_le_bytes());
            put_len(&mut out, ml);
            ip = mstart + best_len;
            anchor = ip;
        }
        // last literals: as many as fit
        let avail = cap.saturating_sub(out.len());
        if avail == 0 {
            return (out, anchor);
        }
        let mut run = (iend - anchor).min(avail - 1);
        while run > 0 && 1 + len_bytes(run) + run > avail {
            run -= 1;
        }
        out.push((run.min(15) as u8) << 4);
        put_len(&mut out, run);
        out.extend_from_slice(&src[anchor..anchor + run]);
        (out, anchor + run)
    }
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

    /// The rules of LZ4_decompress_safe (full decoding): the last sequence
    /// is literals only; no match starts in the last 12 bytes of the output
    /// or ends in its last 5.
    fn check_full_rules(block: &[u8], out_len: usize) {
        let (mut i, mut o) = (0usize, 0usize);
        loop {
            let token = block[i];
            i += 1;
            let mut lit = (token >> 4) as usize;
            if lit == 15 {
                lit = more_len(block, &mut i, lit).unwrap();
            }
            i += lit;
            o += lit;
            if i == block.len() {
                assert_eq!(o, out_len);
                return;
            }
            assert!(o + 12 <= out_len, "match starts at {} of {}", o, out_len);
            i += 2;
            let mut ml = (token & 15) as usize;
            if ml == 15 {
                ml = more_len(block, &mut i, ml).unwrap();
            }
            o += ml + 4;
            assert!(o + 5 <= out_len, "match ends at {} of {}", o, out_len);
        }
    }

    #[test]
    fn dest_size_round_trip() {
        let mut enc = Encoder::new(16);
        let text: Vec<u8> = b"jancox erofs lz4 android system vendor\n"
            .iter()
            .copied()
            .cycle()
            .take(200_000)
            .collect();
        let mut x = 7u32;
        let noise: Vec<u8> = (0..20_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        for src in [
            text.clone(),
            noise.clone(),
            vec![0u8; 100_000],
            [&text[..9000], &noise[..5000]].concat(),
            b"tiny".to_vec(),
        ] {
            for cap in [4096usize, 512, 20] {
                let (out, used) = enc.compress_dest(&src, cap);
                assert!(out.len() <= cap);
                assert!(used <= src.len());
                assert_eq!(decompress(&out, used).unwrap(), &src[..used]);
                if used > 0 {
                    check_full_rules(&out, used);
                }
            }
        }
        // compressible data fills the block with much more input
        let (_, used) = enc.compress_dest(&text, 4096);
        assert!(used > 30_000, "{}", used);
        let (_, used) = enc.compress_dest(&noise, 4096);
        assert!(used > 3900 && used < 4096, "{}", used);
    }
}
