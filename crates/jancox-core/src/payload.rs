//! A/B OTA `payload.bin` (update_engine), full OTAs only:
//!
//! ```text
//! "CrAU" | version (u64 BE) | manifest size (u64 BE)
//!        | metadata signature size (u32 BE, version 2 only)
//! manifest (DeltaArchiveManifest protobuf) | metadata signature
//! data blobs (operations point into them) | payload signature
//! ```
//!
//! Every partition is a list of operations, each writing its decoded blob
//! to `dst_extents` (blocks of the partition). A full OTA only uses
//! REPLACE (plain), REPLACE_BZ, REPLACE_XZ, ZSTD, ZERO and DISCARD, so each
//! operation stands on its own. Incremental OTAs read the old partition and
//! are refused. Field numbers are from AOSP `update_metadata.proto`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};

use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::factory::{self, Group};
use crate::fs::{invalid, Window};
use crate::proto::{self, Writer};

pub const MAGIC: &[u8; 4] = b"CrAU";
/// Name of the payload in an OTA zip.
pub const ENTRY: &str = "payload.bin";

pub const REPLACE: u32 = 0;
pub const REPLACE_BZ: u32 = 1;
pub const ZERO: u32 = 6;
pub const DISCARD: u32 = 7;
pub const REPLACE_XZ: u32 = 8;
pub const ZSTD: u32 = 14;

fn op_name(kind: u32) -> String {
    let name = match kind {
        0 => "REPLACE",
        1 => "REPLACE_BZ",
        2 => "MOVE",
        3 => "BSDIFF",
        4 => "SOURCE_COPY",
        5 => "SOURCE_BSDIFF",
        6 => "ZERO",
        7 => "DISCARD",
        8 => "REPLACE_XZ",
        9 => "PUFFDIFF",
        10 => "BROTLI_BSDIFF",
        11 => "ZUCCHINI",
        12 => "LZ4DIFF_BSDIFF",
        13 => "LZ4DIFF_PUFFDIFF",
        14 => "ZSTD",
        _ => return format!("operation type {}", kind),
    };
    name.to_string()
}

/// `blocks` blocks from block `start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub start: u64,
    pub blocks: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Operation {
    pub kind: u32,
    /// Blob position, relative to the start of the data blobs.
    pub data_offset: u64,
    pub data_length: u64,
    pub src_extents: Vec<Extent>,
    pub dst_extents: Vec<Extent>,
    /// SHA-256 of the blob (empty when not set).
    pub data_sha256: Vec<u8>,
}

