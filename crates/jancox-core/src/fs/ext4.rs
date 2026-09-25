//! Read-only ext2/3/4 reader for Android images: extents and legacy block
//! maps, htree directories (read linearly), inline data, fast and slow
//! symlinks, and xattrs (in-inode, xattr block and EA inodes).

use std::io::{self, Read, Seek, SeekFrom, Write};

use super::{invalid, parse_capability, Filesystem, Kind, Meta};

const SUPERBLOCK_OFFSET: u64 = 1024;
const EXT4_MAGIC: u16 = 0xEF53;
const ROOT_INO: u32 = 2;

// s_feature_incompat
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_RECOVER: u32 = 0x4;
const INCOMPAT_JOURNAL_DEV: u32 = 0x8;
const INCOMPAT_META_BG: u32 = 0x10;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_MMP: u32 = 0x100;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const INCOMPAT_EA_INODE: u32 = 0x400;
const INCOMPAT_DIRDATA: u32 = 0x1000;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_LARGEDIR: u32 = 0x4000;
const INCOMPAT_INLINE_DATA: u32 = 0x8000;
const INCOMPAT_ENCRYPT: u32 = 0x10000;
const INCOMPAT_CASEFOLD: u32 = 0x20000;
const INCOMPAT_SUPPORTED: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_RECOVER
    | INCOMPAT_EXTENTS
    | INCOMPAT_64BIT
    | INCOMPAT_MMP
    | INCOMPAT_FLEX_BG
    | INCOMPAT_EA_INODE
    | INCOMPAT_CSUM_SEED
    | INCOMPAT_LARGEDIR
    | INCOMPAT_INLINE_DATA
    | INCOMPAT_ENCRYPT
    | INCOMPAT_CASEFOLD;

// i_flags
const FLAG_HUGE_FILE: u32 = 0x40000;
const FLAG_EXTENTS: u32 = 0x80000;
const FLAG_INLINE_DATA: u32 = 0x1000_0000;
const FLAG_ENCRYPT: u32 = 0x800;

const EXTENT_MAGIC: u16 = 0xF30A;
const XATTR_MAGIC: u32 = 0xEA02_0000;

/// Feature names as printed by dumpe2fs, by (field, bit).
const FEATURE_NAMES: &[(u8, u32, &str)] = &[
    (0, 0x1, "dir_prealloc"),
    (0, 0x2, "imagic_inodes"),
    (0, 0x4, "has_journal"),
    (0, 0x8, "ext_attr"),
    (0, 0x10, "resize_inode"),
    (0, 0x20, "dir_index"),
    (0, 0x200, "sparse_super2"),
    (0, 0x400, "fast_commit"),
    (0, 0x800, "stable_inodes"),
    (0, 0x1000, "orphan_file"),
    (1, INCOMPAT_FILETYPE, "filetype"),
    (1, INCOMPAT_RECOVER, "needs_recovery"),
    (1, INCOMPAT_JOURNAL_DEV, "journal_dev"),
    (1, INCOMPAT_META_BG, "meta_bg"),
    (1, INCOMPAT_EXTENTS, "extent"),
    (1, INCOMPAT_64BIT, "64bit"),
    (1, INCOMPAT_MMP, "mmp"),
    (1, INCOMPAT_FLEX_BG, "flex_bg"),
    (1, INCOMPAT_EA_INODE, "ea_inode"),
    (1, INCOMPAT_DIRDATA, "dirdata"),
    (1, INCOMPAT_CSUM_SEED, "metadata_csum_seed"),
    (1, INCOMPAT_LARGEDIR, "large_dir"),
    (1, INCOMPAT_INLINE_DATA, "inline_data"),
    (1, INCOMPAT_ENCRYPT, "encrypt"),
    (1, INCOMPAT_CASEFOLD, "casefold"),
    (2, 0x1, "sparse_super"),
    (2, 0x2, "large_file"),
    (2, 0x8, "huge_file"),
    (2, 0x10, "uninit_bg"),
    (2, 0x20, "dir_nlink"),
    (2, 0x40, "extra_isize"),
    (2, 0x100, "quota"),
    (2, 0x200, "bigalloc"),
    (2, 0x400, "metadata_csum"),
    (2, 0x1000, "read-only"),
    (2, 0x2000, "project"),
    (2, 0x4000, "shared_blocks"),
    (2, 0x8000, "verity"),
    (2, 0x10000, "orphan_present"),
];

