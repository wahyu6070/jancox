//! Read-only EROFS reader for Android images: compact and extended inodes,
//! flat (plain and inline tail) and chunk-based files, directories, and
//! xattrs (inline, shared, long name prefixes).
//!
//! Compressed files (lz4, lzma, ...) are not supported yet: reading one
//! fails with an error naming the file layout.

use std::io::{self, Read, Seek, SeekFrom, Write};

use super::ext4::uuid;
use super::{invalid, parse_capability, Filesystem, Kind, Meta};

pub const SUPERBLOCK_OFFSET: u64 = 1024;
pub const EROFS_MAGIC: u32 = 0xE0F5_E1E2;
const NULL_ADDR: u32 = u32::MAX;

// feature_compat
pub const COMPAT_SB_CHKSUM: u32 = 0x1;
pub const COMPAT_MTIME: u32 = 0x2;
pub const COMPAT_XATTR_FILTER: u32 = 0x4;

// feature_incompat
const INCOMPAT_ZERO_PADDING: u32 = 0x1;
const INCOMPAT_COMPR_CFGS: u32 = 0x2;
const INCOMPAT_CHUNKED_FILE: u32 = 0x4;
const INCOMPAT_DEVICE_TABLE: u32 = 0x8;
const INCOMPAT_ZTAILPACKING: u32 = 0x10;
const INCOMPAT_FRAGMENTS: u32 = 0x20;
const INCOMPAT_XATTR_PREFIXES: u32 = 0x40;
const INCOMPAT_48BIT: u32 = 0x80;
const INCOMPAT_METABOX: u32 = 0x100;
/// Features that change how the metadata we read is laid out. The
/// compression ones only matter for compressed files, which are refused
/// one by one.
const INCOMPAT_SUPPORTED: u32 = INCOMPAT_ZERO_PADDING
    | INCOMPAT_COMPR_CFGS
    | INCOMPAT_CHUNKED_FILE
    | INCOMPAT_ZTAILPACKING
    | INCOMPAT_FRAGMENTS
    | INCOMPAT_XATTR_PREFIXES;

const FEATURE_NAMES: &[(bool, u32, &str)] = &[
    (false, COMPAT_SB_CHKSUM, "sb_csum"),
    (false, COMPAT_MTIME, "mtime"),
    (false, COMPAT_XATTR_FILTER, "xattr_filter"),
    (true, INCOMPAT_ZERO_PADDING, "0padding"),
    (true, INCOMPAT_COMPR_CFGS, "compr_cfgs"),
    (true, INCOMPAT_CHUNKED_FILE, "chunked_file"),
    (true, INCOMPAT_DEVICE_TABLE, "device_table"),
    (true, INCOMPAT_ZTAILPACKING, "ztailpacking"),
    (true, INCOMPAT_FRAGMENTS, "fragments"),
    (true, INCOMPAT_XATTR_PREFIXES, "xattr_prefixes"),
    (true, INCOMPAT_48BIT, "48bit"),
    (true, INCOMPAT_METABOX, "metabox"),
];

// i_format data layouts
pub const LAYOUT_FLAT_PLAIN: u16 = 0;
const LAYOUT_COMPRESSED_FULL: u16 = 1;
pub const LAYOUT_FLAT_INLINE: u16 = 2;
const LAYOUT_COMPRESSED_COMPACT: u16 = 3;
const LAYOUT_CHUNK_BASED: u16 = 4;

const CHUNK_FORMAT_BLKBITS_MASK: u16 = 0x1F;
const CHUNK_FORMAT_INDEXES: u16 = 0x20;

pub const XATTR_INDEX_SECURITY: u8 = 6;
const XATTR_LONG_PREFIX: u8 = 0x80;
/// Name prefixes of the short xattr indexes.
const XATTR_PREFIXES: [&str; 7] = [
    "",
    "user.",
    "system.posix_acl_access",
    "system.posix_acl_default",
    "trusted.",
    "lustre.",
    "security.",
];

