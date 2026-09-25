//! Writes an uncompressed EROFS image from a `mkext4::Node` tree, like
//! Android's `mkfs.erofs` without compression:
//!
//! ```text
//! block 0      superblock at 1024, shared xattrs, then the inodes
//! ...          inodes: 32-byte slots (nid = byte offset / 32); each inode
//!              is followed by its xattr header and, for FLAT_INLINE, the
//!              tail of its data. None of them crosses a block.
//! data blocks  whole blocks of files, directories and symlinks, in inode order
//! ```
//!
//! Every xattr (SELinux label, capabilities) is shared: stored once, and
//! referenced by id from each inode. Compact inodes are used unless uid,
//! gid, size or link count need an extended one. All times are the build
//! time. Directories hold "." and "..", sorted with the other names.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::erofs::{
    COMPAT_SB_CHKSUM, EROFS_MAGIC, LAYOUT_FLAT_INLINE, LAYOUT_FLAT_PLAIN, SUPERBLOCK_OFFSET,
    XATTR_INDEX_SECURITY,
};
use super::invalid;
use super::mkext4::{security_xattrs, Node, NodeKind, Stats};

#[derive(Debug, Clone)]
pub struct Params {
    pub block_size: u64,
    pub uuid: [u8; 16],
    pub volume_name: String,
    /// Build time: every inode's mtime.
    pub timestamp: u64,
    pub timestamp_nsec: u32,
}

const DIRENT_SIZE: usize = 12;
/// End of the superblock: shared xattrs start here in block 0.
const SB_END: u64 = SUPERBLOCK_OFFSET + 128;

fn file_type(node: &Node) -> u8 {
    match node.kind {
        NodeKind::File { .. } => 1,
        NodeKind::Dir(_) => 2,
        NodeKind::Symlink(_) => 7,
    }
}

/// One inode, in nid order, with what the layout pass decided.
struct Item<'a> {
    node: &'a Node,
    /// Directories: the entries including "." and "..", sorted by name, as
    /// (name, item index, file type).
    entries: Vec<(&'a [u8], usize, u8)>,
    /// Directories: entries per block.
    dir_blocks: Vec<usize>,
    size: u64,
    nlink: u32,
    extended: bool,
    shared: Vec<u32>,
    inline_tail: bool,
    nid: u64,
    /// First data block of the whole blocks.
    blkaddr: u64,
}

impl Item<'_> {
    fn isize(&self) -> u64 {
        if self.extended {
            64
        } else {
            32
        }
    }

    fn xattr_isize(&self) -> u64 {
        if self.shared.is_empty() {
            0
        } else {
            12 + 4 * self.shared.len() as u64
        }
    }

    fn tail_len(&self, bs: u64) -> u64 {
        if self.inline_tail {
            self.size % bs
        } else {
            0
        }
    }

    /// Blocks in the data area.
    fn data_blocks(&self, bs: u64) -> u64 {
        if self.inline_tail {
            self.size / bs
        } else {
            self.size.div_ceil(bs)
        }
    }
}

/// Items in inode order: root = 0; directories are visited depth-first and
/// number all their children at once, so siblings are consecutive.
fn flatten(root: &Node) -> io::Result<Vec<Item<'_>>> {
    if !matches!(root.kind, NodeKind::Dir(_)) {
        return Err(invalid("image root must be a directory"));
    }
    let mut items: Vec<Option<Item>> = vec![None];
    // (node, its index, parent index)
    let mut stack = vec![(root, 0usize, 0usize)];
    while let Some((node, idx, parent)) = stack.pop() {
        let mut entries: Vec<(&[u8], usize, u8)> = Vec::new();
        let mut nlink = 1;
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
            nlink = 2;
            entries.push((b".", idx, 2));
            entries.push((b"..", parent, 2));
            let first = items.len();
            for (k, child) in sorted.iter().enumerate() {
                let name = child.name.as_slice();
                if name.is_empty()
                    || name.len() > 255
                    || name.contains(&b'/')
                    || name == b"."
                    || name == b".."
                {
                    return Err(invalid(format!(
                        "bad file name {:?}",
                        String::from_utf8_lossy(name)
                    )));
                }
                if matches!(child.kind, NodeKind::Dir(_)) {
                    nlink += 1;
                }
                entries.push((name, first + k, file_type(child)));
                items.push(None);
            }
            // "." and ".." sort with the other names
            entries.sort_by(|a, b| a.0.cmp(b.0));
            // reversed so the first child is visited first
            for (k, child) in sorted.iter().enumerate().rev() {
                stack.push((child, first + k, idx));
            }
        }
        items[idx] = Some(Item {
            node,
            entries,
            dir_blocks: Vec::new(),
            size: 0,
            nlink,
            extended: false,
            shared: Vec::new(),
            inline_tail: false,
            nid: 0,
            blkaddr: 0,
        });
    }
    Ok(items.into_iter().map(|i| i.unwrap()).collect())
}