fn le16(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn le32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn uuid(b: &[u8]) -> String {
    let h: Vec<String> = b.iter().map(|x| format!("{:02x}", x)).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].concat(),
        h[4..6].concat(),
        h[6..8].concat(),
        h[8..10].concat(),
        h[10..16].concat()
    )
}

#[derive(Debug, Clone)]
struct Superblock {
    raw: Vec<u8>,
    block_size: u64,
    inodes_count: u32,
    blocks_count: u64,
    inodes_per_group: u32,
    inode_size: u64,
    first_data_block: u64,
    desc_size: u64,
    compat: u32,
    incompat: u32,
    ro_compat: u32,
}

#[derive(Debug, Clone)]
struct Inode {
    mode: u16,
    uid: u32,
    gid: u32,
    size: u64,
    mtime: i64,
    flags: u32,
    /// `i_blocks` in 512-byte sectors.
    sectors: u64,
    block: [u8; 60],
    file_acl: u64,
    /// In-inode xattr area (after `i_extra_isize`), may be empty.
    xattr_area: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
struct Extent {
    logical: u64,
    physical: u64,
    len: u64,
    /// Allocated but not written: reads as zeros.
    uninit: bool,
}

pub struct Ext4<R> {
    r: R,
    sb: Superblock,
    /// First block of each group's inode table.
    inode_tables: Vec<u64>,
}

/// True when `head` (at least 1024 + 58 bytes of an image) has the ext
/// superblock magic.
pub fn is_ext4(head: &[u8]) -> bool {
    head.len() >= 1024 + 0x3A && le16(head, 1024 + 0x38) == EXT4_MAGIC
}

impl<R: Read + Seek> Ext4<R> {
    pub fn open(mut r: R) -> io::Result<Self> {
        let mut raw = vec![0u8; 1024];
        r.seek(SeekFrom::Start(SUPERBLOCK_OFFSET))?;
        r.read_exact(&mut raw)?;
        if le16(&raw, 0x38) != EXT4_MAGIC {
            return Err(invalid("not an ext2/3/4 image (bad superblock magic)"));
        }
        let log = le32(&raw, 0x18);
        if log > 6 {
            return Err(invalid(format!("bad block size (log {})", log)));
        }
        let block_size = 1024u64 << log;
        let incompat = le32(&raw, 0x60);
        let is64 = incompat & INCOMPAT_64BIT != 0;
        let rev = le32(&raw, 0x4C);
        let sb = Superblock {
            block_size,
            inodes_count: le32(&raw, 0x0),
            blocks_count: le32(&raw, 0x4) as u64
                | if is64 {
                    (le32(&raw, 0x150) as u64) << 32
                } else {
                    0
                },
            inodes_per_group: le32(&raw, 0x28),
            inode_size: if rev == 0 {
                128
            } else {
                le16(&raw, 0x58) as u64
            },
            first_data_block: le32(&raw, 0x14) as u64,
            desc_size: if is64 {
                (le16(&raw, 0xFE) as u64).max(32)
            } else {
                32
            },
            compat: le32(&raw, 0x5C),
            incompat,
            ro_compat: le32(&raw, 0x64),
            raw,
        };

        let unsupported = sb.incompat & !INCOMPAT_SUPPORTED;
        if unsupported != 0 {
            return Err(invalid(format!(
                "unsupported ext4 features: {}",
                feature_list(1, unsupported)
            )));
        }
        if sb.inodes_per_group == 0 || sb.inode_size < 128 {
            return Err(invalid(
                "bad ext4 superblock (inodes per group / inode size)",
            ));
        }

        // group descriptors follow the superblock's block
        let groups = sb.inodes_count.div_ceil(sb.inodes_per_group) as u64;
        let gdt = (sb.first_data_block + 1) * block_size;
        let mut table = vec![0u8; (groups * sb.desc_size) as usize];
        r.seek(SeekFrom::Start(gdt))?;
        r.read_exact(&mut table)?;
        let inode_tables = (0..groups as usize)
            .map(|g| {
                let d = &table[g * sb.desc_size as usize..];
                let hi = if is64 && sb.desc_size >= 64 {
                    le32(d, 0x28) as u64
                } else {
                    0
                };
                le32(d, 0x8) as u64 | hi << 32
            })
            .collect();

        Ok(Ext4 {
            r,
            sb,
            inode_tables,
        })
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.r.seek(SeekFrom::Start(offset))?;
        self.r.read_exact(buf)
    }

