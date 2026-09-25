//! Minimal ext4 image writer for Android read-only partitions.
//!
//! Layout: no journal, no flex_bg, no resize inode, sparse superblock
//! backups, 256-byte inodes, extents, linear directories. Features:
//! `ext_attr filetype extent sparse_super large_file dir_nlink extra_isize`.
//! Every file gets contiguous blocks where possible; xattrs
//! (`security.selinux`, `security.capability`) go in the inode when they
//! fit, otherwise in one xattr block.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::invalid;

const ROOT_INO: u32 = 2;
const LOST_FOUND_INO: u32 = 11;
const FIRST_INO: u32 = 11;
const INODE_SIZE: u64 = 256;
const EXTRA_ISIZE: usize = 32;
/// mke2fs gives lost+found 16 KiB so e2fsck can use it without allocating.
const LOST_FOUND_BYTES: u64 = 16384;
const MAX_EXTENT_LEN: u64 = 32768;

const COMPAT_EXT_ATTR: u32 = 0x8;
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_EXTENTS: u32 = 0x40;
const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
const RO_COMPAT_LARGE_FILE: u32 = 0x2;
const RO_COMPAT_DIR_NLINK: u32 = 0x20;
const RO_COMPAT_EXTRA_ISIZE: u32 = 0x40;
const FLAG_EXTENTS: u32 = 0x80000;
const XATTR_MAGIC: u32 = 0xEA02_0000;
const XATTR_INDEX_SECURITY: u8 = 6;

#[derive(Debug, Clone)]
pub enum NodeKind {
    Dir(Vec<Node>),
    /// Content is read from `source`; `size` must match it.
    File {
        source: PathBuf,
        size: u64,
    },
    Symlink(Vec<u8>),
}

/// One file, directory or symlink of the image tree.
#[derive(Debug, Clone)]
pub struct Node {
    /// Empty for the root.
    pub name: Vec<u8>,
    pub kind: NodeKind,
    /// Permission bits (`0o7777`).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub selinux: Option<String>,
    pub capabilities: Option<u64>,
}

impl Node {
    fn file_type(&self) -> u8 {
        match self.kind {
            NodeKind::File { .. } => 1,
            NodeKind::Dir(_) => 2,
            NodeKind::Symlink(_) => 7,
        }
    }