/// Splits sorted directory entries into blocks; returns the entries per
/// block and the directory size (the last block is not padded).
fn dir_layout(entries: &[(&[u8], usize, u8)], bs: usize) -> (Vec<usize>, u64) {
    let mut blocks = Vec::new();
    let (mut count, mut used) = (0usize, 0usize);
    for (name, _, _) in entries {
        let need = DIRENT_SIZE + name.len();
        if used + need > bs {
            blocks.push(count);
            count = 0;
            used = 0;
        }
        count += 1;
        used += need;
    }
    blocks.push(count);
    let size = (blocks.len() as u64 - 1) * bs as u64 + used as u64;
    (blocks, size)
}

fn dir_data(items: &[Item], item: &Item, bs: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(item.size as usize);
    let mut rest = item.entries.as_slice();
    let nblocks = item.dir_blocks.len();
    for (b, &count) in item.dir_blocks.iter().enumerate() {
        let (these, next) = rest.split_at(count);
        rest = next;
        let start = out.len();
        let mut nameoff = count * DIRENT_SIZE;
        for (name, idx, ftype) in these {
            out.extend_from_slice(&items[*idx].nid.to_le_bytes());
            out.extend_from_slice(&(nameoff as u16).to_le_bytes());
            out.push(*ftype);
            out.push(0);
            nameoff += name.len();
        }
        for (name, _, _) in these {
            out.extend_from_slice(name);
        }
        if b + 1 < nblocks {
            out.resize(start + bs, 0);
        }
    }
    out
}

/// One xattr entry: name length, index, value size, name, value; 4-aligned.
fn xattr_entry(name: &str, value: &[u8]) -> Vec<u8> {
    let mut e = vec![name.len() as u8, XATTR_INDEX_SECURITY];
    e.extend_from_slice(&(value.len() as u16).to_le_bytes());
    e.extend_from_slice(name.as_bytes());
    e.extend_from_slice(value);
    e.resize(e.len().next_multiple_of(4), 0);
    e
}

/// crc32c (Castagnoli) without the final inversion, as the kernel uses it
/// for the superblock checksum.
fn crc32c(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0x82F6_3B78 } else { 0 };
        }
    }
    crc
}

/// Size in bytes of the image `write_image` would make, and the number of
/// inodes.
pub fn image_size(root: &Node, block_size: u64) -> io::Result<(u64, u32)> {
    let mut items = flatten(root)?;
    let (_, _, blocks) = layout(&mut items, block_size)?;
    Ok((blocks * block_size, items.len() as u32))
}