    fn read_block(&mut self, block: u64) -> io::Result<Vec<u8>> {
        if block >= self.sb.blocks_count {
            return Err(invalid(format!(
                "block {} is outside the filesystem",
                block
            )));
        }
        let mut buf = vec![0u8; self.sb.block_size as usize];
        self.read_at(block * self.sb.block_size, &mut buf)?;
        Ok(buf)
    }

    fn inode(&mut self, ino: u32) -> io::Result<Inode> {
        if ino == 0 || ino > self.sb.inodes_count {
            return Err(invalid(format!("inode {} out of range", ino)));
        }
        let group = ((ino - 1) / self.sb.inodes_per_group) as usize;
        let index = ((ino - 1) % self.sb.inodes_per_group) as u64;
        let offset = self.inode_tables[group] * self.sb.block_size + index * self.sb.inode_size;
        let mut b = vec![0u8; self.sb.inode_size as usize];
        self.read_at(offset, &mut b)?;

        let flags = le32(&b, 0x20);
        let mut sectors = le32(&b, 0x1C) as u64 | (le16(&b, 0x74) as u64) << 32;
        if flags & FLAG_HUGE_FILE != 0 {
            sectors *= self.sb.block_size / 512;
        }
        let mut mtime = le32(&b, 0x10) as i32 as i64;
        let mut xattr_area = Vec::new();
        if b.len() > 128 {
            let extra = le16(&b, 0x80) as usize;
            if extra >= 12 && 128 + extra <= b.len() {
                // i_mtime_extra (0x88): epoch bits extend the 32-bit seconds
                let mtime_extra = le32(&b, 0x88);
                mtime += ((mtime_extra & 3) as i64) << 32;
            }
            if 128 + extra + 4 <= b.len() {
                xattr_area = b[128 + extra..].to_vec();
            }
        }
        let mut block = [0u8; 60];
        block.copy_from_slice(&b[0x28..0x64]);
        Ok(Inode {
            mode: le16(&b, 0x0),
            uid: le16(&b, 0x2) as u32 | (le16(&b, 0x78) as u32) << 16,
            gid: le16(&b, 0x18) as u32 | (le16(&b, 0x7A) as u32) << 16,
            size: le32(&b, 0x4) as u64 | (le32(&b, 0x6C) as u64) << 32,
            mtime,
            flags,
            sectors,
            block,
            file_acl: le32(&b, 0x68) as u64 | (le16(&b, 0x76) as u64) << 32,
            xattr_area,
        })
    }

    /// All xattrs of an inode as (full name, value).
    fn xattrs(&mut self, inode: &Inode) -> io::Result<Vec<(String, Vec<u8>)>> {
        let mut out = Vec::new();
        let area = &inode.xattr_area;
        if area.len() >= 4 && le32(area, 0) == XATTR_MAGIC {
            // in-inode: value offsets are relative to the first entry
            let entries = area[4..].to_vec();
            self.parse_xattrs(&entries, &entries, &mut out)?;
        }
        if inode.file_acl != 0 {
            let block = self.read_block(inode.file_acl)?;
            if le32(&block, 0) == XATTR_MAGIC {
                self.parse_xattrs(&block[32..], &block, &mut out)?;
            }
        }
        Ok(out)
    }

