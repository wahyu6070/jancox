//! Android sparse images (`simg`): random-access reading as the raw image
//! they stand for, and writing.
//!
//! ```text
//! header (28 bytes): magic 0xed26ff3a, version 1.0, header sizes,
//!     block size, total blocks, total chunks, checksum
//! chunks (12-byte header): RAW (data follows), FILL (4-byte pattern),
//!     DONT_CARE (no data, reads as zeros), CRC32 (ignored)
//! ```

use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::fs::invalid;

pub const MAGIC: u32 = 0xED26_FF3A;
const FILE_HEADER: usize = 28;
const CHUNK_HEADER: usize = 12;
const RAW: u16 = 0xCAC1;
const FILL: u16 = 0xCAC2;
const DONT_CARE: u16 = 0xCAC3;
const CRC32: u16 = 0xCAC4;

pub fn is_sparse(head: &[u8]) -> bool {
    head.len() >= 4 && u32::from_le_bytes(head[..4].try_into().unwrap()) == MAGIC
}

#[derive(Debug, Clone, Copy)]
enum Data {
    /// Bytes at this offset of the sparse file.
    Raw(u64),
    Fill([u8; 4]),
    Zero,
}

#[derive(Debug, Clone, Copy)]
struct Chunk {
    /// Offset in the raw image.
    start: u64,
    len: u64,
    data: Data,
}

/// Reads a sparse image as the raw image it describes.
pub struct SparseReader<R> {
    inner: R,
    chunks: Vec<Chunk>,
    size: u64,
    pos: u64,
    /// Index of the chunk holding `pos` (or the last one looked at).
    at: usize,
}

impl<R: Read + Seek> SparseReader<R> {
    pub fn new(mut inner: R) -> io::Result<Self> {
        inner.seek(SeekFrom::Start(0))?;
        let mut h = [0u8; FILE_HEADER];
        inner.read_exact(&mut h)?;
        let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        if u32_at(&h, 0) != MAGIC || u16_at(&h, 4) != 1 {
            return Err(invalid("not an Android sparse image (version 1)"));
        }
        let file_hdr = u16_at(&h, 8) as u64;
        let chunk_hdr = u16_at(&h, 10) as u64;
        let bs = u32_at(&h, 12) as u64;
        let total_blocks = u32_at(&h, 16) as u64;
        let total_chunks = u32_at(&h, 20);
        if file_hdr < FILE_HEADER as u64
            || chunk_hdr < CHUNK_HEADER as u64
            || bs == 0
            || !bs.is_multiple_of(4)
        {
            return Err(invalid("bad sparse image header"));
        }
        let mut off = file_hdr;
        let mut start = 0u64;
        let mut chunks = Vec::new();
        for _ in 0..total_chunks {
            inner.seek(SeekFrom::Start(off))?;
            let mut c = [0u8; CHUNK_HEADER];
            inner.read_exact(&mut c)?;
            let kind = u16_at(&c, 0);
            let blocks = u32_at(&c, 4) as u64;
            let total = u32_at(&c, 8) as u64;
            let body = total
                .checked_sub(chunk_hdr)
                .ok_or_else(|| invalid("bad sparse chunk size"))?;
            let len = blocks * bs;
            let data = match kind {
                RAW if body == len => Data::Raw(off + chunk_hdr),
                FILL if body == 4 => {
                    let mut p = [0u8; 4];
                    inner.seek(SeekFrom::Start(off + chunk_hdr))?;
                    inner.read_exact(&mut p)?;
                    if p == [0; 4] {
                        Data::Zero
                    } else {
                        Data::Fill(p)
                    }
                }
                DONT_CARE => Data::Zero,
                CRC32 => {
                    off += total;
                    continue;
                }
                _ => return Err(invalid(format!("bad sparse chunk type 0x{:x}", kind))),
            };
            if len > 0 {
                chunks.push(Chunk { start, len, data });
            }
            start += len;
            off += total;
        }
        if start != total_blocks * bs {
            return Err(invalid("sparse image chunks don't add up to its size"));
        }
        Ok(SparseReader {
            inner,
            chunks,
            size: start,
            pos: 0,
            at: 0,
        })
    }

    /// Size of the raw image.
    pub fn size(&self) -> u64 {
        self.size
    }

    fn chunk_at(&mut self, pos: u64) -> Option<Chunk> {
        let c = self.chunks.get(self.at)?;
        if !(c.start <= pos && pos < c.start + c.len) {
            self.at = self.chunks.partition_point(|c| c.start + c.len <= pos);
        }
        self.chunks.get(self.at).copied()
    }
}

impl<R: Read + Seek> Read for SparseReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.size {
            return Ok(0);
        }
        let Some(c) = self.chunk_at(self.pos) else {
            return Ok(0);
        };
        let within = self.pos - c.start;
        let n = (buf.len() as u64).min(c.len - within) as usize;
        match c.data {
            Data::Raw(off) => {
                self.inner.seek(SeekFrom::Start(off + within))?;
                self.inner.read_exact(&mut buf[..n])?;
            }
            Data::Zero => buf[..n].fill(0),
            Data::Fill(p) => {
                for (i, b) in buf[..n].iter_mut().enumerate() {
                    *b = p[((within as usize) + i) % 4];
                }
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for SparseReader<R> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.size.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        self.pos = pos;
        Ok(pos)
    }
}

/// What a range of the raw image holds, for [`write_sparse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    /// Data (all-zero blocks are written as zero fills).
    Data,
    /// Nothing that matters: DONT_CARE.
    Skip,
}

/// Chunks being written: merges runs of the same kind.
struct Out<'a, W> {
    w: &'a mut W,
    bs: u64,
    chunks: u32,
    kind: u16,
    blocks: u64,
    raw: Vec<u8>,
}

