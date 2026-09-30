//! Compressed EROFS files: mapping logical ranges to physical clusters
//! (compact and full lcluster indexes, the newer extent records, big
//! pclusters, ztailpacking, fragments in the packed inode, partial
//! references from dedupe) and decompressing them (lz4, MicroLZMA, raw
//! deflate, zstd, and uncompressed "plain" pclusters).
//!
//! The mapping follows erofs-utils `lib/zmap.c` and the decompression
//! `lib/decompress.c` (both "GPL-2.0+ OR MIT"; used here under MIT):
//! Copyright (C) 2018-2019 HUAWEI, Inc., Gao Xiang; Copyright (C)
//! 2008-2020 OPPO Mobile Comm Corp., Ltd., Huang Jianan.

use std::io::{self, Read, Seek, Write};

use super::{le16, le32, le64, Erofs, Inode};
use super::{LAYOUT_COMPRESSED_COMPACT, LAYOUT_COMPRESSED_FULL};
use crate::fs::invalid;

pub(super) const LZ4: u8 = 0;
pub(super) const LZMA: u8 = 1;
pub(super) const DEFLATE: u8 = 2;
pub(super) const ZSTD: u8 = 3;
/// Uncompressed pclusters (not on disk as algorithm numbers).
const SHIFTED: u8 = 4;
const INTERLACED: u8 = 5;

pub(super) const ALG_NAMES: [&str; 4] = ["lz4", "lzma", "deflate", "zstd"];

const ADVISE_COMPACTED_2B: u16 = 0x1;
const ADVISE_EXTENTS: u16 = 0x1;
const ADVISE_BIG_PCLUSTER_1: u16 = 0x2;
const ADVISE_BIG_PCLUSTER_2: u16 = 0x4;
const ADVISE_INLINE_PCLUSTER: u16 = 0x8;
const ADVISE_INTERLACED_PCLUSTER: u16 = 0x10;
const ADVISE_FRAGMENT_PCLUSTER: u16 = 0x20;
const FRAGMENT_INODE_BIT: u8 = 7;

const TYPE_PLAIN: u8 = 0;
const TYPE_HEAD1: u8 = 1;
const TYPE_NONHEAD: u8 = 2;
const TYPE_HEAD2: u8 = 3;

const LI_PARTIAL_REF: u16 = 1 << 15;
const LI_D0_CBLKCNT: u16 = 1 << 11;

const EXTENT_PLEN_PARTIAL: u32 = 1 << 27;
const EXTENT_PLEN_FMT_BIT: u32 = 28;
const PCLUSTER_MAX_SIZE: u64 = 1 << 20;
const EXTENT_PLEN_MASK: u32 = ((PCLUSTER_MAX_SIZE << 1) - 1) as u32;
const PCLUSTER_MAX_DSIZE: u64 = 12 << 20;
/// Largest dictionary MicroLZMA may use (8 * max pcluster size).
pub(super) const LZMA_MAX_DICT: u32 = 8 << 20;

/// Compression settings from the superblock.
#[derive(Debug, Clone, Default)]
pub(super) struct Config {
    /// Bitmap of algorithms used (bit n = algorithm n).
    pub algs: u16,
    /// lz4: maximum pcluster size in blocks (big pclusters when > 1).
    pub max_pclusterblks: u16,
}

/// Per-inode compression header (`z_erofs_map_header`), read once.
#[derive(Debug, Clone, Default)]
pub(super) struct ZInfo {
    advise: u16,
    algs: [u8; 2],
    lclusterbits: u32,
    /// ztailpacking: bytes of the inline compressed tail.
    idata_size: u64,
    /// ztailpacking: where the inline tail is; fragments: its offset in
    /// the packed inode.
    fragmentoff: u64,
    tailextent_headlcn: u64,
    /// The whole file is a fragment of the packed inode.
    whole_fragment: bool,
    extents: u64,
}