/// Decides sizes, inode forms, xattr ids, nids and data blocks. Returns the
/// shared xattr area, the end of the metadata (bytes) and the total blocks.
fn layout(items: &mut [Item], bs: u64) -> io::Result<(Vec<u8>, u64, u64)> {
    // shared xattrs, from byte SB_END of block 0 (xattr_blkaddr = 0)
    let mut shared_area = Vec::new();
    let mut ids: BTreeMap<Vec<u8>, u32> = BTreeMap::new();
    for item in items.iter_mut() {
        for (name, value) in security_xattrs(item.node) {
            let entry = xattr_entry(name, &value);
            let id = match ids.get(&entry) {
                Some(id) => *id,
                None => {
                    let mut pos = SB_END + shared_area.len() as u64;
                    if pos % bs + entry.len() as u64 > bs {
                        let pad = bs - pos % bs;
                        shared_area.resize(shared_area.len() + pad as usize, 0);
                        pos += pad;
                    }
                    let id = (pos / 4) as u32;
                    shared_area.extend_from_slice(&entry);
                    ids.insert(entry, id);
                    id
                }
            };
            item.shared.push(id);
        }
    }

    // sizes and inode forms
    for item in items.iter_mut() {
        item.size = match &item.node.kind {
            NodeKind::File { size, .. } => *size,
            NodeKind::Symlink(target) => {
                if target.is_empty() || target.len() as u64 > bs {
                    return Err(invalid(format!(
                        "symlink {:?}: bad target length",
                        String::from_utf8_lossy(&item.node.name)
                    )));
                }
                target.len() as u64
            }
            NodeKind::Dir(_) => {
                let (blocks, size) = dir_layout(&item.entries, bs as usize);
                item.dir_blocks = blocks;
                size
            }
        };
        let n = item.node;
        item.extended =
            n.uid > 0xFFFF || n.gid > 0xFFFF || item.size > u32::MAX as u64 || item.nlink > 0xFFFF;
        let tail = item.size % bs;
        item.inline_tail = tail > 0 && item.isize() + item.xattr_isize() + tail <= bs;
    }

    // nids: inodes after the shared xattrs, none crossing a block
    let mut pos = (SB_END + shared_area.len() as u64).next_multiple_of(32);
    for (i, item) in items.iter_mut().enumerate() {
        let record = item.isize() + item.xattr_isize() + item.tail_len(bs);
        if pos % bs + record > bs {
            pos = pos.next_multiple_of(bs);
        }
        item.nid = pos / 32;
        if i == 0 && item.nid > u16::MAX as u64 {
            return Err(invalid("too many shared xattrs: root inode out of reach"));
        }
        pos = (pos + record).next_multiple_of(32);
    }
    let meta_end = pos;

    // data blocks, in inode order
    let mut block = meta_end.div_ceil(bs);
    for item in items.iter_mut() {
        let n = item.data_blocks(bs);
        item.blkaddr = if n > 0 { block } else { 0 };
        block += n;
    }
    if block > u32::MAX as u64 {
        return Err(invalid("image too big for EROFS block addresses"));
    }
    Ok((shared_area, meta_end, block))
}

/// Writes the on-disk inode (and its xattr header) at `buf[off..]`.
fn put_inode(buf: &mut [u8], off: usize, item: &Item, index: usize) {
    let n = item.node;
    let layout = if item.inline_tail {
        LAYOUT_FLAT_INLINE
    } else {
        LAYOUT_FLAT_PLAIN
    };
    let mode = match n.kind {
        NodeKind::File { .. } => 0x8000,
        NodeKind::Dir(_) => 0x4000,
        NodeKind::Symlink(_) => 0xA000,
    } | (n.mode as u16 & 0o7777);
    // xattr_isize = 12 + 4 * (icount - 1)
    let icount: u16 = if item.shared.is_empty() {
        0
    } else {
        item.shared.len() as u16 + 1
    };
    let b = &mut buf[off..];
    let put16 = |b: &mut [u8], i: usize, v: u16| b[i..i + 2].copy_from_slice(&v.to_le_bytes());
    let put32 = |b: &mut [u8], i: usize, v: u32| b[i..i + 4].copy_from_slice(&v.to_le_bytes());
    put16(b, 0, (layout << 1) | item.extended as u16);
    put16(b, 2, icount);
    put16(b, 4, mode);
    put32(b, 16, item.blkaddr as u32);
    put32(b, 20, index as u32 + 1);
    if item.extended {
        b[8..16].copy_from_slice(&item.size.to_le_bytes());
        put32(b, 24, n.uid);
        put32(b, 28, n.gid);
        // i_mtime (32) and i_mtime_nsec (40) are set by the caller
        put32(b, 44, item.nlink);
    } else {
        put16(b, 6, item.nlink as u16);
        put32(b, 8, item.size as u32);
        // i_mtime (12): relative to the build time
        put16(b, 24, n.uid as u16);
        put16(b, 26, n.gid as u16);
    }
    if !item.shared.is_empty() {
        let x = item.isize() as usize;
        // h_name_filter (unused), h_shared_count, reserved
        b[x + 4] = item.shared.len() as u8;
        for (k, id) in item.shared.iter().enumerate() {
            put32(b, x + 12 + k * 4, *id);
        }
    }
}