    fn parse_xattrs(
        &mut self,
        entries: &[u8],
        values: &[u8],
        out: &mut Vec<(String, Vec<u8>)>,
    ) -> io::Result<()> {
        let mut i = 0;
        while i + 16 <= entries.len() && le32(entries, i) != 0 {
            let name_len = entries[i] as usize;
            let index = entries[i + 1];
            let value_offs = le16(entries, i + 2) as usize;
            let value_inum = le32(entries, i + 4);
            let value_size = le32(entries, i + 8) as usize;
            let name_end = i + 16 + name_len;
            if name_end > entries.len() {
                return Err(invalid("xattr entry runs past its block"));
            }
            let prefix = match index {
                1 => "user.",
                2 => "system.posix_acl_access",
                3 => "system.posix_acl_default",
                4 => "trusted.",
                6 => "security.",
                7 => "system.",
                8 => "system.richacl",
                _ => "",
            };
            let name = format!(
                "{}{}",
                prefix,
                String::from_utf8_lossy(&entries[i + 16..name_end])
            );
            let value = if value_inum != 0 {
                // EA_INODE: the value is the content of another inode
                let ea = self.inode(value_inum)?;
                let mut v = Vec::new();
                self.write_data(&ea, &mut v)?;
                v.truncate(value_size);
                v
            } else {
                values
                    .get(value_offs..value_offs + value_size)
                    .ok_or_else(|| invalid("xattr value runs past its block"))?
                    .to_vec()
            };
            out.push((name, value));
            i = (name_end + 3) & !3;
        }
        Ok(())
    }

    /// Maps the logical blocks of an inode to physical blocks.
    fn extents(&mut self, inode: &Inode) -> io::Result<Vec<Extent>> {
        let nblocks = inode.size.div_ceil(self.sb.block_size);
        let mut out = Vec::new();
        if inode.flags & FLAG_EXTENTS != 0 {
            self.extent_node(&inode.block, 0, &mut out)?;
        } else {
            let ptrs: Vec<u64> = (0..15).map(|i| le32(&inode.block, i * 4) as u64).collect();
            let mut logical = 0u64;
            for (i, &p) in ptrs.iter().enumerate() {
                if logical >= nblocks {
                    break;
                }
                let depth = i.saturating_sub(11) as u32; // 0 for direct pointers
                self.map_indirect(p, depth, &mut logical, nblocks, &mut out)?;
            }
        }
        out.sort_by_key(|e| e.logical);
        Ok(out)
    }

    fn extent_node(&mut self, node: &[u8], level: u32, out: &mut Vec<Extent>) -> io::Result<()> {
        if level > 8 || node.len() < 12 || le16(node, 0) != EXTENT_MAGIC {
            return Err(invalid("bad extent tree"));
        }
        let entries = le16(node, 2) as usize;
        let depth = le16(node, 6);
        if 12 + entries * 12 > node.len() {
            return Err(invalid("extent node overflows"));
        }
        for n in 0..entries {
            let e = &node[12 + n * 12..24 + n * 12];
            if depth == 0 {
                let raw_len = le16(e, 4) as u64;
                let (len, uninit) = if raw_len > 32768 {
                    (raw_len - 32768, true)
                } else {
                    (raw_len, false)
                };
                out.push(Extent {
                    logical: le32(e, 0) as u64,
                    physical: le32(e, 8) as u64 | (le16(e, 6) as u64) << 32,
                    len,
                    uninit,
                });
            } else {
                let child = le32(e, 4) as u64 | (le16(e, 8) as u64) << 32;
                let block = self.read_block(child)?;
                self.extent_node(&block, level + 1, out)?;
            }
        }
        Ok(())
    }

    /// Legacy block map: `depth` 0 is a data block, 1-3 indirect blocks.
    fn map_indirect(
        &mut self,
        ptr: u64,
        depth: u32,
        logical: &mut u64,
        nblocks: u64,
        out: &mut Vec<Extent>,
    ) -> io::Result<()> {
        let per_block = self.sb.block_size / 4;
        let span = per_block.pow(depth);
        if ptr == 0 {
            *logical += span; // hole
            return Ok(());
        }
        if depth == 0 {
            match out.last_mut() {
                Some(e)
                    if !e.uninit && e.logical + e.len == *logical && e.physical + e.len == ptr =>
                {
                    e.len += 1
                }
                _ => out.push(Extent {
                    logical: *logical,
                    physical: ptr,
                    len: 1,
                    uninit: false,
                }),
            }
            *logical += 1;
            return Ok(());
        }
        let block = self.read_block(ptr)?;
        for i in 0..per_block as usize {
            if *logical >= nblocks {
                break;
            }
            self.map_indirect(le32(&block, i * 4) as u64, depth - 1, logical, nblocks, out)?;
        }
        Ok(())
    }