/// One mapped extent (`erofs_map_blocks`).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Map {
    pub la: u64,
    pub llen: u64,
    pub pa: u64,
    pub plen: u64,
    pub mapped: bool,
    pub fragment: bool,
    pub partial: bool,
    pub alg: u8,
}

/// State while walking lcluster indexes (`z_erofs_maprecorder`).
#[derive(Debug, Default)]
struct Rec {
    lcn: u64,
    kind: u8,
    headtype: u8,
    clusterofs: u64,
    delta: [u64; 2],
    pblk: u64,
    compressedblks: u64,
    nextpackoff: u64,
    partialref: bool,
}

fn corrupt(inode: &Inode, what: &str) -> io::Error {
    invalid(format!(
        "inode {}: corrupt compressed file ({})",
        inode.nid, what
    ))
}

/// `lobits` bits of value and 2 bits of type at bit `pos` of a pack.
fn compacted_bits(lobits: u32, pack: &[u8], pos: u32) -> io::Result<(u32, u8)> {
    let at = (pos / 8) as usize;
    let b = pack
        .get(at..at + 4)
        .ok_or_else(|| invalid("corrupt compressed index"))?;
    let v = u32::from_le_bytes(b.try_into().unwrap()) >> (pos & 7);
    Ok((v & ((1 << lobits) - 1), ((v >> lobits) & 3) as u8))
}

fn la_distance(
    lobits: u32,
    encodebits: u32,
    vcnt: u32,
    pack: &[u8],
    mut i: u32,
) -> io::Result<u64> {
    let mut d1 = 0u64;
    let mut lo;
    loop {
        let (l, t) = compacted_bits(lobits, pack, encodebits * i)?;
        lo = l;
        if t != TYPE_NONHEAD {
            return Ok(d1);
        }
        d1 += 1;
        i += 1;
        if i >= vcnt {
            break;
        }
    }
    // the last lcluster of the pack keeps delta[1]
    if lo & LI_D0_CBLKCNT as u32 == 0 {
        d1 = (d1 + lo as u64).saturating_sub(1);
    }
    Ok(d1)
}

impl<R: Read + Seek> Erofs<R> {
    /// Reads the compression header of an inode and, for ztailpacking and
    /// fragments, where its tail extent starts.
    pub(super) fn zinfo(&mut self, inode: &Inode) -> io::Result<ZInfo> {
        let pos = self.after_inode(inode).next_multiple_of(8);
        let h = self.read_vec(pos, 8)?;
        if h[7] >> FRAGMENT_INODE_BIT != 0 {
            return Ok(ZInfo {
                advise: ADVISE_FRAGMENT_PCLUSTER,
                fragmentoff: le64(&h, 0) ^ (1 << 63),
                whole_fragment: true,
                ..ZInfo::default()
            });
        }
        let mut z = ZInfo {
            advise: le16(&h, 4),
            lclusterbits: self.blkszbits() + (h[7] & 15) as u32,
            ..ZInfo::default()
        };
        if inode.layout == LAYOUT_COMPRESSED_FULL && z.advise & ADVISE_EXTENTS != 0 {
            z.extents = le32(&h, 0) as u64 | (le16(&h, 6) as u64) << 32;
            return Ok(z);
        }
        z.algs = [h[6] & 15, h[6] >> 4];
        if z.advise & ADVISE_FRAGMENT_PCLUSTER != 0 {
            z.fragmentoff = le32(&h, 0) as u64;
        } else if z.advise & ADVISE_INLINE_PCLUSTER != 0 {
            z.idata_size = le16(&h, 2) as u64;
        }
        if z.lclusterbits > 30 {
            return Err(corrupt(inode, "cluster size"));
        }
        if inode.layout == LAYOUT_COMPRESSED_COMPACT
            && (z.advise & ADVISE_BIG_PCLUSTER_1 == 0) != (z.advise & ADVISE_BIG_PCLUSTER_2 == 0)
        {
            return Err(corrupt(inode, "inconsistent big pcluster flags"));
        }
        if z.idata_size > 0 || z.advise & ADVISE_FRAGMENT_PCLUSTER != 0 {
            // find the tail extent
            let size = inode.size;
            if size > 0 {
                self.map_fo(inode, &mut z, size - 1, true, false)?;
            }
        }
        Ok(z)
    }