fn le16(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn le32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

fn le64(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// True when `head` (at least 1028 bytes of an image) has the EROFS magic.
pub fn is_erofs(head: &[u8]) -> bool {
    head.len() >= 1028 && le32(head, 1024) == EROFS_MAGIC
}

#[derive(Debug, Clone)]
struct Superblock {
    raw: Vec<u8>,
    block_size: u64,
    root_nid: u64,
    inodes: u64,
    build_time: u64,
    blocks: u64,
    meta_blkaddr: u64,
    xattr_blkaddr: u64,
    compat: u32,
    incompat: u32,
}

#[derive(Debug, Clone)]
struct Inode {
    nid: u64,
    layout: u16,
    /// Bytes of the on-disk inode (32 or 64).
    isize: u64,
    /// Bytes of the inline xattr area after the inode.
    xattr_isize: u64,
    xattr_icount: u16,
    mode: u16,
    size: u64,
    /// raw_blkaddr, rdev or chunk format, by layout and type.
    u: u32,
    uid: u32,
    gid: u32,
    mtime: i64,
}

impl Inode {
    fn kind(&self) -> io::Result<Kind> {
        Ok(match self.mode & 0xF000 {
            0x4000 => Kind::Dir,
            0x8000 => Kind::File,
            0xA000 => Kind::Symlink,
            0x2000 => Kind::CharDevice,
            0x6000 => Kind::BlockDevice,
            0x1000 => Kind::Fifo,
            0xC000 => Kind::Socket,
            m => {
                return Err(invalid(format!(
                    "inode {}: unknown file type 0x{:x}",
                    self.nid, m
                )))
            }
        })
    }
}

/// A contiguous piece of a file: `len` bytes at `physical`, or zeros.
#[derive(Debug, Clone, Copy)]
struct Piece {
    physical: Option<u64>,
    len: u64,
}

pub struct Erofs<R> {
    r: R,
    sb: Superblock,
    /// Long xattr name prefixes: (base index, infix).
    long_prefixes: Vec<(u8, Vec<u8>)>,
}

impl<R: Read + Seek> Erofs<R> {
    pub fn open(mut r: R) -> io::Result<Self> {
        let mut raw = vec![0u8; 128];
        r.seek(SeekFrom::Start(SUPERBLOCK_OFFSET))?;
        r.read_exact(&mut raw)?;
        if le32(&raw, 0) != EROFS_MAGIC {
            return Err(invalid("not an EROFS image (bad superblock magic)"));
        }
        let blkszbits = raw[12];
        if !(9..=16).contains(&blkszbits) {
            return Err(invalid(format!("bad EROFS block size (log {})", blkszbits)));
        }
        let incompat = le32(&raw, 80);
        let unsupported = incompat & !INCOMPAT_SUPPORTED;
        if unsupported != 0 {
            return Err(invalid(format!(
                "unsupported EROFS features: {}",
                feature_list(true, unsupported)
            )));
        }
        let sb = Superblock {
            block_size: 1 << blkszbits,
            root_nid: le16(&raw, 14) as u64,
            inodes: le64(&raw, 16),
            build_time: le64(&raw, 24),
            blocks: le32(&raw, 36) as u64,
            meta_blkaddr: le32(&raw, 40) as u64,
            xattr_blkaddr: le32(&raw, 44) as u64,
            compat: le32(&raw, 8),
            incompat,
            raw,
        };
        let mut fs = Erofs {
            r,
            sb,
            long_prefixes: Vec::new(),
        };
        fs.read_long_prefixes()?;
        Ok(fs)
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.r.seek(SeekFrom::Start(offset))?;
        self.r.read_exact(buf)
    }

    fn read_vec(&mut self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len as usize];
        self.read_at(offset, &mut buf)?;
        Ok(buf)
    }

    /// Long xattr name prefixes, stored at `xattr_prefix_start * 4` as
    /// (u16 size, u8 base index, infix), each 4-byte aligned.
    fn read_long_prefixes(&mut self) -> io::Result<()> {
        let count = self.sb.raw[91] as usize;
        if self.sb.incompat & INCOMPAT_XATTR_PREFIXES == 0 || count == 0 {
            return Ok(());
        }
        if le64(&self.sb.raw, 96) != 0 {
            return Err(invalid(
                "EROFS long xattr prefixes in a packed inode are not supported yet",
            ));
        }
        let mut pos = (le32(&self.sb.raw, 92) as u64) << 2;
        for _ in 0..count {
            let mut len = [0u8; 2];
            self.read_at(pos, &mut len)?;
            let len = u16::from_le_bytes(len) as u64;
            if len == 0 {
                return Err(invalid("bad EROFS long xattr prefix"));
            }
            let p = self.read_vec(pos + 2, len)?;
            self.long_prefixes.push((p[0], p[1..].to_vec()));
            pos += (2 + len).next_multiple_of(4);
        }
        Ok(())
    }

    fn inode_offset(&self, nid: u64) -> u64 {
        self.sb.meta_blkaddr * self.sb.block_size + nid * 32
    }

    fn inode(&mut self, nid: u64) -> io::Result<Inode> {
        if nid * 32 >= self.sb.blocks * self.sb.block_size {
            return Err(invalid(format!("inode {} is outside the filesystem", nid)));
        }
        let mut b = [0u8; 64];
        let off = self.inode_offset(nid);
        self.read_at(off, &mut b[..32])?;
        let format = le16(&b, 0);
        let layout = (format >> 1) & 0x7;
        let xattr_icount = le16(&b, 2);
        let xattr_isize = if xattr_icount == 0 {
            0
        } else {
            12 + (xattr_icount as u64 - 1) * 4
        };
        let base = Inode {
            nid,
            layout,
            isize: 32,
            xattr_isize,
            xattr_icount,
            mode: le16(&b, 4),
            size: 0,
            u: le32(&b, 16),
            uid: 0,
            gid: 0,
            mtime: 0,
        };
        Ok(match format & 1 {
            0 => {
                let rel = if self.sb.compat & COMPAT_MTIME != 0 {
                    le32(&b, 12) as u64
                } else {
                    0
                };
                Inode {
                    size: le32(&b, 8) as u64,
                    uid: le16(&b, 24) as u32,
                    gid: le16(&b, 26) as u32,
                    mtime: (self.sb.build_time + rel) as i64,
                    ..base
                }
            }
            _ => {
                self.read_at(off + 32, &mut b[32..])?;
                Inode {
                    isize: 64,
                    size: le64(&b, 8),
                    uid: le32(&b, 24),
                    gid: le32(&b, 28),
                    mtime: le64(&b, 32) as i64,
                    ..base
                }
            }
        })
    }

    /// Byte offset right after the inode and its inline xattrs.
    fn after_inode(&self, inode: &Inode) -> u64 {
        self.inode_offset(inode.nid) + inode.isize + inode.xattr_isize
    }

    /// Where the bytes of a file, directory or symlink are, in order.
    fn pieces(&mut self, inode: &Inode) -> io::Result<Vec<Piece>> {
        let bs = self.sb.block_size;
        let size = inode.size;
        match inode.layout {
            LAYOUT_FLAT_PLAIN | LAYOUT_FLAT_INLINE => {
                let nblocks = size.div_ceil(bs);
                let tail = inode.layout == LAYOUT_FLAT_INLINE && size > 0;
                let lastblk = nblocks - tail as u64;
                let mut out = Vec::new();
                let body = (lastblk * bs).min(size);
                if body > 0 {
                    out.push(Piece {
                        physical: (inode.u != NULL_ADDR).then(|| inode.u as u64 * bs),
                        len: body,
                    });
                }
                if tail {
                    let start = self.after_inode(inode);
                    let len = size - body;
                    if start % bs + len > bs {
                        return Err(invalid(format!(
                            "inode {}: inline tail crosses a block boundary",
                            inode.nid
                        )));
                    }
                    out.push(Piece {
                        physical: Some(start),
                        len,
                    });
                }
                Ok(out)
            }
            LAYOUT_CHUNK_BASED => {
                let format = inode.u as u16;
                let chunk = bs << (format & CHUNK_FORMAT_BLKBITS_MASK);
                let unit = if format & CHUNK_FORMAT_INDEXES != 0 {
                    8
                } else {
                    4
                };
                let chunks = size.div_ceil(chunk);
                let start = self.after_inode(inode).next_multiple_of(unit);
                let table = self.read_vec(start, chunks * unit)?;
                let mut out = Vec::new();
                for i in 0..chunks {
                    let e = (i * unit) as usize;
                    let (dev, addr) = if unit == 8 {
                        (le16(&table, e + 2), le32(&table, e + 4))
                    } else {
                        (0, le32(&table, e))
                    };
                    if dev != 0 {
                        return Err(invalid(format!(
                            "inode {}: chunks on extra devices are not supported",
                            inode.nid
                        )));
                    }
                    out.push(Piece {
                        physical: (addr != NULL_ADDR).then(|| addr as u64 * bs),
                        len: chunk.min(size - i * chunk),
                    });
                }
                Ok(out)
            }
            LAYOUT_COMPRESSED_FULL | LAYOUT_COMPRESSED_COMPACT => Err(invalid(format!(
                "inode {}: compressed EROFS files are not supported yet",
                inode.nid
            ))),
            l => Err(invalid(format!(
                "inode {}: unknown data layout {}",
                inode.nid, l
            ))),
        }
    }

    fn copy_data(&mut self, inode: &Inode, out: &mut dyn Write) -> io::Result<u64> {
        let pieces = self.pieces(inode)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0;
        for p in pieces {
            let mut left = p.len;
            if let Some(phys) = p.physical {
                self.r.seek(SeekFrom::Start(phys))?;
            }
            while left > 0 {
                let n = left.min(buf.len() as u64) as usize;
                match p.physical {
                    Some(_) => self.r.read_exact(&mut buf[..n])?,
                    None => buf[..n].fill(0),
                }
                out.write_all(&buf[..n])?;
                left -= n as u64;
            }
            total += p.len;
        }
        Ok(total)
    }

    fn data(&mut self, inode: &Inode) -> io::Result<Vec<u8>> {
        let mut v = Vec::with_capacity(inode.size as usize);
        self.copy_data(inode, &mut v)?;
        Ok(v)
    }

    /// Full name of an xattr entry's index + name.
    fn xattr_name(&self, index: u8, name: &[u8]) -> Option<String> {
        let (prefix, infix): (&str, &[u8]) = if index & XATTR_LONG_PREFIX != 0 {
            let (base, infix) = self.long_prefixes.get((index & 0x7F) as usize)?;
            (XATTR_PREFIXES.get(*base as usize)?, infix)
        } else {
            (XATTR_PREFIXES.get(index as usize)?, &[])
        };
        let mut s = prefix.to_string();
        s.push_str(&String::from_utf8_lossy(infix));
        s.push_str(&String::from_utf8_lossy(name));
        Some(s)
    }

    /// Parses one entry at the start of `b`: (entry size, name, value).
    fn xattr_entry(&self, b: &[u8]) -> io::Result<(usize, Option<String>, Vec<u8>)> {
        if b.len() < 4 {
            return Err(invalid("truncated EROFS xattr entry"));
        }
        let name_len = b[0] as usize;
        let value_len = le16(b, 2) as usize;
        let end = 4 + name_len + value_len;
        if end > b.len() {
            return Err(invalid("truncated EROFS xattr entry"));
        }
        let name = self.xattr_name(b[1], &b[4..4 + name_len]);
        Ok((end.next_multiple_of(4), name, b[4 + name_len..end].to_vec()))
    }

    /// All xattrs of an inode as (full name, value).
    fn xattrs(&mut self, inode: &Inode) -> io::Result<Vec<(String, Vec<u8>)>> {
        let mut out = Vec::new();
        if inode.xattr_icount == 0 {
            return Ok(out);
        }
        let start = self.inode_offset(inode.nid) + inode.isize;
        let area = self.read_vec(start, inode.xattr_isize)?;
        let shared = area[4] as usize;
        let mut pos = 12 + shared * 4;
        if pos > area.len() {
            return Err(invalid(format!("inode {}: bad xattr header", inode.nid)));
        }
        for i in 0..shared {
            let id = le32(&area, 12 + i * 4) as u64;
            let at = self.sb.xattr_blkaddr * self.sb.block_size + id * 4;
            let mut head = [0u8; 4];
            self.read_at(at, &mut head)?;
            let len = 4 + head[0] as u64 + le16(&head, 2) as u64;
            let entry = self.read_vec(at, len)?;
            if let (_, Some(n), v) = self.xattr_entry(&entry)? {
                out.push((n, v));
            }
        }
        while pos + 4 <= area.len() {
            let (len, name, value) = self.xattr_entry(&area[pos..])?;
            if let Some(n) = name {
                out.push((n, value));
            }
            pos += len;
        }
        Ok(out)
    }
}

fn feature_list(incompat: bool, bits: u32) -> String {
    let mut names = Vec::new();
    for bit in 0..32 {
        let b = 1u32 << bit;
        if bits & b == 0 {
            continue;
        }
        match FEATURE_NAMES
            .iter()
            .find(|(i, f, _)| *i == incompat && *f == b)
        {
            Some((_, _, n)) => names.push(n.to_string()),
            None => names.push(format!("0x{:x}", b)),
        }
    }
    names.join(" ")
}

impl<R: Read + Seek> Filesystem for Erofs<R> {
    type Node = u64;

    fn fs_type(&self) -> &'static str {
        "erofs"
    }

    fn root(&self) -> u64 {
        self.sb.root_nid
    }

    fn meta(&mut self, nid: u64) -> io::Result<Meta> {
        let inode = self.inode(nid)?;
        let kind = inode.kind()?;
        let mut selinux = None;
        let mut capabilities = None;
        for (name, value) in self.xattrs(&inode)? {
            match name.as_str() {
                "security.selinux" => {
                    let end = value.iter().position(|&c| c == 0).unwrap_or(value.len());
                    selinux = Some(String::from_utf8_lossy(&value[..end]).into_owned());
                }
                "security.capability" => capabilities = parse_capability(&value),
                _ => {}
            }
        }
        Ok(Meta {
            kind,
            mode: (inode.mode & 0o7777) as u32,
            uid: inode.uid,
            gid: inode.gid,
            mtime: inode.mtime,
            size: inode.size,
            selinux,
            capabilities,
        })
    }

    fn read_dir(&mut self, nid: u64) -> io::Result<Vec<(Vec<u8>, u64)>> {
        let inode = self.inode(nid)?;
        if inode.kind()? != Kind::Dir {
            return Err(invalid(format!("inode {} is not a directory", nid)));
        }
        let data = self.data(&inode)?;
        let bs = self.sb.block_size as usize;
        let mut out = Vec::new();
        for block in data.chunks(bs) {
            if block.len() < 12 {
                return Err(invalid(format!("inode {}: truncated directory", nid)));
            }
            let first = le16(block, 8) as usize;
            if first < 12 || !first.is_multiple_of(12) || first > block.len() {
                return Err(invalid(format!("inode {}: bad directory block", nid)));
            }
            let count = first / 12;
            for i in 0..count {
                let e = i * 12;
                let child = le64(block, e);
                let start = le16(block, e + 8) as usize;
                let end = if i + 1 < count {
                    le16(block, e + 12 + 8) as usize
                } else {
                    // the last name runs to the end of the block, NUL padded
                    let rest = block.get(start..).unwrap_or(&[]);
                    start + rest.iter().position(|&c| c == 0).unwrap_or(rest.len())
                };
                if start > end || end > block.len() {
                    return Err(invalid(format!("inode {}: bad directory entry", nid)));
                }
                let name = &block[start..end];
                if name == b"." || name == b".." {
                    continue;
                }
                out.push((name.to_vec(), child));
            }
        }
        Ok(out)
    }

    fn read_file(&mut self, nid: u64, out: &mut dyn Write) -> io::Result<u64> {
        let inode = self.inode(nid)?;
        self.copy_data(&inode, out)
    }

    fn read_link(&mut self, nid: u64) -> io::Result<Vec<u8>> {
        let inode = self.inode(nid)?;
        self.data(&inode)
    }

    fn volume_name(&self) -> String {
        cstr(&self.sb.raw[64..80])
    }

    fn info(&self) -> Vec<(String, String)> {
        let raw = &self.sb.raw;
        let features = [
            feature_list(false, self.sb.compat),
            feature_list(true, self.sb.incompat),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
        [
            ("fs_type", "erofs".to_string()),
            ("volume_name", self.volume_name()),
            ("uuid", uuid(&raw[48..64])),
            ("block_size", self.sb.block_size.to_string()),
            ("blocks", self.sb.blocks.to_string()),
            ("inodes", self.sb.inodes.to_string()),
            ("created", self.sb.build_time.to_string()),
            ("created_nsec", le32(raw, 32).to_string()),
            ("features", features),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }
}