    /// Writes `inode.size` bytes of data (holes and unwritten extents as zeros).
    fn write_data(&mut self, inode: &Inode, out: &mut dyn Write) -> io::Result<u64> {
        if inode.flags & FLAG_ENCRYPT != 0 {
            return Err(invalid("encrypted file"));
        }
        if inode.flags & FLAG_INLINE_DATA != 0 {
            let data = self.inline_data(inode)?;
            out.write_all(&data)?;
            return Ok(data.len() as u64);
        }

        const CHUNK: u64 = 1 << 20;
        let bs = self.sb.block_size;
        let size = inode.size;
        let zeros = vec![0u8; CHUNK as usize];
        let mut buf = vec![0u8; CHUNK as usize];
        let mut pos = 0u64; // bytes written
        let write_zeros = |out: &mut dyn Write, mut n: u64| -> io::Result<()> {
            while n > 0 {
                let k = n.min(CHUNK);
                out.write_all(&zeros[..k as usize])?;
                n -= k;
            }
            Ok(())
        };

        for e in self.extents(inode)? {
            let start = (e.logical * bs).min(size);
            if start < pos {
                return Err(invalid("overlapping extents"));
            }
            write_zeros(out, start - pos)?;
            pos = start;
            let end = ((e.logical + e.len) * bs).min(size);
            if e.uninit {
                write_zeros(out, end - pos)?;
                pos = end;
                continue;
            }
            if e.physical + e.len > self.sb.blocks_count {
                return Err(invalid("extent points outside the filesystem"));
            }
            self.r.seek(SeekFrom::Start(e.physical * bs))?;
            while pos < end {
                let k = (end - pos).min(CHUNK) as usize;
                self.r.read_exact(&mut buf[..k])?;
                out.write_all(&buf[..k])?;
                pos += k as u64;
            }
        }
        write_zeros(out, size - pos)?;
        Ok(size)
    }

    /// Inline data: the first 60 bytes live in `i_block`, the rest in the
    /// `system.data` xattr.
    fn inline_data(&mut self, inode: &Inode) -> io::Result<Vec<u8>> {
        let mut data = inode.block.to_vec();
        if let Some((_, v)) = self
            .xattrs(inode)?
            .into_iter()
            .find(|(n, _)| n == "system.data")
        {
            data.extend_from_slice(&v);
        }
        data.truncate(inode.size as usize);
        Ok(data)
    }

    fn parse_dirents(buf: &[u8], filetype: bool, out: &mut Vec<(Vec<u8>, u32)>) {
        let mut i = 0;
        while i + 8 <= buf.len() {
            let ino = le32(buf, i);
            let rec_len = le16(buf, i + 4) as usize;
            let name_len = if filetype {
                buf[i + 6] as usize
            } else {
                le16(buf, i + 6) as usize
            };
            if rec_len < 8 || i + rec_len > buf.len() {
                break;
            }
            // inode 0: unused entry, htree node or checksum tail
            if ino != 0 && 8 + name_len <= rec_len {
                let name = &buf[i + 8..i + 8 + name_len];
                if name != b"." && name != b".." {
                    out.push((name.to_vec(), ino));
                }
            }
            i += rec_len;
        }
    }
}

impl<R: Read + Seek> Filesystem for Ext4<R> {
    type Node = u32;

    fn fs_type(&self) -> &'static str {
        "ext4"
    }

    fn root(&self) -> u32 {
        ROOT_INO
    }