    fn type_bits(&self) -> u16 {
        match self.kind {
            NodeKind::File { .. } => 0x8000,
            NodeKind::Dir(_) => 0x4000,
            NodeKind::Symlink(_) => 0xA000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Params {
    pub block_size: u64,
    /// Filesystem size in blocks.
    pub blocks: u64,
    /// Minimum number of inodes (rounded up to fill the inode tables).
    pub inodes: u32,
    pub reserved_blocks: u64,
    pub uuid: [u8; 16],
    pub hash_seed: [u8; 16],
    pub volume_name: String,
    pub last_mounted: String,
    /// Used for every inode time and the superblock times.
    pub timestamp: u32,
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub blocks: u64,
    pub used_blocks: u64,
    pub inodes: u32,
    pub used_inodes: u32,
}

/// Node in inode-number order, with the numbers of its parent and children.
struct Flat<'a> {
    node: &'a Node,
    parent: u32,
    /// (name, inode, file type) for directories.
    children: Vec<(&'a [u8], u32, u8)>,
}

/// Assigns inode numbers: root = 2, lost+found = 11, the rest from 12.
/// Directories are visited depth-first and number all their children at
/// once, so siblings get consecutive inodes. Index `i` of the result holds
/// inode `i`; 0, 1 and 3-10 stay empty.
fn flatten(root: &Node) -> io::Result<Vec<Option<Flat<'_>>>> {
    let NodeKind::Dir(children) = &root.kind else {
        return Err(invalid("image root must be a directory"));
    };
    if !children
        .iter()
        .any(|c| c.name == b"lost+found" && matches!(c.kind, NodeKind::Dir(_)))
    {
        return Err(invalid("image root has no lost+found directory"));
    }

    let mut flat: Vec<Option<Flat>> = (0..=LOST_FOUND_INO).map(|_| None).collect();
    let mut next = LOST_FOUND_INO + 1;
    // (node, its inode, parent inode)
    let mut stack = vec![(root, ROOT_INO, ROOT_INO)];
    while let Some((node, ino, parent)) = stack.pop() {
        let mut kids = Vec::new();
        if let NodeKind::Dir(children) = &node.kind {
            let mut sorted: Vec<&Node> = children.iter().collect();
            sorted.sort_by(|a, b| a.name.cmp(&b.name));
            for w in sorted.windows(2) {
                if w[0].name == w[1].name {
                    return Err(invalid(format!(
                        "duplicate name {:?}",
                        String::from_utf8_lossy(&w[0].name)
                    )));
                }
            }
            for child in &sorted {
                if child.name.is_empty() || child.name.len() > 255 || child.name.contains(&b'/') {
                    return Err(invalid(format!(
                        "bad file name {:?}",
                        String::from_utf8_lossy(&child.name)
                    )));
                }
                let child_ino = if ino == ROOT_INO && child.name == b"lost+found" {
                    LOST_FOUND_INO
                } else {
                    next += 1;
                    next - 1
                };
                kids.push((child.name.as_slice(), child_ino, child.file_type()));
            }
            // reversed so the first child is visited first
            for (child, &(_, child_ino, _)) in sorted.iter().zip(kids.iter()).rev() {
                stack.push((child, child_ino, ino));
            }
        }
        let idx = ino as usize;
        if flat.len() <= idx {
            flat.resize_with(idx + 1, || None);
        }
        flat[idx] = Some(Flat {
            node,
            parent,
            children: kids,
        });
    }
    Ok(flat)
}

/// Directory blocks: ".", ".." and the children, entries never crossing a
/// block; the last entry of a block takes the rest of it.
fn dir_blocks(
    ino: u32,
    parent: u32,
    children: &[(&[u8], u32, u8)],
    bs: usize,
    min_bytes: u64,
) -> Vec<u8> {
    let mut entries: Vec<(&[u8], u32, u8)> = vec![(b".", ino, 2), (b"..", parent, 2)];
    entries.extend_from_slice(children);

    let mut out = Vec::new();
    let mut block = vec![0u8; bs];
    let mut pos = 0usize;
    let mut last = 0usize; // start of the previous entry in this block
    for (name, child, ftype) in entries {
        let len = (8 + name.len() + 3) & !3;
        if pos + len > bs {
            // stretch the previous entry to the end of the block
            let rec = (bs - last) as u16;
            block[last + 4..last + 6].copy_from_slice(&rec.to_le_bytes());
            out.extend_from_slice(&block);
            block.fill(0);
            pos = 0;
        }
        block[pos..pos + 4].copy_from_slice(&child.to_le_bytes());
        block[pos + 4..pos + 6].copy_from_slice(&(len as u16).to_le_bytes());
        block[pos + 6] = name.len() as u8;
        block[pos + 7] = ftype;
        block[pos + 8..pos + 8 + name.len()].copy_from_slice(name);
        last = pos;
        pos += len;
    }
    let rec = (bs - last) as u16;
    block[last + 4..last + 6].copy_from_slice(&rec.to_le_bytes());
    out.extend_from_slice(&block);

    // extra empty blocks (lost+found): one unused entry spanning the block
    while (out.len() as u64) < min_bytes {
        let mut empty = vec![0u8; bs];
        empty[4..6].copy_from_slice(&(bs as u16).to_le_bytes());
        out.extend_from_slice(&empty);
    }
    out
}

/// A security.* xattr: (name without prefix, value).
fn xattrs(node: &Node) -> Vec<(&'static str, Vec<u8>)> {
    let mut v = Vec::new();
    if let Some(label) = &node.selinux {
        let mut value = label.as_bytes().to_vec();
        value.push(0);
        v.push(("selinux", value));
    }
    if let Some(caps) = node.capabilities {
        // struct vfs_cap_data, revision 2 with the effective flag
        let mut value = Vec::with_capacity(20);
        value.extend_from_slice(&0x0200_0001u32.to_le_bytes());
        value.extend_from_slice(&(caps as u32).to_le_bytes());
        value.extend_from_slice(&0u32.to_le_bytes());
        value.extend_from_slice(&((caps >> 32) as u32).to_le_bytes());
        value.extend_from_slice(&0u32.to_le_bytes());
        v.push(("capability", value));
    }
    // sorted by (index, name length, name) like the kernel keeps them
    v.sort_by(|a, b| (a.0.len(), a.0).cmp(&(b.0.len(), b.0)));
    v
}

fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

/// `ext2fs_ext_attr_hash_entry`
fn xattr_hash(name: &str, value: &[u8]) -> u32 {
    let mut hash = 0u32;
    for &c in name.as_bytes() {
        hash = (hash << 5) ^ (hash >> 27) ^ c as u32;
    }
    let mut padded = value.to_vec();
    padded.resize(pad4(value.len()), 0);
    for w in padded.as_chunks::<4>().0 {
        hash = (hash << 16) ^ (hash >> 16) ^ u32::from_le_bytes(*w);
    }
    hash
}

/// Lays out xattr entries and values into `area`. Entry value offsets are
/// relative to `base` (the first entry for in-inode, the block start for a
/// block). Returns false when they don't fit.
fn place_xattrs(
    attrs: &[(&str, Vec<u8>)],
    area: &mut [u8],
    entries_at: usize,
    base: usize,
    with_hash: bool,
) -> bool {
    let entries_len: usize = attrs.iter().map(|(n, _)| 16 + pad4(n.len())).sum::<usize>() + 4;
    let values_len: usize = attrs.iter().map(|(_, v)| pad4(v.len())).sum();
    if entries_at + entries_len + values_len > area.len() {
        return false;
    }
    let mut e = entries_at;
    let mut value_end = area.len();
    for (name, value) in attrs {
        value_end -= pad4(value.len());
        area[value_end..value_end + value.len()].copy_from_slice(value);
        area[e] = name.len() as u8;
        area[e + 1] = XATTR_INDEX_SECURITY;
        area[e + 2..e + 4].copy_from_slice(&((value_end - base) as u16).to_le_bytes());
        area[e + 4..e + 8].copy_from_slice(&0u32.to_le_bytes());
        area[e + 8..e + 12].copy_from_slice(&(value.len() as u32).to_le_bytes());
        let hash = if with_hash {
            xattr_hash(name, value)
        } else {
            0
        };
        area[e + 12..e + 16].copy_from_slice(&hash.to_le_bytes());
        area[e + 16..e + 16 + name.len()].copy_from_slice(name.as_bytes());
        e += 16 + pad4(name.len());
    }
    true
}

fn xattr_block(attrs: &[(&str, Vec<u8>)], bs: usize) -> io::Result<Vec<u8>> {
    let mut block = vec![0u8; bs];
    if !place_xattrs(attrs, &mut block, 32, 0, true) {
        return Err(invalid("xattrs don't fit in one block"));
    }
    // header: magic, refcount, blocks, hash
    block[0..4].copy_from_slice(&XATTR_MAGIC.to_le_bytes());
    block[4..8].copy_from_slice(&1u32.to_le_bytes());
    block[8..12].copy_from_slice(&1u32.to_le_bytes());
    let mut hash = 0u32;
    let mut e = 32;
    for (name, _) in attrs {
        let h = u32::from_le_bytes(block[e + 12..e + 16].try_into().unwrap());
        hash = (hash << 16) ^ (hash >> 16) ^ h;
        e += 16 + pad4(name.len());
    }
    block[12..16].copy_from_slice(&hash.to_le_bytes());
    Ok(block)
}

struct Layout {
    bs: u64,
    blocks: u64,
    groups: u64,
    bpg: u64,
    ipg: u64,
    gdt_blocks: u64,
    itable_blocks: u64,
}

impl Layout {
    fn new(bs: u64, blocks: u64, min_inodes: u64) -> io::Result<Layout> {
        let bpg = bs * 8;
        let first = if bs == 1024 { 1 } else { 0 };
        let mut blocks = blocks;
        loop {
            let groups = (blocks - first).div_ceil(bpg);
            let per_block = bs / INODE_SIZE;
            // inodes per group: a multiple of 8 filling whole itable blocks
            let align = (per_block).max(8);
            let ipg = (min_inodes.div_ceil(groups)).div_ceil(align) * align;
            if ipg > bpg {
                return Err(invalid("too many inodes for this size"));
            }
            let gdt_blocks = (groups * 32).div_ceil(bs);
            let itable_blocks = ipg * INODE_SIZE / bs;
            let l = Layout {
                bs,
                blocks,
                groups,
                bpg,
                ipg,
                gdt_blocks,
                itable_blocks,
            };
            // like mke2fs: drop a last group too small for its metadata
            let last_len = blocks - first - (groups - 1) * bpg;
            if groups > 1 && last_len < l.overhead(groups - 1) + 50 {
                blocks -= last_len;
                continue;
            }
            if groups == 1 && blocks < l.overhead(0) + 16 {
                return Err(invalid("image size too small"));
            }
            return Ok(l);
        }
    }

    fn first_data_block(&self) -> u64 {
        if self.bs == 1024 {
            1
        } else {
            0
        }
    }

    fn has_super(g: u64) -> bool {
        fn power_of(mut n: u64, b: u64) -> bool {
            while n > 1 && n.is_multiple_of(b) {
                n /= b;
            }
            n == 1
        }
        g <= 1 || power_of(g, 3) || power_of(g, 5) || power_of(g, 7)
    }

    fn group_start(&self, g: u64) -> u64 {
        self.first_data_block() + g * self.bpg
    }

    fn group_len(&self, g: u64) -> u64 {
        (self.blocks - self.group_start(g)).min(self.bpg)
    }

    /// Blocks taken by the superblock copy, GDT, bitmaps and inode table.
    fn overhead(&self, g: u64) -> u64 {
        let sb = if Self::has_super(g) {
            1 + self.gdt_blocks
        } else {
            0
        };
        sb + 2 + self.itable_blocks
    }

    fn block_bitmap(&self, g: u64) -> u64 {
        self.group_start(g)
            + if Self::has_super(g) {
                1 + self.gdt_blocks
            } else {
                0
            }
    }

    fn inode_table(&self, g: u64) -> u64 {
        self.block_bitmap(g) + 2
    }

    fn inodes(&self) -> u64 {
        self.ipg * self.groups
    }
}

struct Allocator<'a> {
    l: &'a Layout,
    used: Vec<bool>,
    group: u64,
    /// Next block to hand out in `group`.
    next: u64,
}

impl<'a> Allocator<'a> {
    fn new(l: &'a Layout) -> Self {
        let mut used = vec![false; l.blocks as usize];
        for b in 0..l.first_data_block() {
            used[b as usize] = true;
        }
        for g in 0..l.groups {
            let s = l.group_start(g);
            for b in s..s + l.overhead(g) {
                used[b as usize] = true;
            }
        }
        Allocator {
            l,
            used,
            group: 0,
            next: l.group_start(0) + l.overhead(0),
        }
    }