impl<W: Write> Out<'_, W> {
    fn flush(&mut self) -> io::Result<()> {
        if self.blocks == 0 {
            return Ok(());
        }
        let body: &[u8] = match self.kind {
            RAW => &self.raw,
            FILL => &[0, 0, 0, 0],
            _ => &[],
        };
        let mut h = [0u8; CHUNK_HEADER];
        h[..2].copy_from_slice(&self.kind.to_le_bytes());
        h[4..8].copy_from_slice(&(self.blocks as u32).to_le_bytes());
        h[8..12].copy_from_slice(&((CHUNK_HEADER + body.len()) as u32).to_le_bytes());
        self.w.write_all(&h)?;
        self.w.write_all(body)?;
        self.raw.clear();
        self.blocks = 0;
        self.chunks += 1;
        Ok(())
    }

    /// Adds `blocks` blocks of `kind` (`data` for RAW, one block).
    fn add(&mut self, kind: u16, blocks: u64, data: Option<&[u8]>) -> io::Result<()> {
        // RAW chunks hold at most 64 MiB so their sizes fit in u32
        let limit = if kind == RAW {
            (64 << 20) / self.bs
        } else {
            u32::MAX as u64
        };
        if self.blocks > 0 && (self.kind != kind || self.blocks + blocks > limit) {
            self.flush()?;
        }
        self.kind = kind;
        self.blocks += blocks;
        if let Some(d) = data {
            self.raw.extend_from_slice(d);
        }
        Ok(())
    }
}

/// Writes a sparse image of `size` bytes (a multiple of `bs`) to `out`.
/// `ranges` lists (start, len, kind) in order; gaps are DONT_CARE. `src`
/// fills a buffer with the raw image at an offset.
pub fn write_sparse<W: Write + Seek>(
    out: &mut W,
    size: u64,
    bs: u32,
    ranges: &[(u64, u64, Fill)],
    src: &mut dyn FnMut(u64, &mut [u8]) -> io::Result<()>,
) -> io::Result<()> {
    let bsz = bs as u64;
    if !size.is_multiple_of(bsz) {
        return Err(invalid("sparse image size must be whole blocks"));
    }
    let start = out.stream_position()?;
    out.write_all(&[0u8; FILE_HEADER])?;
    let mut o = Out {
        w: out,
        bs: bsz,
        chunks: 0,
        kind: RAW,
        blocks: 0,
        raw: Vec::new(),
    };
    let mut buf = vec![0u8; bs as usize];
    let mut pos = 0u64;
    for &(rstart, rlen, kind) in ranges {
        if rstart % bsz != 0 || rlen % bsz != 0 || rstart < pos || rstart + rlen > size {
            return Err(invalid("bad sparse range"));
        }
        if rstart > pos {
            o.add(DONT_CARE, (rstart - pos) / bsz, None)?;
            pos = rstart;
        }
        let end = rstart + rlen;
        if kind == Fill::Skip {
            o.add(DONT_CARE, rlen / bsz, None)?;
            pos = end;
            continue;
        }
        while pos < end {
            src(pos, &mut buf)?;
            if buf.iter().all(|&b| b == 0) {
                o.add(FILL, 1, None)?;
            } else {
                o.add(RAW, 1, Some(&buf))?;
            }
            pos += bsz;
        }
    }
    if size > pos {
        o.add(DONT_CARE, (size - pos) / bsz, None)?;
    }
    o.flush()?;
    let chunks = o.chunks;
    let end = out.stream_position()?;
    let mut h = [0u8; FILE_HEADER];
    h[..4].copy_from_slice(&MAGIC.to_le_bytes());
    h[4..6].copy_from_slice(&1u16.to_le_bytes());
    h[8..10].copy_from_slice(&(FILE_HEADER as u16).to_le_bytes());
    h[10..12].copy_from_slice(&(CHUNK_HEADER as u16).to_le_bytes());
    h[12..16].copy_from_slice(&bs.to_le_bytes());
    h[16..20].copy_from_slice(&((size / bsz) as u32).to_le_bytes());
    h[20..24].copy_from_slice(&chunks.to_le_bytes());
    out.seek(SeekFrom::Start(start))?;
    out.write_all(&h)?;
    out.seek(SeekFrom::Start(end))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn write_then_read() {
        let bs = 4096u32;
        let size = 40 * 4096u64;
        let mut raw = vec![0u8; size as usize];
        for (i, b) in raw[4096..3 * 4096].iter_mut().enumerate() {
            *b = (i % 251) as u8 + 1;
        }
        raw[20 * 4096..21 * 4096].fill(7);
        let ranges = [
            (0, 8 * 4096, Fill::Data),
            (8 * 4096, 4 * 4096, Fill::Skip),
            (20 * 4096, 2 * 4096, Fill::Data),
        ];
        let mut out = Cursor::new(Vec::new());
        let src = raw.clone();
        write_sparse(&mut out, size, bs, &ranges, &mut |pos, buf| {
            buf.copy_from_slice(&src[pos as usize..pos as usize + buf.len()]);
            Ok(())
        })
        .unwrap();
        let bytes = out.into_inner();
        assert!(is_sparse(&bytes));
        assert!(bytes.len() < 4 * 4096);
        let mut r = SparseReader::new(Cursor::new(bytes)).unwrap();
        assert_eq!(r.size(), size);
        let mut back = Vec::new();
        r.read_to_end(&mut back).unwrap();
        assert_eq!(back, raw);
        r.seek(SeekFrom::Start(20 * 4096 + 5)).unwrap();
        let mut b = [0u8; 3];
        r.read_exact(&mut b).unwrap();
        assert_eq!(b, [7, 7, 7]);
        assert!(SparseReader::new(Cursor::new(vec![0u8; 64])).is_err());
    }
}