    fn meta(&mut self, ino: u32) -> io::Result<Meta> {
        let inode = self.inode(ino)?;
        let kind = match inode.mode & 0xF000 {
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
                    ino, m
                )))
            }
        };
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

    fn read_dir(&mut self, ino: u32) -> io::Result<Vec<(Vec<u8>, u32)>> {
        let inode = self.inode(ino)?;
        if inode.mode & 0xF000 != 0x4000 {
            return Err(invalid(format!("inode {} is not a directory", ino)));
        }
        let filetype = self.sb.incompat & INCOMPAT_FILETYPE != 0;
        let mut out = Vec::new();
        if inode.flags & FLAG_INLINE_DATA != 0 {
            // 4-byte parent inode, then entries; more entries in system.data
            let data = self.inline_data(&Inode {
                size: u64::MAX,
                ..inode.clone()
            })?;
            let first = &data[4..60.min(data.len())];
            Self::parse_dirents(first, filetype, &mut out);
            if data.len() > 60 {
                Self::parse_dirents(&data[60..], filetype, &mut out);
            }
            return Ok(out);
        }
        let mut data = Vec::new();
        self.write_data(&inode, &mut data)?;
        for block in data.chunks(self.sb.block_size as usize) {
            Self::parse_dirents(block, filetype, &mut out);
        }
        Ok(out)
    }

    fn read_file(&mut self, ino: u32, out: &mut dyn Write) -> io::Result<u64> {
        let inode = self.inode(ino)?;
        self.write_data(&inode, out)
    }

    fn read_link(&mut self, ino: u32) -> io::Result<Vec<u8>> {
        let inode = self.inode(ino)?;
        let acl_sectors = if inode.file_acl != 0 {
            self.sb.block_size / 512
        } else {
            0
        };
        let fast = inode.flags & (FLAG_EXTENTS | FLAG_INLINE_DATA) == 0
            && inode.sectors.saturating_sub(acl_sectors) == 0
            && inode.size < 60;
        if fast {
            return Ok(inode.block[..inode.size as usize].to_vec());
        }
        let mut v = Vec::new();
        self.write_data(&inode, &mut v)?;
        Ok(v)
    }

    fn volume_name(&self) -> String {
        cstr(&self.sb.raw[0x78..0x88])
    }

    fn info(&self) -> Vec<(String, String)> {
        let raw = &self.sb.raw;
        let hash = match raw[0xFC] {
            0 => "legacy",
            1 => "half_md4",
            2 => "tea",
            _ => "unknown",
        };
        let features = [
            feature_list(0, self.sb.compat),
            feature_list(1, self.sb.incompat),
            feature_list(2, self.sb.ro_compat),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
        let r_blocks = le32(raw, 0x8) as u64
            | if self.sb.incompat & INCOMPAT_64BIT != 0 {
                (le32(raw, 0x154) as u64) << 32
            } else {
                0
            };
        [
            ("fs_type", "ext4".to_string()),
            ("volume_name", self.volume_name()),
            ("last_mounted", cstr(&raw[0x88..0xC8])),
            ("uuid", uuid(&raw[0x68..0x78])),
            ("hash_seed", uuid(&raw[0xEC..0xFC])),
            ("default_hash", hash.to_string()),
            ("block_size", self.sb.block_size.to_string()),
            ("blocks", self.sb.blocks_count.to_string()),
            ("reserved_blocks", r_blocks.to_string()),
            ("inodes", self.sb.inodes_count.to_string()),
            ("inode_size", self.sb.inode_size.to_string()),
            ("blocks_per_group", le32(raw, 0x20).to_string()),
            ("inodes_per_group", self.sb.inodes_per_group.to_string()),
            ("created", le32(raw, 0x108).to_string()),
            ("features", features),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }
}

fn feature_list(field: u8, bits: u32) -> String {
    let mut names = Vec::new();
    for bit in 0..32 {
        let mask = 1u32 << bit;
        if bits & mask == 0 {
            continue;
        }
        match FEATURE_NAMES
            .iter()
            .find(|&&(f, m, _)| f == field && m == mask)
        {
            Some(&(_, _, name)) => names.push(name.to_string()),
            None => names.push(format!(
                "FEATURE_{}{}",
                ["C", "I", "R"][field as usize],
                bit
            )),
        }
    }
    names.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_names() {
        assert_eq!(feature_list(0, 0x4 | 0x8), "has_journal ext_attr");
        assert_eq!(feature_list(2, 0x4000), "shared_blocks");
        assert_eq!(feature_list(1, 1 << 30), "FEATURE_I30");
    }

    #[test]
    fn uuid_format() {
        let b: Vec<u8> = (0..16).collect();
        assert_eq!(uuid(&b), "00010203-0405-0607-0809-0a0b0c0d0e0f");
    }

    #[test]
    fn rejects_non_ext4() {
        assert!(Ext4::open(std::io::Cursor::new(vec![0u8; 4096])).is_err());
        assert!(!is_ext4(&[0u8; 2048]));
    }
}