    /// `n` blocks as contiguous runs (start, len), filling groups in order.
    fn alloc(&mut self, mut n: u64) -> io::Result<Vec<(u64, u64)>> {
        let mut runs: Vec<(u64, u64)> = Vec::new();
        while n > 0 {
            let end = self.l.group_start(self.group) + self.l.group_len(self.group);
            if self.next >= end {
                self.group += 1;
                if self.group >= self.l.groups {
                    return Err(io::Error::new(
                        io::ErrorKind::StorageFull,
                        "image is full; give a bigger size",
                    ));
                }
                self.next = self.l.group_start(self.group) + self.l.overhead(self.group);
                continue;
            }
            let k = n.min(end - self.next);
            for b in self.next..self.next + k {
                self.used[b as usize] = true;
            }
            match runs.last_mut() {
                Some(r) if r.0 + r.1 == self.next => r.1 += k,
                _ => runs.push((self.next, k)),
            }
            self.next += k;
            n -= k;
        }
        Ok(runs)
    }
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn extent_header(b: &mut [u8], entries: u16, max: u16, depth: u16) {
    put16(b, 0, 0xF30A);
    put16(b, 2, entries);
    put16(b, 4, max);
    put16(b, 6, depth);
    put32(b, 8, 0);
}

fn extent_leaf(b: &mut [u8], logical: u64, start: u64, len: u64) {
    put32(b, 0, logical as u32);
    put16(b, 4, len as u16);
    put16(b, 6, (start >> 32) as u16);
    put32(b, 8, start as u32);
}

fn extent_index(b: &mut [u8], first: u64, child: u64) {
    put32(b, 0, first as u32);
    put32(b, 4, child as u32);
    put16(b, 8, (child >> 32) as u16);
    put16(b, 10, 0);
}

/// Writes the image. Returns block and inode usage.
pub fn write_image(out: &Path, root: &Node, p: &Params) -> io::Result<Stats> {
    if p.block_size != 4096 && p.block_size != 1024 && p.block_size != 2048 {
        return Err(invalid(format!("unsupported block size {}", p.block_size)));
    }
    let flat = flatten(root)?;
    let used_inodes = flat.len() as u32 - 1;
    let l = Layout::new(
        p.block_size,
        p.blocks,
        (used_inodes as u64 + 1).max(p.inodes as u64),
    )?;
    if l.inodes() < used_inodes as u64 {
        return Err(invalid("not enough inodes"));
    }
    let bs = l.bs as usize;
    let mut alloc = Allocator::new(&l);

    let mut img = File::create(out)?;
    img.set_len(l.blocks * l.bs)?;
    let mut inode_used = vec![false; l.inodes() as usize + 1];
    for i in 1..FIRST_INO {
        inode_used[i as usize] = true;
    }
    let mut dirs_per_group = vec![0u32; l.groups as usize];

    for (ino, f) in flat.iter().enumerate() {
        let Some(f) = f else { continue };
        let ino = ino as u32;
        inode_used[ino as usize] = true;
        let node = f.node;

        // write the data, then the inode pointing at it
        let (tree, data_blocks, size) = match &node.kind {
            NodeKind::Dir(_) => {
                let min = if ino == LOST_FOUND_INO {
                    LOST_FOUND_BYTES
                } else {
                    0
                };
                let data = dir_blocks(ino, f.parent, &f.children, bs, min);
                let runs = alloc.alloc(data.len() as u64 / l.bs)?;
                write_runs(&mut img, &runs, l.bs, &mut &data[..])?;
                dirs_per_group[((ino - 1) as u64 / l.ipg) as usize] += 1;
                let (tree, blocks) = extent_tree(&mut img, &mut alloc, &contiguous(&runs), l.bs)?;
                (tree, blocks, data.len() as u64)
            }
            NodeKind::File { source, size } => {
                let extents = write_file(&mut img, &mut alloc, source, *size, l.bs)
                    .map_err(|e| with_path(e, source))?;
                let (tree, blocks) = extent_tree(&mut img, &mut alloc, &extents, l.bs)?;
                (tree, blocks, *size)
            }
            NodeKind::Symlink(target) => {
                if target.is_empty() || target.len() >= bs {
                    return Err(invalid("symlink target must be 1..block size bytes"));
                }
                if target.len() < 60 {
                    // fast symlink: target in i_block, no extents
                    (Tree::Inline(target.clone()), 0, target.len() as u64)
                } else {
                    let runs = alloc.alloc(1)?;
                    write_runs(&mut img, &runs, l.bs, &mut &target[..])?;
                    let (tree, blocks) =
                        extent_tree(&mut img, &mut alloc, &contiguous(&runs), l.bs)?;
                    (tree, blocks, target.len() as u64)
                }
            }
        };
        finish_inode(
            &mut img,
            &l,
            ino,
            f,
            node,
            (data_blocks, size),
            &tree,
            &mut alloc,
            p,
        )?;
    }

    // group descriptors, bitmaps, superblocks
    let mut gdt = vec![0u8; (l.gdt_blocks * l.bs) as usize];
    let mut free_blocks_total = 0u64;
    let mut free_inodes_total = 0u64;
    for g in 0..l.groups {
        let start = l.group_start(g);
        let len = l.group_len(g);
        let mut bb = vec![0xFFu8; bs]; // padding past the group end stays set
        let mut free_b = 0u64;
        for i in 0..len {
            let used = alloc.used[(start + i) as usize];
            if !used {
                free_b += 1;
                bb[(i / 8) as usize] &= !(1 << (i % 8));
            }
        }
        let mut ib = vec![0xFFu8; bs];
        let mut free_i = 0u64;
        for i in 0..l.ipg {
            let ino = g * l.ipg + i + 1;
            if !inode_used[ino as usize] {
                free_i += 1;
                ib[(i / 8) as usize] &= !(1 << (i % 8));
            }
        }
        write_at(&mut img, l.block_bitmap(g) * l.bs, &bb)?;
        write_at(&mut img, (l.block_bitmap(g) + 1) * l.bs, &ib)?;

        let d = &mut gdt[(g * 32) as usize..(g * 32 + 32) as usize];
        put32(d, 0x0, l.block_bitmap(g) as u32);
        put32(d, 0x4, (l.block_bitmap(g) + 1) as u32);
        put32(d, 0x8, l.inode_table(g) as u32);
        put16(d, 0xC, free_b as u16);
        put16(d, 0xE, free_i as u16);
        put16(d, 0x10, dirs_per_group[g as usize] as u16);
        free_blocks_total += free_b;
        free_inodes_total += free_i;
    }

    let mut sb = vec![0u8; 1024];
    put32(&mut sb, 0x0, l.inodes() as u32);
    put32(&mut sb, 0x4, l.blocks as u32);
    put32(&mut sb, 0x8, p.reserved_blocks.min(l.blocks / 2) as u32);
    put32(&mut sb, 0xC, free_blocks_total as u32);
    put32(&mut sb, 0x10, free_inodes_total as u32);
    put32(&mut sb, 0x14, l.first_data_block() as u32);
    let log = (l.bs / 1024).trailing_zeros();
    put32(&mut sb, 0x18, log);
    put32(&mut sb, 0x1C, log);
    put32(&mut sb, 0x20, l.bpg as u32);
    put32(&mut sb, 0x24, l.bpg as u32);
    put32(&mut sb, 0x28, l.ipg as u32);
    put32(&mut sb, 0x30, p.timestamp);
    put16(&mut sb, 0x36, 0xFFFF);
    put16(&mut sb, 0x38, 0xEF53);
    put16(&mut sb, 0x3A, 1); // clean
    put16(&mut sb, 0x3C, 1); // errors: continue
    put32(&mut sb, 0x40, p.timestamp);
    put32(&mut sb, 0x4C, 1); // dynamic revision
    put32(&mut sb, 0x54, FIRST_INO);
    put16(&mut sb, 0x58, INODE_SIZE as u16);
    put32(&mut sb, 0x5C, COMPAT_EXT_ATTR);
    put32(&mut sb, 0x60, INCOMPAT_FILETYPE | INCOMPAT_EXTENTS);
    put32(
        &mut sb,
        0x64,
        RO_COMPAT_SPARSE_SUPER | RO_COMPAT_LARGE_FILE | RO_COMPAT_DIR_NLINK | RO_COMPAT_EXTRA_ISIZE,
    );
    sb[0x68..0x78].copy_from_slice(&p.uuid);
    let name = p.volume_name.as_bytes();
    sb[0x78..0x78 + name.len().min(16)].copy_from_slice(&name[..name.len().min(16)]);
    let lm = p.last_mounted.as_bytes();
    sb[0x88..0x88 + lm.len().min(63)].copy_from_slice(&lm[..lm.len().min(63)]);
    sb[0xEC..0xFC].copy_from_slice(&p.hash_seed);
    sb[0xFC] = 1; // half_md4
    put32(&mut sb, 0x100, 0x0C); // default mount options: user_xattr acl
    put32(&mut sb, 0x108, p.timestamp);
    put16(&mut sb, 0x15C, EXTRA_ISIZE as u16);
    put16(&mut sb, 0x15E, EXTRA_ISIZE as u16);

    for g in 0..l.groups {
        if !Layout::has_super(g) {
            continue;
        }
        put16(&mut sb, 0x5A, g as u16);
        let start = l.group_start(g);
        let sb_at = if g == 0 { 1024 } else { start * l.bs };
        write_at(&mut img, sb_at, &sb)?;
        write_at(&mut img, (start + 1) * l.bs, &gdt)?;
    }
    img.flush()?;

    let used_blocks = alloc.used.iter().filter(|&&u| u).count() as u64;
    Ok(Stats {
        blocks: l.blocks,
        used_blocks,
        inodes: l.inodes() as u32,
        used_inodes,
    })
}

fn with_path(e: io::Error, p: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {}", p.display(), e))
}

fn write_at(img: &mut File, offset: u64, data: &[u8]) -> io::Result<()> {
    img.seek(SeekFrom::Start(offset))?;
    img.write_all(data)
}

/// Copies `src` into the runs; stops early when `src` ends (the image is
/// already zero-filled).
fn write_runs(img: &mut File, runs: &[(u64, u64)], bs: u64, src: &mut dyn Read) -> io::Result<()> {
    for &(start, len) in runs {
        img.seek(SeekFrom::Start(start * bs))?;
        let n = io::copy(&mut src.take(len * bs), img)?;
        if n < len * bs {
            break;
        }
    }
    Ok(())
}

/// Where the extent tree lives.
enum Tree {
    /// Up to 4 extents (logical, physical, len) in the inode.
    Direct(Vec<(u64, u64, u64)>),
    /// Up to 4 (first logical block, child block) index entries in the
    /// inode, pointing at a tree `depth` levels deep.
    Indexed { top: Vec<(u64, u64)>, depth: u16 },
    /// Fast symlink target.
    Inline(Vec<u8>),
}

/// Extents for data stored in `runs`, logically contiguous from block 0.
fn contiguous(runs: &[(u64, u64)]) -> Vec<(u64, u64, u64)> {
    let mut logical = 0;
    runs.iter()
        .map(|&(start, len)| {
            logical += len;
            (logical - len, start, len)
        })
        .collect()
}

fn is_zero(b: &[u8]) -> bool {
    b.iter().all(|&x| x == 0)
}

/// Copies a file into newly allocated blocks. All-zero blocks are left as
/// holes (like the sparse files in Android images). Returns the extents
/// (logical, physical, len).
fn write_file(
    img: &mut File,
    alloc: &mut Allocator,
    source: &Path,
    size: u64,
    bs: u64,
) -> io::Result<Vec<(u64, u64, u64)>> {
    const CHUNK_BLOCKS: usize = 256;
    let bs_us = bs as usize;
    let mut src = File::open(source)?;
    let mut buf = vec![0u8; CHUNK_BLOCKS * bs_us];
    let mut extents: Vec<(u64, u64, u64)> = Vec::new();
    let mut logical = 0u64;
    let mut left = size;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        src.read_exact(&mut buf[..n]).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                io::Error::new(e.kind(), "file shrank while building")
            } else {
                e
            }
        })?;
        let blocks = n.div_ceil(bs_us);
        let block = |k: usize| &buf[k * bs_us..((k + 1) * bs_us).min(n)];
        let mut i = 0;
        while i < blocks {
            if is_zero(block(i)) {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < blocks && !is_zero(block(j)) {
                j += 1;
            }
            let mut off = i;
            for (start, len) in alloc.alloc((j - i) as u64)? {
                let end = (off + len as usize) * bs_us;
                write_at(img, start * bs, &buf[off * bs_us..end.min(n)])?;
                let lg = logical + off as u64;
                match extents.last_mut() {
                    Some(e) if e.0 + e.2 == lg && e.1 + e.2 == start => e.2 += len,
                    _ => extents.push((lg, start, len)),
                }
                off += len as usize;
            }
            i = j;
        }
        logical += blocks as u64;
        left -= n as u64;
    }
    Ok(extents)
}