    fn blkszbits(&self) -> u32 {
        self.sb.block_size.trailing_zeros()
    }

    fn load_full(&mut self, inode: &Inode, z: &ZInfo, lcn: u64, m: &mut Rec) -> io::Result<()> {
        let pos = (self.after_inode(inode).next_multiple_of(8) + 8 + 8) + lcn * 8;
        let di = self.read_vec(pos, 8)?;
        m.lcn = lcn;
        m.nextpackoff = pos + 8;
        let advise = le16(&di, 0);
        m.kind = (advise & 3) as u8;
        if m.kind == TYPE_NONHEAD {
            m.clusterofs = 1 << z.lclusterbits;
            let d0 = le16(&di, 4);
            m.delta[0] = d0 as u64;
            if d0 & LI_D0_CBLKCNT != 0 {
                if z.advise & (ADVISE_BIG_PCLUSTER_1 | ADVISE_BIG_PCLUSTER_2) == 0 {
                    return Err(corrupt(inode, "CBLKCNT without big pclusters"));
                }
                m.compressedblks = (d0 & !LI_D0_CBLKCNT) as u64;
                m.delta[0] = 1;
            }
            m.delta[1] = le16(&di, 6) as u64;
        } else {
            m.partialref = advise & LI_PARTIAL_REF != 0;
            m.clusterofs = le16(&di, 2) as u64;
            m.pblk = le32(&di, 4) as u64;
        }
        Ok(())
    }

    fn load_compact(
        &mut self,
        inode: &Inode,
        z: &ZInfo,
        lcn: u64,
        lookahead: bool,
        m: &mut Rec,
    ) -> io::Result<()> {
        let ebase = 8 + self.after_inode(inode).next_multiple_of(8);
        let lclusterbits = z.lclusterbits;
        let totalidx = inode.size.div_ceil(self.sb.block_size);
        let big = z.advise & ADVISE_BIG_PCLUSTER_1 != 0;
        if lcn >= totalidx || lclusterbits > 14 {
            return Err(corrupt(inode, "lcluster out of range"));
        }
        m.lcn = lcn;
        let initial_4b = ((32 - ebase % 32) / 4) & 7;
        let mut compacted_2b = 0;
        if z.advise & ADVISE_COMPACTED_2B != 0 && initial_4b < totalidx {
            compacted_2b = (totalidx - initial_4b) / 16 * 16;
        }
        let mut pos = ebase;
        let mut shift = 2u32;
        let mut l = lcn;
        if l >= initial_4b {
            pos += initial_4b * 4;
            l -= initial_4b;
            if l < compacted_2b {
                shift = 1;
            } else {
                pos += compacted_2b * 2;
                l -= compacted_2b;
            }
        }
        pos += l << shift;
        let vcnt: u32 = match shift {
            2 if lclusterbits <= 14 => 2,
            1 if lclusterbits <= 12 => 16,
            _ => return Err(corrupt(inode, "unsupported compact index")),
        };
        let packsize = (vcnt as u64) << shift;
        let packstart = pos / packsize * packsize;
        let pack = self.read_vec(packstart, packsize)?;
        m.nextpackoff = packstart + packsize;
        let lobits = lclusterbits.max(LI_D0_CBLKCNT.trailing_zeros() + 1);
        let encodebits = (((packsize - 4) * 8) >> vcnt.trailing_zeros()) as u32;
        let i = ((pos - packstart) >> shift) as i64;

        let (lo, kind) = compacted_bits(lobits, &pack, encodebits * i as u32)?;
        m.kind = kind;
        if kind == TYPE_NONHEAD {
            m.clusterofs = 1 << lclusterbits;
            if lookahead {
                m.delta[1] = la_distance(lobits, encodebits, vcnt, &pack, i as u32)?;
            }
            if lo & LI_D0_CBLKCNT as u32 != 0 {
                if !big {
                    return Err(corrupt(inode, "CBLKCNT without big pclusters"));
                }
                m.compressedblks = (lo & !(LI_D0_CBLKCNT as u32)) as u64;
                m.delta[0] = 1;
                return Ok(());
            } else if i + 1 != vcnt as i64 {
                m.delta[0] = lo as u64;
                return Ok(());
            }
            // the last lcluster of a pack keeps delta[1]: delta[0] comes
            // from the one before
            let (lo, t) = compacted_bits(lobits, &pack, encodebits * (i as u32 - 1))?;
            let lo = if t != TYPE_NONHEAD {
                0
            } else if lo & LI_D0_CBLKCNT as u32 != 0 {
                1
            } else {
                lo
            };
            m.delta[0] = lo as u64 + 1;
            return Ok(());
        }
        m.clusterofs = lo as u64;
        m.delta[0] = 0;
        // blkaddr of a HEAD: the pack's base address plus the blocks of
        // the heads before it in the pack
        let mut nblk: i64;
        let mut i = i;
        if !big {
            nblk = 1;
            while i > 0 {
                i -= 1;
                let (lo, t) = compacted_bits(lobits, &pack, encodebits * i as u32)?;
                if t == TYPE_NONHEAD {
                    i -= lo as i64;
                }
                if i >= 0 {
                    nblk += 1;
                }
            }
        } else {
            nblk = 0;
            while i > 0 {
                i -= 1;
                let (lo, t) = compacted_bits(lobits, &pack, encodebits * i as u32)?;
                if t == TYPE_NONHEAD {
                    if lo & LI_D0_CBLKCNT as u32 != 0 {
                        i -= 1;
                        nblk += (lo & !(LI_D0_CBLKCNT as u32)) as i64;
                        continue;
                    }
                    if lo <= 1 {
                        return Err(corrupt(inode, "bad big pcluster index"));
                    }
                    i -= lo as i64 - 2;
                    continue;
                }
                nblk += 1;
            }
        }
        let base = le32(&pack, packsize as usize - 4) as i64;
        m.pblk = (base + nblk) as u64;
        Ok(())
    }

