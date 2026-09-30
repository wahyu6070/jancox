//! Logical partitions (AOSP liblp): the metadata of `super.img` and
//! `super_empty.img`, reading partitions out of a super image, and the
//! metadata of a new one. Formats from `liblp/metadata_format.h`:
//!
//! ```text
//! super.img:       4096 reserved | geometry | backup geometry
//!                  | metadata slot 0..n | backup slots 0..n | partition data
//! super_empty.img: geometry | metadata (one copy)
//! ```
//!
//! Sizes are in 512-byte sectors; each metadata slot is a header plus the
//! partition, extent, group and block device tables, with SHA-256
//! checksums.

use std::io::{self, Read, Seek, SeekFrom};

use sha2::{Digest, Sha256};

use crate::fs::invalid;

pub const SECTOR: u64 = 512;
pub const RESERVED: u64 = 4096;
const GEOMETRY_SIZE: u64 = 4096;
const GEOMETRY_MAGIC: u32 = 0x616c_4467;
const GEOMETRY_STRUCT: usize = 52;
const HEADER_MAGIC: u32 = 0x414C_5030;
const MAJOR: u16 = 10;
const PARTITION_SIZE: usize = 52;
const EXTENT_SIZE: usize = 24;
const GROUP_SIZE: usize = 48;
const BLOCK_DEVICE_SIZE: usize = 64;
pub const TARGET_LINEAR: u32 = 0;
pub const TARGET_ZERO: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub metadata_max_size: u32,
    pub slot_count: u32,
    pub logical_block_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub sectors: u64,
    pub target_type: u32,
    /// First sector on the block device (linear extents).
    pub target_data: u64,
    pub target_source: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub name: String,
    pub attributes: u32,
    pub extents: Vec<Extent>,
    pub group: u32,
}