/// Builds the extent tree, writing leaf and index blocks when more than 4
/// entries are needed. Returns the tree and the blocks used (data + tree).
fn extent_tree(
    img: &mut File,
    alloc: &mut Allocator,
    extents: &[(u64, u64, u64)],
    bs: u64,
) -> io::Result<(Tree, u64)> {
    // extents hold at most 32768 blocks
    let mut split = Vec::new();
    for &(lg, ph, len) in extents {
        let mut done = 0;
        while done < len {
            let k = (len - done).min(MAX_EXTENT_LEN);
            split.push((lg + done, ph + done, k));
            done += k;
        }
    }
    let data: u64 = extents.iter().map(|e| e.2).sum();
    if split.len() <= 4 {
        return Ok((Tree::Direct(split), data));
    }

    let per_block = ((bs - 12) / 12) as usize;
    let mut tree_blocks = 0u64;
    // leaves, then index levels until 4 entries fit in the inode
    let mut level: Vec<(u64, u64)> = Vec::new();
    for chunk in split.chunks(per_block) {
        let block = alloc.alloc(1)?[0].0;
        let mut b = vec![0u8; bs as usize];
        extent_header(&mut b, chunk.len() as u16, per_block as u16, 0);
        for (i, &(lg, ph, len)) in chunk.iter().enumerate() {
            extent_leaf(&mut b[12 + i * 12..], lg, ph, len);
        }
        write_at(img, block * bs, &b)?;
        level.push((chunk[0].0, block));
        tree_blocks += 1;
    }
    let mut depth = 1u16;
    while level.len() > 4 {
        let mut up = Vec::new();
        for chunk in level.chunks(per_block) {
            let block = alloc.alloc(1)?[0].0;
            let mut b = vec![0u8; bs as usize];
            extent_header(&mut b, chunk.len() as u16, per_block as u16, depth);
            for (i, &(first, child)) in chunk.iter().enumerate() {
                extent_index(&mut b[12 + i * 12..], first, child);
            }
            write_at(img, block * bs, &b)?;
            up.push((chunk[0].0, block));
            tree_blocks += 1;
        }
        level = up;
        depth += 1;
    }
    Ok((Tree::Indexed { top: level, depth }, data + tree_blocks))
}