    fn load_lcluster(
        &mut self,
        inode: &Inode,
        z: &ZInfo,
        lcn: u64,
        lookahead: bool,
        m: &mut Rec,
    ) -> io::Result<()> {
        if inode.layout == LAYOUT_COMPRESSED_COMPACT {
            self.load_compact(inode, z, lcn, lookahead, m)?;
        } else {
            self.load_full(inode, z, lcn, m)?;
        }
        if m.kind != TYPE_NONHEAD && m.clusterofs >= 1 << z.lclusterbits {
            return Err(corrupt(inode, "cluster offset"));
        }
        Ok(())
    }

    fn lookback(
        &mut self,
        inode: &Inode,
        z: &ZInfo,
        mut dist: u64,
        m: &mut Rec,
        map: &mut Map,
    ) -> io::Result<()> {
        while m.lcn >= dist {
            let lcn = m.lcn - dist;
            self.load_lcluster(inode, z, lcn, false, m)?;
            if m.kind == TYPE_NONHEAD {
                dist = m.delta[0];
                if dist == 0 {
                    break;
                }
                continue;
            }
            m.headtype = m.kind;
            map.la = (lcn << z.lclusterbits) | m.clusterofs;
            return Ok(());
        }
        Err(corrupt(inode, "bogus lookback distance"))
    }