/// Writes `root` as an EROFS image to `out`.
pub fn write_image(out: &Path, root: &Node, p: &Params) -> io::Result<Stats> {
    let bs = p.block_size;
    if !bs.is_power_of_two() || !(512..=65536).contains(&bs) {
        return Err(invalid(format!("unsupported block size {}", bs)));
    }
    if p.volume_name.len() > 16 {
        return Err(invalid("volume name longer than 16 bytes"));
    }
    let mut items = flatten(root)?;
    let (shared_area, meta_end, blocks) = layout(&mut items, bs)?;

    let mut meta = vec![0u8; meta_end.next_multiple_of(bs) as usize];
    let s = SB_END as usize;
    meta[s..s + shared_area.len()].copy_from_slice(&shared_area);

    let file = File::create(out)?;
    file.set_len(blocks * bs)?;
    let mut data = BufWriter::with_capacity(1 << 20, file);
    data.seek(SeekFrom::Start(meta_end.div_ceil(bs) * bs))?;

    let mut buf = vec![0u8; 1 << 20];
    for i in 0..items.len() {
        let item = &items[i];
        let off = (item.nid * 32) as usize;
        put_inode(&mut meta, off, item, i);
        if item.extended {
            meta[off + 32..off + 40].copy_from_slice(&p.timestamp.to_le_bytes());
            meta[off + 40..off + 44].copy_from_slice(&p.timestamp_nsec.to_le_bytes());
        }
        let tail_len = item.tail_len(bs) as usize;
        let body = item.size - tail_len as u64;
        let tail_at = off + (item.isize() + item.xattr_isize()) as usize;

        // content: body -> data blocks, tail -> after the inode
        match &item.node.kind {
            NodeKind::File { source, size } => {
                let mut f = File::open(source).map_err(|e| {
                    io::Error::new(e.kind(), format!("{}: {}", source.display(), e))
                })?;
                let mut left = body;
                while left > 0 {
                    let n = left.min(buf.len() as u64) as usize;
                    f.read_exact(&mut buf[..n])
                        .map_err(|e| changed(source, e))?;
                    data.write_all(&buf[..n])?;
                    left -= n as u64;
                }
                f.read_exact(&mut meta[tail_at..tail_at + tail_len])
                    .map_err(|e| changed(source, e))?;
                if f.read(&mut buf[..1])? != 0 || f.metadata()?.len() != *size {
                    return Err(changed(source, io::ErrorKind::InvalidData.into()));
                }
            }
            NodeKind::Symlink(target) => {
                let (b, t) = target.split_at(body as usize);
                data.write_all(b)?;
                meta[tail_at..tail_at + tail_len].copy_from_slice(t);
            }
            NodeKind::Dir(_) => {
                let d = dir_data(&items, item, bs as usize);
                let (b, t) = d.split_at(body as usize);
                data.write_all(b)?;
                meta[tail_at..tail_at + tail_len].copy_from_slice(t);
            }
        }
        // pad the last data block
        let pad = (bs - body % bs) % bs;
        if pad > 0 {
            io::copy(&mut io::repeat(0).take(pad), &mut data)?;
        }
    }

    // superblock
    let sb = &mut meta[SUPERBLOCK_OFFSET as usize..SB_END as usize];
    sb[0..4].copy_from_slice(&EROFS_MAGIC.to_le_bytes());
    sb[8..12].copy_from_slice(&COMPAT_SB_CHKSUM.to_le_bytes());
    sb[12] = bs.trailing_zeros() as u8;
    sb[14..16].copy_from_slice(&(items[0].nid as u16).to_le_bytes());
    sb[16..24].copy_from_slice(&(items.len() as u64).to_le_bytes());
    sb[24..32].copy_from_slice(&p.timestamp.to_le_bytes());
    sb[32..36].copy_from_slice(&p.timestamp_nsec.to_le_bytes());
    sb[36..40].copy_from_slice(&(blocks as u32).to_le_bytes());
    // meta_blkaddr (40) and xattr_blkaddr (44) are 0
    sb[48..64].copy_from_slice(&p.uuid);
    sb[64..64 + p.volume_name.len()].copy_from_slice(p.volume_name.as_bytes());
    let crc = crc32c(!0, &meta[SUPERBLOCK_OFFSET as usize..bs as usize]);
    meta[SUPERBLOCK_OFFSET as usize + 4..SUPERBLOCK_OFFSET as usize + 8]
        .copy_from_slice(&crc.to_le_bytes());

    data.seek(SeekFrom::Start(0))?;
    data.write_all(&meta)?;
    data.flush()?;
    Ok(Stats {
        blocks,
        used_blocks: blocks,
        inodes: items.len() as u32,
        used_inodes: items.len() as u32,
    })
}