#[allow(clippy::too_many_arguments)]
fn finish_inode(
    img: &mut File,
    l: &Layout,
    ino: u32,
    f: &Flat,
    node: &Node,
    (data_blocks, size): (u64, u64),
    tree: &Tree,
    alloc: &mut Allocator,
    p: &Params,
) -> io::Result<()> {
    let mut b = vec![0u8; INODE_SIZE as usize];
    put16(&mut b, 0x0, node.type_bits() | (node.mode & 0o7777) as u16);
    put16(&mut b, 0x2, node.uid as u16);
    put32(&mut b, 0x4, size as u32);
    for at in [0x8, 0xC, 0x10] {
        put32(&mut b, at, p.timestamp);
    }
    put16(&mut b, 0x18, node.gid as u16);
    let links = match &node.kind {
        NodeKind::Dir(_) => {
            let subdirs = f.children.iter().filter(|c| c.2 == 2).count() as u64;
            let n = 2 + subdirs;
            // dir_nlink: 1 means "too many to count"
            if n >= 65000 {
                1
            } else {
                n as u16
            }
        }
        _ => 1,
    };
    put16(&mut b, 0x1A, links);
    put32(&mut b, 0x6C, (size >> 32) as u32);
    put16(&mut b, 0x78, (node.uid >> 16) as u16);
    put16(&mut b, 0x7A, (node.gid >> 16) as u16);
    put16(&mut b, 0x80, EXTRA_ISIZE as u16);
    put32(&mut b, 0x90, p.timestamp); // crtime

    let mut flags = 0u32;
    match tree {
        Tree::Inline(target) => b[0x28..0x28 + target.len()].copy_from_slice(target),
        Tree::Direct(extents) => {
            flags |= FLAG_EXTENTS;
            let ib = &mut b[0x28..0x64];
            extent_header(ib, extents.len() as u16, 4, 0);
            for (i, &(lg, ph, len)) in extents.iter().enumerate() {
                extent_leaf(&mut ib[12 + i * 12..], lg, ph, len);
            }
        }
        Tree::Indexed { top, depth } => {
            flags |= FLAG_EXTENTS;
            let ib = &mut b[0x28..0x64];
            extent_header(ib, top.len() as u16, 4, *depth);
            for (i, &(first, child)) in top.iter().enumerate() {
                extent_index(&mut ib[12 + i * 12..], first, child);
            }
        }
    }
    put32(&mut b, 0x20, flags);

    // xattrs: in the inode when they fit, else one xattr block
    let mut sectors = data_blocks * (l.bs / 512);
    let attrs = xattrs(node);
    if !attrs.is_empty() {
        let area_start = 128 + EXTRA_ISIZE;
        put32(&mut b, area_start, XATTR_MAGIC);
        let fits = place_xattrs(&attrs, &mut b[area_start + 4..], 0, 0, false);
        if !fits {
            b[area_start..].fill(0);
            let block = alloc.alloc(1)?[0].0;
            write_at(img, block * l.bs, &xattr_block(&attrs, l.bs as usize)?)?;
            put32(&mut b, 0x68, block as u32);
            put16(&mut b, 0x76, (block >> 32) as u16);
            sectors += l.bs / 512;
        }
    }
    put32(&mut b, 0x1C, sectors as u32);
    put16(&mut b, 0x74, (sectors >> 32) as u16);

    let g = (ino - 1) as u64 / l.ipg;
    let idx = (ino - 1) as u64 % l.ipg;
    write_at(img, l.inode_table(g) * l.bs + idx * INODE_SIZE, &b)
}