    fn compressed_len(
        &mut self,
        inode: &Inode,
        z: &ZInfo,
        m: &mut Rec,
        map: &mut Map,
    ) -> io::Result<()> {
        let big1 = z.advise & ADVISE_BIG_PCLUSTER_1 != 0;
        let big2 = z.advise & ADVISE_BIG_PCLUSTER_2 != 0;
        let lcn = m.lcn + 1;
        if (m.headtype == TYPE_HEAD1 && !big1)
            || ((m.headtype == TYPE_PLAIN || m.headtype == TYPE_HEAD2) && !big2)
            || (lcn << z.lclusterbits) >= inode.size
        {
            m.compressedblks = 1;
        }
        if m.compressedblks == 0 {
            self.load_lcluster(inode, z, lcn, false, m)?;
            if m.kind == TYPE_NONHEAD {
                if m.delta[0] != 1 {
                    return Err(corrupt(inode, "bogus CBLKCNT"));
                }
                if m.compressedblks == 0 {
                    return Err(corrupt(inode, "no CBLKCNT"));
                }
            } else {
                // the next lcluster is a HEAD or PLAIN: one block
                m.compressedblks = 1;
            }
        }
        map.plen = m.compressedblks << self.blkszbits();
        Ok(())
    }

    fn decompressed_len(
        &mut self,
        inode: &Inode,
        z: &ZInfo,
        m: &mut Rec,
        map: &mut Map,
    ) -> io::Result<()> {
        let bits = z.lclusterbits;
        let mut lcn = m.lcn;
        let headlcn = map.la >> bits;
        loop {
            if (lcn << bits) >= inode.size {
                map.llen = inode.size - map.la;
                return Ok(());
            }
            self.load_lcluster(inode, z, lcn, true, m)?;
            if m.kind == TYPE_NONHEAD {
                // pre-1.0 mkfs wrote zero delta[1]
                if m.delta[1] == 0 {
                    m.delta[1] = 1;
                }
            } else {
                if lcn != headlcn {
                    break;
                }
                m.delta[1] = 1;
            }
            lcn += m.delta[1];
        }
        map.llen = ((lcn << bits) + m.clusterofs)
            .checked_sub(map.la)
            .ok_or_else(|| corrupt(inode, "extent length"))?;
        Ok(())
    }

    /// Maps the extent holding byte `la` (`z_erofs_map_blocks_fo`). With
    /// `fiemap`, `llen` is the whole extent, not only up to `la`'s lcluster.
    fn map_fo(
        &mut self,
        inode: &Inode,
        z: &mut ZInfo,
        la: u64,
        findtail: bool,
        fiemap: bool,
    ) -> io::Result<Map> {
        let fragment = z.advise & ADVISE_FRAGMENT_PCLUSTER != 0;
        let ztailpacking = z.idata_size > 0;
        let bits = z.lclusterbits;
        let mut map = Map {
            la,
            ..Map::default()
        };
        if fragment && !findtail && z.tailextent_headlcn == 0 {
            map.la = 0;
            map.llen = inode.size;
            map.fragment = true;
            map.mapped = true;
            return Ok(map);
        }
        let mut m = Rec::default();
        let initial_lcn = la >> bits;
        let endoff = la & ((1 << bits) - 1);
        self.load_lcluster(inode, z, initial_lcn, false, &mut m)?;
        if findtail && ztailpacking {
            z.fragmentoff = m.nextpackoff;
        }
        map.mapped = true;
        let mut end = (m.lcn + 1) << bits;
        match m.kind {
            TYPE_PLAIN | TYPE_HEAD1 | TYPE_HEAD2 if endoff >= m.clusterofs => {
                m.headtype = m.kind;
                map.la = (m.lcn << bits) | m.clusterofs;
                // ztailpacking EOF lclusters can hold three parts
                if ztailpacking && end > inode.size {
                    end = inode.size;
                }
            }
            TYPE_PLAIN | TYPE_HEAD1 | TYPE_HEAD2 => {
                if m.lcn == 0 {
                    return Err(corrupt(inode, "invalid logical cluster 0"));
                }
                end = (m.lcn << bits) | m.clusterofs;
                self.lookback(inode, z, 1, &mut m, &mut map)?;
            }
            TYPE_NONHEAD => {
                let d = m.delta[0];
                self.lookback(inode, z, d, &mut m, &mut map)?;
            }
            t => {
                return Err(corrupt(inode, &format!("unknown lcluster type {}", t)));
            }
        }
        map.partial = m.partialref;
        map.llen = end
            .checked_sub(map.la)
            .ok_or_else(|| corrupt(inode, "extent ends before it starts"))?;
        if findtail {
            z.tailextent_headlcn = m.lcn;
            // full indexes keep the high 32 bits of the fragment offset
            if fragment && inode.layout == LAYOUT_COMPRESSED_FULL {
                z.fragmentoff |= m.pblk << 32;
            }
        }
        if ztailpacking && m.lcn == z.tailextent_headlcn {
            map.pa = z.fragmentoff;
            map.plen = z.idata_size;
            if map.pa % self.sb.block_size + map.plen > self.sb.block_size {
                return Err(corrupt(inode, "inline tail crosses a block"));
            }
        } else if fragment && m.lcn == z.tailextent_headlcn {
            map.fragment = true;
        } else {
            map.pa = m.pblk << self.blkszbits();
            self.compressed_len(inode, z, &mut m, &mut map)?;
        }
        map.alg = match m.headtype {
            TYPE_PLAIN => {
                if map.llen > map.plen && !map.fragment {
                    return Err(corrupt(inode, "plain extent longer than its pcluster"));
                }
                if z.advise & ADVISE_INTERLACED_PCLUSTER != 0 {
                    INTERLACED
                } else {
                    SHIFTED
                }
            }
            TYPE_HEAD2 => z.algs[1],
            _ => z.algs[0],
        };
        if fiemap {
            self.decompressed_len(inode, z, &mut m, &mut map)?;
        }
        Ok(map)
    }