impl Operation {
    /// Blocks the operation writes.
    pub fn dst_blocks(&self) -> u64 {
        self.dst_extents.iter().map(|e| e.blocks).sum()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartitionUpdate {
    pub name: String,
    /// Size of the new partition image in bytes.
    pub size: u64,
    /// SHA-256 of the new partition image (empty when not set).
    pub hash: Vec<u8>,
    /// Incremental OTAs describe the old partition too.
    pub has_old_info: bool,
    pub ops: Vec<Operation>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub block_size: u32,
    /// 0 for a full OTA.
    pub minor_version: u32,
    pub partial_update: bool,
    pub partitions: Vec<PartitionUpdate>,
    /// Groups of the super partition (dynamic partitions), without slot
    /// suffix; empty on devices without dynamic partitions.
    pub groups: Vec<Group>,
    /// Virtual A/B (`snapshot_enabled`).
    pub snapshot_enabled: bool,
}

fn extent(msg: &[u8]) -> io::Result<Extent> {
    let mut e = Extent {
        start: 0,
        blocks: 0,
    };
    for (f, v) in proto::fields(msg)? {
        match f {
            1 => e.start = v.as_u64()?,
            2 => e.blocks = v.as_u64()?,
            _ => {}
        }
    }
    Ok(e)
}

fn operation(msg: &[u8]) -> io::Result<Operation> {
    let mut op = Operation::default();
    for (f, v) in proto::fields(msg)? {
        match f {
            1 => op.kind = v.as_u64()? as u32,
            2 => op.data_offset = v.as_u64()?,
            3 => op.data_length = v.as_u64()?,
            4 => op.src_extents.push(extent(v.as_bytes()?)?),
            6 => op.dst_extents.push(extent(v.as_bytes()?)?),
            8 => op.data_sha256 = v.as_bytes()?.to_vec(),
            _ => {}
        }
    }
    Ok(op)
}

/// PartitionInfo: (size, hash)
fn partition_info(msg: &[u8]) -> io::Result<(u64, Vec<u8>)> {
    let mut info = (0, Vec::new());
    for (f, v) in proto::fields(msg)? {
        match f {
            1 => info.0 = v.as_u64()?,
            2 => info.1 = v.as_bytes()?.to_vec(),
            _ => {}
        }
    }
    Ok(info)
}

fn partition_update(msg: &[u8]) -> io::Result<PartitionUpdate> {
    let mut p = PartitionUpdate::default();
    for (f, v) in proto::fields(msg)? {
        match f {
            1 => p.name = v.as_string()?,
            6 => p.has_old_info = true,
            7 => (p.size, p.hash) = partition_info(v.as_bytes()?)?,
            8 => p.ops.push(operation(v.as_bytes()?)?),
            _ => {}
        }
    }
    Ok(p)
}

fn dynamic_metadata(msg: &[u8], m: &mut Manifest) -> io::Result<()> {
    for (f, v) in proto::fields(msg)? {
        match f {
            1 => {
                let mut g = Group {
                    name: String::new(),
                    max_size: 0,
                    partitions: Vec::new(),
                };
                for (f, v) in proto::fields(v.as_bytes()?)? {
                    match f {
                        1 => g.name = v.as_string()?,
                        2 => g.max_size = v.as_u64()?,
                        3 => g.partitions.push(v.as_string()?),
                        _ => {}
                    }
                }
                m.groups.push(g);
            }
            2 => m.snapshot_enabled = v.as_u64()? != 0,
            _ => {}
        }
    }
    Ok(())
}

impl Manifest {
    pub fn parse(msg: &[u8]) -> io::Result<Manifest> {
        let mut m = Manifest {
            block_size: 4096,
            ..Manifest::default()
        };
        for (f, v) in proto::fields(msg)? {
            match f {
                // install_operations / kernel_install_operations: major
                // version 1 (Chrome OS) payloads
                1 | 2 => return Err(invalid("payload.bin: old (major version 1) payload")),
                3 => m.block_size = v.as_u64()? as u32,
                12 => m.minor_version = v.as_u64()? as u32,
                13 => m.partitions.push(partition_update(v.as_bytes()?)?),
                15 => dynamic_metadata(v.as_bytes()?, &mut m)?,
                // ASUS reuses 16 and up for strings of its own
                16 => m.partial_update = matches!(v, proto::Value::Varint(1)),
                _ => {}
            }
        }
        if !m.block_size.is_power_of_two() || m.block_size < 512 {
            return Err(invalid(format!(
                "payload.bin: bad block size {}",
                m.block_size
            )));
        }
        Ok(m)
    }

    pub fn partition(&self, name: &str) -> Option<&PartitionUpdate> {
        self.partitions.iter().find(|p| p.name == name)
    }

    /// Refuses incremental OTAs and operations we can't decode.
    pub fn check_full(&self) -> io::Result<()> {
        let incremental = || {
            invalid("this is an incremental OTA (it patches the installed ROM); only full OTAs can be unpacked")
        };
        if self.minor_version != 0 {
            return Err(incremental());
        }
        for p in &self.partitions {
            if p.has_old_info {
                return Err(incremental());
            }
            for op in &p.ops {
                match op.kind {
                    REPLACE | REPLACE_BZ | REPLACE_XZ | ZSTD | ZERO | DISCARD
                        if op.src_extents.is_empty() => {}
                    k => {
                        return Err(invalid(format!(
                            "payload.bin: {}: {} is not supported",
                            p.name,
                            op_name(k)
                        )))
                    }
                }
            }
        }
        Ok(())
    }
}

/// Encodes an extent list field.
fn put_extents(w: &mut Writer, field: u32, extents: &[Extent]) {
    for e in extents {
        let mut m = Writer::new();
        m.varint(1, e.start).varint(2, e.blocks);
        w.message(field, &m);
    }
}

impl Operation {
    pub fn encode(&self) -> Writer {
        let mut w = Writer::new();
        w.varint(1, self.kind as u64);
        if self.data_length > 0 {
            w.varint(2, self.data_offset).varint(3, self.data_length);
        }
        put_extents(&mut w, 4, &self.src_extents);
        put_extents(&mut w, 6, &self.dst_extents);
        if !self.data_sha256.is_empty() {
            w.bytes(8, &self.data_sha256);
        }
        w
    }
}

/// The payload header and manifest.
#[derive(Debug, Clone)]
pub struct Payload {
    pub version: u64,
    pub manifest: Manifest,
    /// Header + manifest + metadata signature: where the blobs start.
    pub data_offset: u64,
    /// Header + manifest: the bytes `METADATA_HASH` covers.
    pub metadata_size: u64,
}

impl Payload {
    /// Reads the header and manifest from the start of `r`.
    pub fn read(r: &mut impl Read) -> io::Result<Payload> {
        let mut head = [0u8; 24];
        r.read_exact(&mut head[..20])?;
        if &head[..4] != MAGIC {
            return Err(invalid("not a payload.bin (no CrAU magic)"));
        }
        let version = u64::from_be_bytes(head[4..12].try_into().unwrap());
        let manifest_len = u64::from_be_bytes(head[12..20].try_into().unwrap());
        let (head_len, signature_len) = match version {
            1 => (20, 0),
            2 => {
                r.read_exact(&mut head[20..24])?;
                (
                    24,
                    u32::from_be_bytes(head[20..24].try_into().unwrap()) as u64,
                )
            }
            v => return Err(invalid(format!("payload.bin: unknown version {}", v))),
        };
        if manifest_len > 256 << 20 {
            return Err(invalid("payload.bin: manifest too big"));
        }
        let mut manifest = vec![0u8; manifest_len as usize];
        r.read_exact(&mut manifest)?;
        Ok(Payload {
            version,
            manifest: Manifest::parse(&manifest)?,
            data_offset: head_len + manifest_len + signature_len,
            metadata_size: head_len + manifest_len,
        })
    }
}

/// Byte range of the payload in `path`: all of it for a bare payload.bin,
/// or the `payload.bin` entry of an OTA zip (it must be stored).
pub fn locate(path: &Path) -> io::Result<(u64, u64)> {
    let mut file = File::open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", path.display(), e)))?;
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_ok() && &magic == MAGIC {
        return Ok((0, file.metadata()?.len()));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut zip = ZipArchive::new(BufReader::new(file)).map_err(io::Error::from)?;
    let index = zip
        .index_for_name(ENTRY)
        .ok_or_else(|| invalid(format!("{}: no {}", path.display(), ENTRY)))?;
    factory::stored_range(&mut zip, index)?
        .ok_or_else(|| invalid(format!("{} is compressed inside the zip", ENTRY)))
}

/// Decodes the blob of one operation into the bytes of its dst_extents.
/// ZERO and DISCARD give an empty vector.
fn decode(op: &Operation, data: Vec<u8>, block_size: u64) -> io::Result<Vec<u8>> {
    if !op.data_sha256.is_empty() && Sha256::digest(&data)[..] != op.data_sha256[..] {
        return Err(invalid("blob hash mismatch (corrupt payload)"));
    }
    let want = op.dst_blocks() * block_size;
    let read_all = |r: Box<dyn Read + '_>| -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(want as usize);
        r.take(want + 1).read_to_end(&mut out)?;
        Ok(out)
    };
    let out = match op.kind {
        ZERO | DISCARD => return Ok(Vec::new()),
        REPLACE => data,
        REPLACE_BZ => read_all(Box::new(bzip2::read::MultiBzDecoder::new(&data[..])))?,
        REPLACE_XZ => read_all(Box::new(lzma_rust2::XzReader::new(&data[..], true)))?,
        ZSTD => read_all(Box::new(
            ruzstd::decoding::StreamingDecoder::new(&data[..])
                .map_err(|e| invalid(format!("zstd: {}", e)))?,
        ))?,
        k => return Err(invalid(format!("{} is not supported", op_name(k)))),
    };
    if out.len() as u64 != want {
        return Err(invalid(format!(
            "{} gave {} bytes for {} blocks",
            op_name(op.kind),
            out.len(),
            op.dst_blocks()
        )));
    }
    Ok(out)
}

/// Writes partition `part` of the payload into `out`, decoding its
/// operations on `threads` threads, and checks its SHA-256. `payload` reads
/// the whole payload.bin (e.g. a [`Window`] into the OTA zip).
pub fn dump_partition<R: Read + Seek + Send>(
    payload: &mut R,
    info: &Payload,
    part: &PartitionUpdate,
    out: &mut File,
    threads: usize,
) -> io::Result<()> {
    let bs = info.manifest.block_size as u64;
    let err = |e: io::Error| io::Error::new(e.kind(), format!("{}: {}", part.name, e));
    for op in &part.ops {
        for e in &op.dst_extents {
            let end = e
                .start
                .checked_add(e.blocks)
                .and_then(|b| b.checked_mul(bs));
            if end.is_none_or(|end| end > part.size) {
                return Err(invalid(format!(
                    "{}: an operation writes past the end of the partition",
                    part.name
                )));
            }
        }
    }
    out.set_len(0)?;
    out.set_len(part.size)?;
    out.seek(SeekFrom::Start(0))?;

    let threads = threads.max(1);
    let in_flight = threads * 2;
    let data_offset = info.data_offset;
    let ops = &part.ops;
    let (job_tx, job_rx) = mpsc::sync_channel::<(usize, Vec<u8>)>(threads);
    // shared by the decoders; dropped with the last one, which stops the
    // reader when they quit early
    let job_rx = Arc::new(Mutex::new(job_rx));
    let (res_tx, res_rx) = mpsc::channel::<(usize, io::Result<Vec<u8>>)>();
    // the reader takes a credit per operation, the writer gives it back
    // once written: at most `in_flight` blobs are in memory
    let (credit_tx, credit_rx) = mpsc::sync_channel::<()>(in_flight);
    for _ in 0..in_flight {
        credit_tx.send(()).unwrap();
    }

    let mut hasher = Sha256::new();
    let mut hashed = 0u64;
    let mut in_order = true;
    std::thread::scope(|s| -> io::Result<()> {
        let reader = s.spawn(move || -> io::Result<()> {
            for (i, op) in ops.iter().enumerate() {
                if credit_rx.recv().is_err() {
                    break;
                }
                let mut data = vec![0u8; op.data_length as usize];
                if op.data_length > 0 {
                    payload.seek(SeekFrom::Start(data_offset + op.data_offset))?;
                    payload.read_exact(&mut data)?;
                }
                if job_tx.send((i, data)).is_err() {
                    break;
                }
            }
            Ok(())
        });
        for _ in 0..threads {
            let res_tx = res_tx.clone();
            let job_rx = Arc::clone(&job_rx);
            s.spawn(move || loop {
                let job = job_rx.lock().unwrap().recv();
                let Ok((i, data)) = job else { break };
                if res_tx.send((i, decode(&ops[i], data, bs))).is_err() {
                    break;
                }
            });
        }
        drop(res_tx);
        drop(job_rx);

        // write in operation order, hashing while the extents are in order
        let zeros = vec![0u8; 1 << 16];
        let mut pending = BTreeMap::new();
        let mut next = 0;
        let written = (|| -> io::Result<()> {
            while next < ops.len() {
                let Ok((i, data)) = res_rx.recv() else {
                    return Err(io::Error::other("the payload reader stopped"));
                };
                pending.insert(i, data);
                while let Some(data) = pending.remove(&next) {
                    let data = data?;
                    let op = &ops[next];
                    let mut at = 0usize;
                    for e in &op.dst_extents {
                        let pos = e.start * bs;
                        let len = (e.blocks * bs) as usize;
                        let zero = data.is_empty();
                        if !zero {
                            out.seek(SeekFrom::Start(pos))?;
                            out.write_all(&data[at..at + len])?;
                        }
                        if in_order && pos == hashed {
                            if zero {
                                let mut left = len;
                                while left > 0 {
                                    let n = left.min(zeros.len());
                                    hasher.update(&zeros[..n]);
                                    left -= n;
                                }
                            } else {
                                hasher.update(&data[at..at + len]);
                            }
                            hashed += len as u64;
                        } else {
                            in_order = false;
                        }
                        if !zero {
                            at += len;
                        }
                    }
                    next += 1;
                    let _ = credit_tx.send(());
                }
            }
            Ok(())
        })();
        drop(res_rx);
        drop(credit_tx);
        let read = reader.join().expect("payload reader panicked");
        // a reader error is the cause of a stopped writer
        read.and(written)
    })
    .map_err(err)?;
    out.flush()?;

    if part.hash.is_empty() {
        return Ok(());
    }
    let digest = if in_order && hashed == part.size {
        hasher.finalize()
    } else {
        // extents out of order or holes left: hash the file
        out.seek(SeekFrom::Start(0))?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut r = (&mut *out).take(part.size);
        loop {
            match r.read(&mut buf)? {
                0 => break,
                n => h.update(&buf[..n]),
            }
        }
        h.finalize()
    };
    if digest[..] != part.hash[..] {
        return Err(invalid(format!(
            "{}: SHA-256 of the image does not match the payload",
            part.name
        )));
    }
    Ok(())
}

/// Opens the payload in `path` (payload.bin or an OTA zip) as a reader of
/// its own, and reads its manifest.
pub fn open(path: &Path) -> io::Result<(Window<BufReader<File>>, Payload)> {
    let (start, len) = locate(path)?;
    let mut r = factory::open_window(path, start, len)?;
    let info = Payload::read(&mut r)?;
    if info.data_offset > len {
        return Err(invalid("payload.bin: truncated"));
    }
    Ok((r, info))
}

/// Threads for decoding: the number of CPUs.
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A payload with one partition per (name, image), each image as 4 KiB
    /// blocks: plain, xz, bzip2, zstd and zero chunks.
    pub fn make_payload(parts: &[(&str, &[u8])], groups: &[Group]) -> Vec<u8> {
        let bs = 4096usize;
        let mut blobs = Vec::new();
        let mut m = Writer::new();
        m.varint(3, bs as u64).varint(12, 0);
        for (name, image) in parts {
            assert_eq!(image.len() % bs, 0);
            let mut p = Writer::new();
            p.string(1, name);
            let mut info = Writer::new();
            info.varint(1, image.len() as u64)
                .bytes(2, &Sha256::digest(image));
            p.message(7, &info);
            // two blocks per operation, in reverse order to test hashing
            let chunks: Vec<(usize, &[u8])> = image.chunks(2 * bs).enumerate().collect();
            for (i, chunk) in chunks.into_iter().rev() {
                let kind = if chunk.iter().all(|&b| b == 0) {
                    ZERO
                } else {
                    [REPLACE, REPLACE_XZ, REPLACE_BZ, ZSTD][i % 4]
                };
                let data = match kind {
                    REPLACE => chunk.to_vec(),
                    REPLACE_XZ => {
                        let mut w = lzma_rust2::XzWriter::new(
                            Vec::new(),
                            lzma_rust2::XzOptions::with_preset(1),
                        )
                        .unwrap();
                        w.write_all(chunk).unwrap();
                        w.finish().unwrap()
                    }
                    REPLACE_BZ => {
                        let mut w =
                            bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
                        w.write_all(chunk).unwrap();
                        w.finish().unwrap()
                    }
                    ZSTD => ruzstd::encoding::compress_to_vec(
                        chunk,
                        ruzstd::encoding::CompressionLevel::Fastest,
                    ),
                    _ => Vec::new(),
                };
                let op = Operation {
                    kind,
                    data_offset: blobs.len() as u64,
                    data_length: data.len() as u64,
                    src_extents: Vec::new(),
                    dst_extents: vec![Extent {
                        start: (i * 2) as u64,
                        blocks: (chunk.len() / bs) as u64,
                    }],
                    data_sha256: if data.is_empty() {
                        Vec::new()
                    } else {
                        Sha256::digest(&data).to_vec()
                    },
                };
                blobs.extend_from_slice(&data);
                p.message(8, &op.encode());
            }
            m.message(13, &p);
        }
        if !groups.is_empty() {
            let mut d = Writer::new();
            for g in groups {
                let mut gw = Writer::new();
                gw.string(1, &g.name).varint(2, g.max_size);
                for p in &g.partitions {
                    gw.string(3, p);
                }
                d.message(1, &gw);
            }
            d.varint(2, 1);
            m.message(15, &d);
        }
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&2u64.to_be_bytes());
        out.extend_from_slice(&(m.buf.len() as u64).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&m.buf);
        out.extend_from_slice(&blobs);
        out
    }