/// Smallest filesystem size (in blocks) holding `data` blocks of content
/// plus all group metadata for `inodes` inodes.
pub fn blocks_for(data: u64, inodes: u32, bs: u64) -> io::Result<u64> {
    let mut blocks = data + (inodes as u64 * INODE_SIZE).div_ceil(bs) + 64;
    loop {
        let l = Layout::new(bs, blocks, inodes as u64)?;
        let overhead: u64 =
            l.first_data_block() + (0..l.groups).map(|g| l.overhead(g)).sum::<u64>();
        if l.blocks >= data + overhead {
            return Ok(l.blocks);
        }
        // room for the content, plus a new last group big enough to keep
        blocks = data + overhead + l.overhead(l.groups) + 64;
    }
}

/// Blocks needed for `root` (data, directories, extent leaves and xattr
/// blocks), without filesystem metadata. Used to pick an image size.
pub fn data_blocks_needed(root: &Node, bs: u64) -> io::Result<(u64, u32)> {
    let flat = flatten(root)?;
    let mut blocks = 0u64;
    for (ino, f) in flat.iter().enumerate() {
        let Some(f) = f else { continue };
        let node = f.node;
        blocks += match &node.kind {
            NodeKind::Dir(_) => {
                let min = if ino as u32 == LOST_FOUND_INO {
                    LOST_FOUND_BYTES
                } else {
                    0
                };
                dir_blocks(ino as u32, f.parent, &f.children, bs as usize, min).len() as u64 / bs
            }
            NodeKind::File { size, .. } => size.div_ceil(bs),
            NodeKind::Symlink(t) if t.len() >= 60 => 1,
            NodeKind::Symlink(_) => 0,
        };
        let attrs = xattrs(node);
        let inline_room = INODE_SIZE as usize - 128 - EXTRA_ISIZE - 4;
        let need: usize = attrs
            .iter()
            .map(|(n, v)| 16 + pad4(n.len()) + pad4(v.len()))
            .sum::<usize>()
            + 4;
        if !attrs.is_empty() && need > inline_room {
            blocks += 1;
        }
    }
    Ok((blocks, flat.len() as u32 - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xattr_hash_by_hand() {
        // name "a": (0 << 5) ^ (0 >> 27) ^ 0x61
        assert_eq!(xattr_hash("a", b""), 0x61);
        // then one value word: (0x61 << 16) ^ (0x61 >> 16) ^ 1
        assert_eq!(xattr_hash("a", &[1, 0, 0, 0]), 0x0061_0001);
        // values are zero-padded to 4 bytes
        assert_eq!(xattr_hash("a", &[1]), 0x0061_0001);
    }

    #[test]
    fn sparse_super_groups() {
        let with: Vec<u64> = (0..60).filter(|&g| Layout::has_super(g)).collect();
        assert_eq!(with, [0, 1, 3, 5, 7, 9, 25, 27, 49]);
    }

    #[test]
    fn dir_block_layout() {
        let kids: Vec<(&[u8], u32, u8)> = vec![(b"a", 12, 1), (b"bb", 13, 2)];
        let d = dir_blocks(2, 2, &kids, 4096, 0);
        assert_eq!(d.len(), 4096);
        // "." rec_len 12, ".." 12, "a" 12, "bb" takes the rest
        assert_eq!(u16::from_le_bytes([d[4], d[5]]), 12);
        assert_eq!(u16::from_le_bytes([d[16], d[17]]), 12);
        assert_eq!(u16::from_le_bytes([d[28], d[29]]), 12);
        assert_eq!(u16::from_le_bytes([d[40], d[41]]), 4096 - 36);
    }
}