    /// Maps with the extent records of newer mkfs (`z_erofs_map_blocks_ext`).
    fn map_ext(&mut self, inode: &Inode, z: &mut ZInfo, la: u64) -> io::Result<Map> {
        let interlaced = z.advise & ADVISE_INTERLACED_PCLUSTER != 0;
        let recsz = 4u64 << ((z.advise >> 1) & 3);
        let mut pos = (self.after_inode(inode).next_multiple_of(8) + 8).next_multiple_of(recsz);
        let bmask = self.sb.block_size - 1;
        let bits = z.lclusterbits;
        let mut lend = inode.size;
        let mut map = Map {
            la,
            ..Map::default()
        };
        let (mut lstart, last);
        let mut plen = 0u32;
        let mut pa = 0u64;
        if recsz <= 8 {
            let mut seq_pa = None;
            if recsz <= 4 {
                seq_pa = Some(le64(&self.read_vec(pos, 8)?, 0));
                pos += 8;
                lstart = 0;
            } else {
                lstart = la >> bits << bits;
                pos += (lstart >> bits) * recsz;
            }
            while lstart <= la {
                let ext = self.read_vec(pos, recsz)?;
                plen = le32(&ext, 0);
                match &mut seq_pa {
                    Some(p) => {
                        pa = *p;
                        *p += (plen & EXTENT_PLEN_MASK) as u64;
                    }
                    None => pa = le32(&ext, 4) as u64,
                }
                pos += recsz;
                lstart += 1 << bits;
            }
            last = lstart >= inode.size.next_multiple_of(1 << bits);
            lend = lend.min(lstart);
            lstart -= 1 << bits;
        } else {
            lstart = lend;
            let (mut l, mut r) = (0u64, z.extents);
            while l < r {
                let mid = l + (r - l) / 2;
                let ext = self.read_vec(pos + mid * recsz, recsz)?;
                let mut ela = le32(&ext, 12) as u64;
                let epa = le32(&ext, 4) as u64 | (le32(&ext, 8) as u64) << 32;
                if recsz > 16 {
                    ela |= (le32(&ext, 16) as u64) << 32;
                }
                if ela > la {
                    r = mid;
                    if ela > lend {
                        return Err(corrupt(inode, "extent order"));
                    }
                    lend = ela;
                } else {
                    l = mid + 1;
                    if la == ela {
                        r = r.min(l + 1);
                    }
                    lstart = ela;
                    plen = le32(&ext, 0);
                    pa = epa;
                }
            }
            last = l >= z.extents;
        }
        if lstart < lend {
            map.la = lstart;
            if last && z.advise & ADVISE_FRAGMENT_PCLUSTER != 0 {
                map.fragment = true;
                map.mapped = true;
                z.fragmentoff = plen as u64;
                if recsz > 4 {
                    z.fragmentoff |= pa << 32;
                }
            } else if plen & EXTENT_PLEN_MASK != 0 {
                map.mapped = true;
                map.pa = pa;
                let fmt = plen >> EXTENT_PLEN_FMT_BIT;
                map.partial = plen & EXTENT_PLEN_PARTIAL != 0;
                map.plen = (plen & EXTENT_PLEN_MASK) as u64;
                map.alg = if fmt != 0 {
                    (fmt - 1) as u8
                } else if interlaced && (map.pa | map.plen) & bmask == 0 {
                    INTERLACED
                } else {
                    SHIFTED
                };
            }
        }
        map.llen = lend
            .checked_sub(map.la)
            .ok_or_else(|| corrupt(inode, "extent ends before it starts"))?;
        Ok(map)
    }