impl Partition {
    pub fn size(&self) -> u64 {
        self.extents.iter().map(|e| e.sectors).sum::<u64>() * SECTOR
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub flags: u32,
    /// 0 = no limit.
    pub max_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDevice {
    pub first_logical_sector: u64,
    pub alignment: u32,
    pub alignment_offset: u32,
    pub size: u64,
    pub name: String,
    pub flags: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub geometry: Geometry,
    pub minor: u16,
    pub header_size: u32,
    pub flags: u32,
    pub partitions: Vec<Partition>,
    pub groups: Vec<Group>,
    pub block_devices: Vec<BlockDevice>,
}

fn le16(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn le32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
fn le64(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

fn cname(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn put_name(b: &mut [u8], name: &str) -> io::Result<()> {
    if name.len() > 35 {
        return Err(invalid(format!("name too long for liblp: {}", name)));
    }
    b[..name.len()].copy_from_slice(name.as_bytes());
    Ok(())
}

/// Checks the SHA-256 at `sum` of `b`, computed with that field zeroed.
fn checksum_ok(b: &[u8], sum: usize) -> bool {
    let mut copy = b.to_vec();
    copy[sum..sum + 32].fill(0);
    Sha256::digest(&copy)[..] == b[sum..sum + 32]
}

pub fn parse_geometry(b: &[u8]) -> io::Result<Geometry> {
    if b.len() < GEOMETRY_STRUCT || le32(b, 0) != GEOMETRY_MAGIC {
        return Err(invalid("no liblp geometry"));
    }
    let size = le32(b, 4) as usize;
    if size != GEOMETRY_STRUCT || !checksum_ok(&b[..size], 8) {
        return Err(invalid("bad liblp geometry checksum"));
    }
    let g = Geometry {
        metadata_max_size: le32(b, 40),
        slot_count: le32(b, 44),
        logical_block_size: le32(b, 48),
    };
    if g.metadata_max_size == 0
        || !g.metadata_max_size.is_multiple_of(SECTOR as u32)
        || g.slot_count == 0
        || g.logical_block_size == 0
        || !g.logical_block_size.is_multiple_of(SECTOR as u32)
    {
        return Err(invalid("bad liblp geometry"));
    }
    Ok(g)
}

/// Parses one metadata slot (header + tables).
pub fn parse_metadata(b: &[u8], geometry: Geometry) -> io::Result<Metadata> {
    let bad = |what: &str| invalid(format!("liblp metadata: {}", what));
    if b.len() < 128 || le32(b, 0) != HEADER_MAGIC {
        return Err(bad("no header"));
    }
    if le16(b, 4) != MAJOR {
        return Err(bad("unsupported version"));
    }
    let minor = le16(b, 6);
    let header_size = le32(b, 8) as usize;
    if header_size < 128 || header_size > b.len() || !checksum_ok(&b[..header_size], 12) {
        return Err(bad("bad header checksum"));
    }
    let tables_size = le32(b, 44) as usize;
    let tables = b
        .get(header_size..header_size + tables_size)
        .ok_or_else(|| bad("truncated"))?;
    if Sha256::digest(tables)[..] != b[48..80] {
        return Err(bad("bad tables checksum"));
    }
    let flags = if header_size >= 132 { le32(b, 128) } else { 0 };
    let table = |i: usize, entry: usize| -> io::Result<Vec<&[u8]>> {
        let d = 80 + i * 12;
        let (off, n, size) = (
            le32(b, d) as usize,
            le32(b, d + 4) as usize,
            le32(b, d + 8) as usize,
        );
        if size != entry {
            return Err(bad("unexpected table entry size"));
        }
        (0..n)
            .map(|k| {
                tables
                    .get(off + k * size..off + (k + 1) * size)
                    .ok_or_else(|| bad("truncated table"))
            })
            .collect()
    };
    let extents: Vec<Extent> = table(1, EXTENT_SIZE)?
        .iter()
        .map(|e| Extent {
            sectors: le64(e, 0),
            target_type: le32(e, 8),
            target_data: le64(e, 12),
            target_source: le32(e, 20),
        })
        .collect();
    let mut partitions = Vec::new();
    for p in table(0, PARTITION_SIZE)? {
        let first = le32(p, 40) as usize;
        let count = le32(p, 44) as usize;
        let ext = extents
            .get(first..first + count)
            .ok_or_else(|| bad("bad extent index"))?;
        partitions.push(Partition {
            name: cname(&p[..36]),
            attributes: le32(p, 36),
            extents: ext.to_vec(),
            group: le32(p, 48),
        });
    }
    let groups: Vec<Group> = table(2, GROUP_SIZE)?
        .iter()
        .map(|g| Group {
            name: cname(&g[..36]),
            flags: le32(g, 36),
            max_size: le64(g, 40),
        })
        .collect();
    let block_devices: Vec<BlockDevice> = table(3, BLOCK_DEVICE_SIZE)?
        .iter()
        .map(|d| BlockDevice {
            first_logical_sector: le64(d, 0),
            alignment: le32(d, 8),
            alignment_offset: le32(d, 12),
            size: le64(d, 16),
            name: cname(&d[24..60]),
            flags: le32(d, 60),
        })
        .collect();
    if partitions.iter().any(|p| p.group as usize >= groups.len()) || block_devices.is_empty() {
        return Err(bad("bad group or block device table"));
    }
    Ok(Metadata {
        geometry,
        minor,
        header_size: header_size as u32,
        flags,
        partitions,
        groups,
        block_devices,
    })
}

/// Metadata of a `super_empty.img` (geometry at 0, one metadata copy).
pub fn read_empty(image: &[u8]) -> io::Result<Metadata> {
    let g = parse_geometry(image)?;
    let m = image
        .get(GEOMETRY_SIZE as usize..)
        .ok_or_else(|| invalid("super_empty.img: no metadata"))?;
    parse_metadata(m, g)
}

/// True when `r` starts like a super image (geometry after the reserved
/// bytes).
pub fn is_super<R: Read + Seek>(r: &mut R) -> bool {
    let mut b = [0u8; 4];
    r.seek(SeekFrom::Start(RESERVED)).is_ok()
        && r.read_exact(&mut b).is_ok()
        && u32::from_le_bytes(b) == GEOMETRY_MAGIC
}

fn read_exact_at<R: Read + Seek>(r: &mut R, pos: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut b = vec![0u8; len];
    r.seek(SeekFrom::Start(pos))?;
    r.read_exact(&mut b)?;
    Ok(b)
}

/// Metadata of slot `slot` of a super image, from the primary copy or,
/// when that is damaged, the backup.
pub fn read_super<R: Read + Seek>(r: &mut R, slot: u32) -> io::Result<Metadata> {
    let g = match parse_geometry(&read_exact_at(r, RESERVED, GEOMETRY_SIZE as usize)?) {
        Ok(g) => g,
        Err(_) => parse_geometry(&read_exact_at(
            r,
            RESERVED + GEOMETRY_SIZE,
            GEOMETRY_SIZE as usize,
        )?)?,
    };
    if slot >= g.slot_count {
        return Err(invalid(format!(
            "super.img has {} metadata slots",
            g.slot_count
        )));
    }
    let max = g.metadata_max_size as u64;
    let primary = RESERVED + 2 * GEOMETRY_SIZE + max * slot as u64;
    let backup = RESERVED + 2 * GEOMETRY_SIZE + max * (g.slot_count + slot) as u64;
    match parse_metadata(&read_exact_at(r, primary, max as usize)?, g) {
        Ok(m) => Ok(m),
        Err(e) => parse_metadata(&read_exact_at(r, backup, max as usize)?, g).map_err(|_| e),
    }
}

impl Metadata {
    pub fn partition(&self, name: &str) -> Option<&Partition> {
        self.partitions.iter().find(|p| p.name == name)
    }

    /// Where a partition's bytes are on the super device, in order:
    /// (length, Some(byte offset)) or (length, None) for zero extents.
    pub fn segments(&self, p: &Partition) -> io::Result<Vec<(u64, Option<u64>)>> {
        p.extents
            .iter()
            .map(|e| match e.target_type {
                TARGET_LINEAR if e.target_source == 0 => {
                    Ok((e.sectors * SECTOR, Some(e.target_data * SECTOR)))
                }
                TARGET_LINEAR => Err(invalid(format!(
                    "{}: extents on a second block device are not supported",
                    p.name
                ))),
                TARGET_ZERO => Ok((e.sectors * SECTOR, None)),
                t => Err(invalid(format!("{}: unknown extent type {}", p.name, t))),
            })
            .collect()
    }

    /// Size of the super device.
    pub fn super_size(&self) -> u64 {
        self.block_devices[0].size
    }

    /// Checks that every group holds its partitions.
    pub fn check_groups(&self) -> io::Result<()> {
        for (i, g) in self.groups.iter().enumerate() {
            let used: u64 = self
                .partitions
                .iter()
                .filter(|p| p.group as usize == i)
                .map(Partition::size)
                .sum();
            if g.max_size > 0 && used > g.max_size {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    format!(
                        "the partitions in group {} need {} bytes, more than its {} bytes; remove some files",
                        g.name, used, g.max_size
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Gives every partition one linear extent of `sizes[i]` bytes (0 = no
    /// extent), one after another from the first usable sector, aligned as
    /// the block device says.
    pub fn relayout(&mut self, sizes: &[u64]) -> io::Result<()> {
        let dev = &self.block_devices[0];
        let lbs = self.geometry.logical_block_size as u64;
        let align = (dev.alignment as u64).max(lbs) / SECTOR;
        let offset = dev.alignment_offset as u64 / SECTOR;
        let end = dev.size / SECTOR;
        let mut next = dev.first_logical_sector;
        for (p, &size) in self.partitions.iter_mut().zip(sizes) {
            p.extents.clear();
            if size == 0 {
                continue;
            }
            if size % lbs != 0 {
                return Err(invalid(format!(
                    "{}: {} bytes is not a multiple of the {} byte block size",
                    p.name, size, lbs
                )));
            }
            // align (sector - offset) to the alignment
            let start = (next.saturating_sub(offset)).next_multiple_of(align) + offset;
            let start = if start < next { start + align } else { start };
            let sectors = size / SECTOR;
            if start + sectors > end {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    format!(
                        "the partitions don't fit the {} byte super partition; remove some files",
                        dev.size
                    ),
                ));
            }
            p.extents.push(Extent {
                sectors,
                target_type: TARGET_LINEAR,
                target_data: start,
                target_source: 0,
            });
            next = start + sectors;
        }
        self.check_groups()
    }

    /// Encodes one metadata slot (header + tables).
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let mut parts = Vec::new();
        let mut extents = Vec::new();
        let mut n_ext = 0u32;
        for p in &self.partitions {
            let mut b = vec![0u8; PARTITION_SIZE];
            put_name(&mut b[..36], &p.name)?;
            b[36..40].copy_from_slice(&p.attributes.to_le_bytes());
            b[40..44].copy_from_slice(&n_ext.to_le_bytes());
            b[44..48].copy_from_slice(&(p.extents.len() as u32).to_le_bytes());
            b[48..52].copy_from_slice(&p.group.to_le_bytes());
            parts.extend(b);
            for e in &p.extents {
                let mut b = vec![0u8; EXTENT_SIZE];
                b[0..8].copy_from_slice(&e.sectors.to_le_bytes());
                b[8..12].copy_from_slice(&e.target_type.to_le_bytes());
                b[12..20].copy_from_slice(&e.target_data.to_le_bytes());
                b[20..24].copy_from_slice(&e.target_source.to_le_bytes());
                extents.extend(b);
                n_ext += 1;
            }
        }
        let mut groups = Vec::new();
        for g in &self.groups {
            let mut b = vec![0u8; GROUP_SIZE];
            put_name(&mut b[..36], &g.name)?;
            b[36..40].copy_from_slice(&g.flags.to_le_bytes());
            b[40..48].copy_from_slice(&g.max_size.to_le_bytes());
            groups.extend(b);
        }
        let mut devs = Vec::new();
        for d in &self.block_devices {
            let mut b = vec![0u8; BLOCK_DEVICE_SIZE];
            b[0..8].copy_from_slice(&d.first_logical_sector.to_le_bytes());
            b[8..12].copy_from_slice(&d.alignment.to_le_bytes());
            b[12..16].copy_from_slice(&d.alignment_offset.to_le_bytes());
            b[16..24].copy_from_slice(&d.size.to_le_bytes());
            put_name(&mut b[24..60], &d.name)?;
            b[60..64].copy_from_slice(&d.flags.to_le_bytes());
            devs.extend(b);
        }
        let tables = [&parts[..], &extents, &groups, &devs].concat();
        let hs = self.header_size as usize;
        let mut h = vec![0u8; hs];
        h[0..4].copy_from_slice(&HEADER_MAGIC.to_le_bytes());
        h[4..6].copy_from_slice(&MAJOR.to_le_bytes());
        h[6..8].copy_from_slice(&self.minor.to_le_bytes());
        h[8..12].copy_from_slice(&(hs as u32).to_le_bytes());
        h[44..48].copy_from_slice(&(tables.len() as u32).to_le_bytes());
        h[48..80].copy_from_slice(&Sha256::digest(&tables));
        let mut off = 0u32;
        for (i, (len, size)) in [
            (parts.len(), PARTITION_SIZE),
            (extents.len(), EXTENT_SIZE),
            (groups.len(), GROUP_SIZE),
            (devs.len(), BLOCK_DEVICE_SIZE),
        ]
        .into_iter()
        .enumerate()
        {
            let d = 80 + i * 12;
            h[d..d + 4].copy_from_slice(&off.to_le_bytes());
            h[d + 4..d + 8].copy_from_slice(&((len / size) as u32).to_le_bytes());
            h[d + 8..d + 12].copy_from_slice(&(size as u32).to_le_bytes());
            off += len as u32;
        }
        if hs >= 132 {
            h[128..132].copy_from_slice(&self.flags.to_le_bytes());
        }
        let sum = Sha256::digest(&h);
        h[12..44].copy_from_slice(&sum);
        let mut out = h;
        out.extend(tables);
        if out.len() > self.geometry.metadata_max_size as usize {
            return Err(invalid("liblp metadata does not fit its slot"));
        }
        Ok(out)
    }

    /// The geometry block (4096 bytes).
    pub fn encode_geometry(&self) -> Vec<u8> {
        let g = self.geometry;
        let mut b = vec![0u8; GEOMETRY_SIZE as usize];
        b[0..4].copy_from_slice(&GEOMETRY_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&(GEOMETRY_STRUCT as u32).to_le_bytes());
        b[40..44].copy_from_slice(&g.metadata_max_size.to_le_bytes());
        b[44..48].copy_from_slice(&g.slot_count.to_le_bytes());
        b[48..52].copy_from_slice(&g.logical_block_size.to_le_bytes());
        let sum = Sha256::digest(&b[..GEOMETRY_STRUCT]);
        b[8..40].copy_from_slice(&sum);
        b
    }

    /// Everything before the partition data of a super image: reserved
    /// bytes, both geometries, and every metadata slot twice.
    pub fn encode_head(&self) -> io::Result<Vec<u8>> {
        let g = self.geometry;
        let max = g.metadata_max_size as usize;
        let slots = g.slot_count as usize;
        let meta = self.encode()?;
        let mut out = vec![0u8; RESERVED as usize];
        let geo = self.encode_geometry();
        out.extend(&geo);
        out.extend(&geo);
        for _ in 0..2 * slots {
            let at = out.len();
            out.extend(&meta);
            out.resize(at + max, 0);
        }
        if out.len() as u64 > self.block_devices[0].first_logical_sector * SECTOR {
            return Err(invalid("liblp metadata overlaps the partitions"));
        }
        Ok(out)
    }
}

/// A super image read as raw: sparse images are read through
/// [`crate::sparse::SparseReader`].
pub enum SuperReader<R> {
    Raw(R),
    Sparse(crate::sparse::SparseReader<R>),
}

impl<R: Read + Seek> SuperReader<R> {
    pub fn open(mut r: R) -> io::Result<Self> {
        let mut head = [0u8; 4];
        r.seek(SeekFrom::Start(0))?;
        r.read_exact(&mut head)?;
        if crate::sparse::is_sparse(&head) {
            Ok(SuperReader::Sparse(crate::sparse::SparseReader::new(r)?))
        } else {
            Ok(SuperReader::Raw(r))
        }
    }

    pub fn is_sparse(&self) -> bool {
        matches!(self, SuperReader::Sparse(_))
    }
}

impl<R: Read + Seek> Read for SuperReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            SuperReader::Raw(r) => r.read(buf),
            SuperReader::Sparse(r) => r.read(buf),
        }
    }
}

impl<R: Read + Seek> Seek for SuperReader<R> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        match self {
            SuperReader::Raw(r) => r.seek(to),
            SuperReader::Sparse(r) => r.seek(to),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn pixel_super_empty() {
        let m = read_empty(include_bytes!("../testdata/pixel_super_empty.img")).unwrap();
        assert_eq!(m.groups.len(), 3);
        assert_eq!(m.partitions[0].name, "system_a");
        assert!(m.partitions.iter().all(|p| p.extents.is_empty()));
        // encoding what was read gives the same metadata back
        let enc = m.encode().unwrap();
        assert_eq!(parse_metadata(&enc, m.geometry).unwrap(), m);
    }

    #[test]
    fn super_round_trip() {
        let mut m = read_empty(include_bytes!("../testdata/pixel_super_empty.img")).unwrap();
        let lbs = m.geometry.logical_block_size as u64;
        let sizes: Vec<u64> = (0..m.partitions.len() as u64)
            .map(|i| (i % 3) * 5 * lbs)
            .collect();
        m.relayout(&sizes).unwrap();
        let head = m.encode_head().unwrap();
        let mut img = head.clone();
        img.resize(
            (m.block_devices[0].first_logical_sector * SECTOR) as usize + 64,
            0,
        );
        let back = read_super(&mut Cursor::new(&img), 0).unwrap();
        assert_eq!(back, m);
        let mut prev_end = 0;
        for (p, s) in back.partitions.iter().zip(&sizes) {
            assert_eq!(p.size(), *s);
            if let Some(e) = p.extents.first() {
                assert!(e.target_data >= prev_end);
                assert_eq!(
                    e.target_data * SECTOR % (m.block_devices[0].alignment as u64).max(lbs),
                    0
                );
                prev_end = e.target_data + e.sectors;
            }
        }
        // damage the primary metadata: the backup is used
        let slot0 = (RESERVED + 2 * GEOMETRY_SIZE) as usize;
        img[slot0 + 20] ^= 1;
        assert_eq!(read_super(&mut Cursor::new(&img), 0).unwrap(), m);
        // too big for the group
        let huge = vec![1 << 40; m.partitions.len()];
        assert!(m.relayout(&huge).is_err());
    }
}