fn changed(path: &Path, e: io::Error) -> io::Error {
    io::Error::new(
        e.kind(),
        format!(
            "{}: changed while building the image ({})",
            path.display(),
            e
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::erofs::Erofs;
    use crate::fs::{Filesystem, Kind};

    fn node(name: &str, kind: NodeKind, mode: u32, uid: u32) -> Node {
        Node {
            name: name.as_bytes().to_vec(),
            kind,
            mode,
            uid,
            gid: 2000,
            selinux: Some("u:object_r:system_file:s0".into()),
            capabilities: None,
        }
    }

    #[test]
    fn crc32c_check_value() {
        assert_eq!(!crc32c(!0, b"123456789"), 0xE306_9283);
    }

    /// Writes a tree with inline tails, plain files, a multi-block
    /// directory, a big uid (extended inode) and capabilities, then reads
    /// it back with the EROFS reader.
    #[test]
    fn write_then_read() {
        let dir = std::env::temp_dir().join(format!("jancox-mkerofs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut files = Vec::new();
        for (name, len) in [
            ("empty", 0usize),
            ("small", 5),
            ("tail", 10000),
            ("exact", 8192),
        ] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
            let path = dir.join(name);
            std::fs::write(&path, &data).unwrap();
            files.push((name, data, path));
        }
        let mut children: Vec<Node> = files
            .iter()
            .map(|(name, data, path)| {
                let kind = NodeKind::File {
                    source: path.clone(),
                    size: data.len() as u64,
                };
                node(name, kind, 0o644, 0)
            })
            .collect();
        let mut run_as = node("run-as", children[1].kind.clone(), 0o750, 100_000);
        run_as.capabilities = Some(0xc0);
        children.push(run_as);
        children.push(node(
            "link",
            NodeKind::Symlink(b"/system/bin/sh".to_vec()),
            0o777,
            0,
        ));
        let many: Vec<Node> = (0..300)
            .map(|i| {
                node(
                    &format!("entry_with_a_long_name_{:03}", i),
                    NodeKind::Dir(vec![]),
                    0o755,
                    0,
                )
            })
            .collect();
        children.push(node("many", NodeKind::Dir(many), 0o755, 0));
        let root = node("", NodeKind::Dir(children), 0o755, 0);

        let img = dir.join("test.img");
        let params = Params {
            block_size: 4096,
            uuid: [7; 16],
            volume_name: "".into(),
            timestamp: 1_230_768_000,
            timestamp_nsec: 0,
        };
        let stats = write_image(&img, &root, &params).unwrap();
        assert_eq!(stats.used_inodes, 1 + 4 + 2 + 1 + 300);
        assert_eq!(
            image_size(&root, 4096).unwrap().0,
            std::fs::metadata(&img).unwrap().len()
        );

        let mut fs = Erofs::open(File::open(&img).unwrap()).unwrap();
        let top: BTreeMap<Vec<u8>, u64> = fs.read_dir(fs.root()).unwrap().into_iter().collect();
        assert_eq!(top.len(), 7);
        for (name, data, _) in &files {
            let nid = top[name.as_bytes()];
            let mut out = Vec::new();
            assert_eq!(fs.read_file(nid, &mut out).unwrap(), data.len() as u64);
            assert_eq!(&out, data, "{}", name);
            let m = fs.meta(nid).unwrap();
            assert_eq!((m.kind, m.mode, m.gid), (Kind::File, 0o644, 2000));
            assert_eq!(m.selinux.as_deref(), Some("u:object_r:system_file:s0"));
            assert_eq!(m.mtime, 1_230_768_000);
        }
        let m = fs.meta(top[&b"run-as"[..]]).unwrap();
        assert_eq!(
            (m.uid, m.mode, m.capabilities),
            (100_000, 0o750, Some(0xc0))
        );
        assert_eq!(fs.read_link(top[&b"link"[..]]).unwrap(), b"/system/bin/sh");
        let many = fs.read_dir(top[&b"many"[..]]).unwrap();
        assert_eq!(many.len(), 300);
        assert!(many.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(fs.meta(many[299].1).unwrap().kind, Kind::Dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