    fn image(blocks: usize, seed: u8) -> Vec<u8> {
        let mut img: Vec<u8> = (0..blocks * 4096)
            .map(|i| {
                (i as u32)
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add(seed as u32) as u8
                    % 7
            })
            .collect();
        // a zero chunk (blocks 2-3)
        img[2 * 4096..4 * 4096].fill(0);
        img
    }

    #[test]
    fn dump_round_trip() {
        let boot = image(7, 1);
        let system = image(12, 2);
        let groups = [Group {
            name: "main".into(),
            max_size: 1 << 30,
            partitions: vec!["system".into()],
        }];
        let bin = make_payload(&[("boot", &boot), ("system", &system)], &groups);
        let mut r = io::Cursor::new(bin.clone());
        let info = Payload::read(&mut r).unwrap();
        let m = &info.manifest;
        m.check_full().unwrap();
        assert_eq!(m.block_size, 4096);
        assert_eq!(m.groups, groups);
        assert!(m.snapshot_enabled);
        assert_eq!(m.partitions.len(), 2);

        let dir = std::env::temp_dir().join(format!("jancox-payload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, want) in [("boot", &boot), ("system", &system)] {
            let path = dir.join(format!("{}.img", name));
            let mut f = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            dump_partition(&mut r, &info, m.partition(name).unwrap(), &mut f, 3).unwrap();
            assert_eq!(&std::fs::read(&path).unwrap(), want);
        }

        // a corrupt blob is caught by its hash
        let mut bad = bin.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        let mut r = io::Cursor::new(bad);
        let path = dir.join("bad.img");
        let mut f = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let p = m.partition("system").unwrap();
        assert!(dump_partition(&mut r, &info, p, &mut f, 2).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_incremental() {
        let mut m = Manifest {
            block_size: 4096,
            ..Manifest::default()
        };
        m.partitions.push(PartitionUpdate {
            name: "system".into(),
            ops: vec![Operation {
                kind: 4, // SOURCE_COPY
                ..Operation::default()
            }],
            ..PartitionUpdate::default()
        });
        assert!(m.check_full().is_err());
        m.partitions[0].ops[0].kind = REPLACE;
        assert!(m.check_full().is_ok());
        m.minor_version = 8;
        assert!(m.check_full().is_err());
        assert!(Payload::read(&mut &b"PK\x03\x04................"[..]).is_err());
    }
}