    /// The whole extent holding byte `la`.
    pub(super) fn zmap(&mut self, inode: &Inode, z: &mut ZInfo, la: u64) -> io::Result<Map> {
        let map = if z.whole_fragment {
            Map {
                la: 0,
                llen: inode.size,
                mapped: true,
                fragment: true,
                ..Map::default()
            }
        } else if inode.layout == LAYOUT_COMPRESSED_FULL && z.advise & ADVISE_EXTENTS != 0 {
            self.map_ext(inode, z, la)?
        } else {
            self.map_fo(inode, z, la, false, true)?
        };
        if map.mapped && !map.fragment {
            if map.alg < SHIFTED && self.zcfg.algs & (1 << map.alg) == 0 {
                return Err(corrupt(inode, "algorithm not in the superblock"));
            }
            if map.plen > PCLUSTER_MAX_SIZE || map.llen > PCLUSTER_MAX_DSIZE {
                return Err(corrupt(inode, "pcluster too big"));
            }
        }
        if map.llen == 0 || map.la > la || la >= map.la.saturating_add(map.llen) {
            return Err(corrupt(inode, "extent does not cover its offset"));
        }
        Ok(map)
    }

    /// The decoded bytes of an extent (`llen` of them).
    fn zextent(&mut self, inode: &Inode, z: &ZInfo, map: &Map) -> io::Result<Vec<u8>> {
        let want = map.llen as usize;
        if !map.mapped {
            return Ok(vec![0; want]);
        }
        if map.fragment {
            if Some(inode.nid) == self.packed_nid() {
                return Err(corrupt(inode, "fragment in the packed inode"));
            }
            return self.packed_read(z.fragmentoff, map.llen);
        }
        let input = self.read_vec(map.pa, map.plen)?;
        decompress(
            map.alg,
            &input,
            want,
            map.la % self.sb.block_size,
            self.sb.block_size as usize,
        )
        .map_err(|e| {
            invalid(format!(
                "inode {}: {} extent at {}: {}",
                inode.nid,
                alg_name(map.alg),
                map.la,
                e
            ))
        })
    }

    /// Writes a compressed file to `out`.
    pub(super) fn zcopy(&mut self, inode: &Inode, out: &mut dyn Write) -> io::Result<u64> {
        let mut z = self.zinfo(inode)?;
        let mut pos = 0;
        while pos < inode.size {
            let map = self.zmap(inode, &mut z, pos)?;
            let data = self.zextent(inode, &z, &map)?;
            out.write_all(&data[(pos - map.la) as usize..])?;
            pos = map.la + map.llen;
        }
        Ok(inode.size)
    }

    /// `len` bytes of a compressed inode from `off`, through a small cache
    /// of decoded extents (the packed inode is read a little at a time).
    pub(super) fn zread(&mut self, inode: &Inode, off: u64, len: u64) -> io::Result<Vec<u8>> {
        let end = off
            .checked_add(len)
            .filter(|&e| e <= inode.size)
            .ok_or_else(|| corrupt(inode, "read past the end"))?;
        let mut z = self.zinfo(inode)?;
        let mut out = Vec::with_capacity(len.min(1 << 20) as usize);
        let mut pos = off;
        while pos < end {
            let key = (inode.nid, pos);
            let cached = self
                .zcache
                .iter()
                .find(|(nid, la, data)| *nid == key.0 && *la <= pos && pos < la + data.len() as u64)
                .map(|(_, la, data)| (*la, data.clone()));
            let (la, data) = match cached {
                Some(c) => c,
                None => {
                    let map = self.zmap(inode, &mut z, pos)?;
                    let data = std::sync::Arc::new(self.zextent(inode, &z, &map)?);
                    if self.zcache.len() >= 16 {
                        self.zcache.remove(0);
                    }
                    self.zcache.push((inode.nid, map.la, data.clone()));
                    (map.la, data)
                }
            };
            let from = (pos - la) as usize;
            let to = ((end - la) as usize).min(data.len());
            out.extend_from_slice(&data[from..to]);
            pos = la + to as u64;
        }
        Ok(out)
    }
}

fn alg_name(alg: u8) -> &'static str {
    match alg {
        SHIFTED | INTERLACED => "plain",
        a => ALG_NAMES.get(a as usize).copied().unwrap_or("unknown"),
    }
}

/// Decodes a pcluster into `want` bytes. `blkoff` is the offset of the
/// extent in its block (interlaced plain pclusters start there).
fn decompress(alg: u8, input: &[u8], want: usize, blkoff: u64, bs: usize) -> io::Result<Vec<u8>> {
    match alg {
        SHIFTED => {
            return input
                .get(..want)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| invalid("plain pcluster too short"));
        }
        INTERLACED => {
            if want > input.len() || input.len() > bs {
                return Err(invalid("bad interlaced pcluster"));
            }
            let skip = blkoff as usize % bs;
            let right = (bs - skip).min(want).min(input.len().saturating_sub(skip));
            let mut out = input[skip..skip + right].to_vec();
            out.extend_from_slice(&input[..want - right]);
            return Ok(out);
        }
        _ => {}
    }
    // 0padding: the compressed data ends at the end of the pcluster
    let margin = input.iter().take_while(|&&b| b == 0).count();
    if margin >= input.len() {
        return Err(invalid("empty pcluster"));
    }
    let src = &input[margin..];
    let read_want = |mut r: Box<dyn Read + '_>| -> io::Result<Vec<u8>> {
        let mut out = vec![0u8; want];
        r.read_exact(&mut out)?;
        Ok(out)
    };
    match alg {
        LZ4 => crate::lz4::decompress(src, want),
        LZMA => {
            // MicroLZMA: the first byte is the negated properties byte in
            // place of the range coder's leading zero
            let props = !src[0];
            let mut stream = Vec::with_capacity(src.len());
            stream.push(0);
            stream.extend_from_slice(&src[1..]);
            // the stream never looks back past its own output
            let dict = (want as u32).clamp(4096, LZMA_MAX_DICT);
            // size unknown to the decoder: a partial reference stops before
            // the end of the stream, and MicroLZMA has no end marker
            let r =
                lzma_rust2::LzmaReader::new_with_props(&stream[..], u64::MAX, props, dict, None)
                    .map_err(|e| invalid(format!("lzma: {}", e)))?;
            read_want(Box::new(r))
        }
        DEFLATE => read_want(Box::new(flate2::read::DeflateDecoder::new(src))),
        ZSTD => read_want(Box::new(
            ruzstd::decoding::StreamingDecoder::new(src)
                .map_err(|e| invalid(format!("zstd: {}", e)))?,
        )),
        a => Err(invalid(format!("unknown compression algorithm {}", a))),
    }
}
