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

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::factory::{self, Group};
use crate::fs::{invalid, Window};
use crate::proto::{self, Writer};
use crate::sign::Key;

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
    /// The PartitionUpdate as read, for the fields a new payload keeps.
    pub raw: Vec<u8>,
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
    /// The manifest as read, for the fields a new payload keeps.
    pub raw: Vec<u8>,
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
    let mut p = PartitionUpdate {
        raw: msg.to_vec(),
        ..PartitionUpdate::default()
    };
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
            raw: msg.to_vec(),
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

    let data_offset = info.data_offset;
    let ops = &part.ops;
    let mut next_read = 0;
    let mut next_write = 0;
    let mut hasher = Sha256::new();
    let mut hashed = 0u64;
    let mut in_order = true;
    let zeros = vec![0u8; 1 << 16];
    crate::par::pipeline(
        threads,
        || {
            let Some(op) = ops.get(next_read) else {
                return Ok(None);
            };
            let mut data = vec![0u8; op.data_length as usize];
            if op.data_length > 0 {
                payload.seek(SeekFrom::Start(data_offset + op.data_offset))?;
                payload.read_exact(&mut data)?;
            }
            next_read += 1;
            Ok(Some((next_read - 1, data)))
        },
        |(i, data)| decode(&ops[i], data, bs),
        // write in operation order, hashing while the extents are in order
        |data| {
            let op = &ops[next_write];
            next_write += 1;
            let zero = data.is_empty();
            let mut at = 0usize;
            for e in &op.dst_extents {
                let pos = e.start * bs;
                let len = (e.blocks * bs) as usize;
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
            Ok(())
        },
    )
    .map_err(err)?;
    if next_write != ops.len() {
        return Err(err(io::Error::other("not all operations were written")));
    }
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
        crate::sign::hash_reader(&mut (&mut *out).take(part.size), |b| h.update(b))?;
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

/// Size of the pieces a new image is cut into (`full_update_generator.cc`).
pub const CHUNK_SIZE: usize = 2 << 20;

/// xz as AOSP's payload generator writes it: no integrity check (each blob
/// has a SHA-256) and a dictionary no bigger than a chunk.
fn xz(chunk: &[u8], level: u32) -> io::Result<Vec<u8>> {
    let mut opts = lzma_rust2::XzOptions::with_preset(level);
    opts.set_check_sum_type(lzma_rust2::CheckType::None);
    opts.lzma_options.dict_size = opts.lzma_options.dict_size.min(CHUNK_SIZE as u32);
    let mut w = lzma_rust2::XzWriter::new(Vec::with_capacity(chunk.len() / 2), opts)?;
    w.write_all(chunk)?;
    w.finish()
}

/// A partition of a new payload.
#[derive(Debug, Clone, Default)]
pub struct NewPartition {
    pub name: String,
    pub size: u64,
    pub hash: Vec<u8>,
    pub ops: Vec<Operation>,
    /// Rebuilt (the old hashtree, FEC and COW estimates no longer apply)
    /// or taken over from the old payload.
    pub rebuilt: bool,
}

/// Cuts an image into `CHUNK_SIZE` REPLACE_XZ operations (REPLACE where xz
/// doesn't shrink a chunk, as AOSP does) on `threads` threads, and appends
/// their blobs to `data`, which already holds `*data_len` bytes. The image
/// is padded with zeros to a whole block.
pub fn encode_image<R: Read + Send>(
    name: &str,
    image: &mut R,
    block_size: u64,
    level: u32,
    threads: usize,
    data: &mut impl Write,
    data_len: &mut u64,
) -> io::Result<NewPartition> {
    let bs = block_size as usize;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut ops = Vec::new();
    let mut chunk_index = 0u64;
    crate::par::pipeline(
        threads,
        || {
            let mut chunk = vec![0u8; CHUNK_SIZE];
            let mut n = 0;
            while n < chunk.len() {
                match image.read(&mut chunk[n..])? {
                    0 => break,
                    k => n += k,
                }
            }
            if n == 0 {
                return Ok(None);
            }
            chunk.truncate(n.div_ceil(bs) * bs);
            hasher.update(&chunk);
            size += chunk.len() as u64;
            chunk_index += 1;
            Ok(Some((chunk_index - 1, chunk)))
        },
        |(i, chunk)| {
            let blocks = (chunk.len() / bs) as u64;
            let packed = xz(&chunk, level)?;
            let (kind, blob) = if packed.len() < chunk.len() {
                (REPLACE_XZ, packed)
            } else {
                (REPLACE, chunk)
            };
            let op = Operation {
                kind,
                data_offset: 0,
                data_length: blob.len() as u64,
                src_extents: Vec::new(),
                dst_extents: vec![Extent {
                    start: i * (CHUNK_SIZE / bs) as u64,
                    blocks,
                }],
                data_sha256: Sha256::digest(&blob).to_vec(),
            };
            Ok((op, blob))
        },
        |(mut op, blob)| {
            op.data_offset = *data_len;
            data.write_all(&blob)?;
            *data_len += blob.len() as u64;
            ops.push(op);
            Ok(())
        },
    )
    .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", name, e)))?;
    Ok(NewPartition {
        name: name.to_string(),
        size,
        hash: hasher.finalize().to_vec(),
        ops,
        rebuilt: true,
    })
}

/// Copies the operations of `part` and their blobs from the old payload,
/// appending the blobs to `data`.
pub fn copy_partition<R: Read + Seek>(
    payload: &mut R,
    info: &Payload,
    part: &PartitionUpdate,
    data: &mut impl Write,
    data_len: &mut u64,
) -> io::Result<NewPartition> {
    let mut ops = Vec::with_capacity(part.ops.len());
    for op in &part.ops {
        let mut op = op.clone();
        if op.data_length > 0 {
            payload.seek(SeekFrom::Start(info.data_offset + op.data_offset))?;
            let copied = io::copy(&mut (&mut *payload).take(op.data_length), data)?;
            if copied != op.data_length {
                return Err(invalid(format!("{}: payload.bin is truncated", part.name)));
            }
            op.data_offset = *data_len;
            *data_len += op.data_length;
        }
        ops.push(op);
    }
    Ok(NewPartition {
        name: part.name.clone(),
        size: part.size,
        hash: part.hash.clone(),
        ops,
        rebuilt: false,
    })
}

/// Encodes the `Signatures` message holding one signature.
fn signatures_blob(signature: &[u8]) -> Vec<u8> {
    let mut sig = Writer::new();
    sig.bytes(2, signature).fixed32(3, signature.len() as u32);
    let mut w = Writer::new();
    w.message(1, &sig);
    w.buf
}

/// Size of the metadata and payload signature blobs for `key`.
pub fn signatures_size(key: &Key) -> u64 {
    signatures_blob(&vec![0u8; key.signature_len()]).len() as u64
}

// PartitionUpdate fields that only describe the old image: hashtree and
// FEC (update_engine would write them), merge operations and the COW
// estimates of Virtual A/B
const HASH_TREE_FIELDS: std::ops::RangeInclusive<u32> = 10..=16;
const MERGE_OPERATIONS: u32 = 18;
const ESTIMATE_COW_SIZE: u32 = 19;
const ESTIMATE_OP_COUNT_MAX: u32 = 20;

/// Encodes a PartitionUpdate: the fields of `old` (e.g. `version`,
/// postinstall) with the new image info and operations.
fn encode_partition(
    old: Option<&PartitionUpdate>,
    new: &NewPartition,
    bs: u64,
) -> io::Result<Writer> {
    let mut w = Writer::new();
    let mut had_cow_estimate = false;
    let mut had_op_count = false;
    match old {
        Some(old) => {
            for (f, v) in proto::fields(&old.raw)? {
                match f {
                    // old image info, new image info, operations, and the
                    // (unused) per-partition signatures
                    5..=8 => {}
                    ESTIMATE_COW_SIZE if new.rebuilt => had_cow_estimate = true,
                    ESTIMATE_OP_COUNT_MAX if new.rebuilt => had_op_count = true,
                    MERGE_OPERATIONS if new.rebuilt => {}
                    f if new.rebuilt && HASH_TREE_FIELDS.contains(&f) => {}
                    f => {
                        w.value(f, &v);
                    }
                }
            }
        }
        None => {
            w.string(1, &new.name);
        }
    }
    let mut info = Writer::new();
    info.varint(1, new.size).bytes(2, &new.hash);
    w.message(7, &info);
    for op in &new.ops {
        w.message(8, &op.encode());
    }
    // a full image needs a COW as big as the image, plus the operation
    // headers: estimate on the safe side
    if had_cow_estimate {
        let blocks = new.size.div_ceil(bs);
        w.varint(ESTIMATE_COW_SIZE, new.size + blocks * 32 + (2 << 20));
    }
    if had_op_count {
        w.varint(ESTIMATE_OP_COUNT_MAX, new.size.div_ceil(bs) + 16);
    }
    Ok(w)
}

/// Encodes a new manifest: the fields of `old` (block size, timestamps,
/// dynamic partition groups, apex info, OEM fields, ...) with new
/// partitions and signature location.
pub fn encode_manifest(
    old: &Manifest,
    parts: &[NewPartition],
    signatures_offset: u64,
    signatures_size: u64,
) -> io::Result<Vec<u8>> {
    let bs = old.block_size as u64;
    let mut w = Writer::new();
    let mut parts_written = false;
    for (f, v) in proto::fields(&old.raw)? {
        match f {
            4 | 5 => {}
            13 => {
                // the partitions go where the old ones were
                if !parts_written {
                    for p in parts {
                        let enc = encode_partition(old.partition(&p.name), p, bs)?;
                        w.message(13, &enc);
                    }
                    parts_written = true;
                }
            }
            f => {
                w.value(f, &v);
            }
        }
    }
    if !parts_written {
        for p in parts {
            let enc = encode_partition(old.partition(&p.name), p, bs)?;
            w.message(13, &enc);
        }
    }
    w.varint(4, signatures_offset).varint(5, signatures_size);
    Ok(w.buf)
}

/// `payload_properties.txt` of a payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Properties {
    pub file_hash: [u8; 32],
    pub file_size: u64,
    pub metadata_hash: [u8; 32],
    pub metadata_size: u64,
    /// Header + manifest + metadata signature (`payload_metadata.bin`).
    pub metadata_with_signature: u64,
}

impl Properties {
    pub fn text(&self) -> String {
        format!(
            "FILE_HASH={}\nFILE_SIZE={}\nMETADATA_HASH={}\nMETADATA_SIZE={}\n",
            crate::sign::base64(&self.file_hash),
            self.file_size,
            crate::sign::base64(&self.metadata_hash),
            self.metadata_size
        )
    }
}

/// Size of the payload `write_payload` makes.
pub fn payload_size(manifest: &[u8], data_len: u64, key: &Key) -> u64 {
    24 + manifest.len() as u64 + 2 * signatures_size(key) + data_len
}

/// Writes a signed payload.bin: header, `manifest` (made by
/// [`encode_manifest`] with `signatures_offset = data_len`), metadata
/// signature, the `data_len` bytes of blobs from `data`, payload signature.
pub fn write_payload(
    out: &mut impl Write,
    manifest: &[u8],
    data: &mut impl Read,
    data_len: u64,
    key: &Key,
) -> io::Result<Properties> {
    let sig_size = signatures_size(key);
    let mut head = Vec::with_capacity(24 + manifest.len());
    head.extend_from_slice(MAGIC);
    head.extend_from_slice(&2u64.to_be_bytes());
    head.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
    head.extend_from_slice(&(sig_size as u32).to_be_bytes());
    head.extend_from_slice(manifest);
    let metadata_hash: [u8; 32] = Sha256::digest(&head).into();
    let metadata_sig = signatures_blob(&key.sign_sha256(&metadata_hash)?);

    // the payload signature covers the metadata and the blobs, without the
    // metadata signature; FILE_HASH covers every byte
    let mut payload_hash = Sha256::new();
    let mut file_hash = Sha256::new();
    payload_hash.update(&head);
    file_hash.update(&head);
    file_hash.update(&metadata_sig);
    out.write_all(&head)?;
    out.write_all(&metadata_sig)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut left = data_len;
    while left > 0 {
        let n = (left as usize).min(buf.len());
        data.read_exact(&mut buf[..n])?;
        payload_hash.update(&buf[..n]);
        file_hash.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        left -= n as u64;
    }
    let payload_sig = signatures_blob(&key.sign_sha256(&payload_hash.finalize())?);
    file_hash.update(&payload_sig);
    out.write_all(&payload_sig)?;
    Ok(Properties {
        file_hash: file_hash.finalize().into(),
        file_size: payload_size(manifest, data_len, key),
        metadata_hash,
        metadata_size: head.len() as u64,
        metadata_with_signature: (head.len() + metadata_sig.len()) as u64,
    })
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
    fn repack_round_trip() {
        let boot = image(7, 1);
        let system = image(12, 2);
        let groups = [Group {
            name: "main".into(),
            max_size: 1 << 30,
            partitions: vec!["system".into()],
        }];
        let bin = make_payload(&[("boot", &boot), ("system", &system)], &groups);
        let mut r = io::Cursor::new(bin);
        let info = Payload::read(&mut r).unwrap();
        let m = &info.manifest;

        // boot copied as is, system replaced by a new image
        let new_system = image(9, 3);
        let key = Key::test_key().unwrap();
        let mut data = Vec::new();
        let mut len = 0;
        let parts = vec![
            copy_partition(
                &mut r,
                &info,
                m.partition("boot").unwrap(),
                &mut data,
                &mut len,
            )
            .unwrap(),
            encode_image(
                "system",
                &mut &new_system[..],
                4096,
                0,
                2,
                &mut data,
                &mut len,
            )
            .unwrap(),
        ];
        assert_eq!(len, data.len() as u64);
        let manifest = encode_manifest(m, &parts, len, signatures_size(&key)).unwrap();
        let mut out = Vec::new();
        let props = write_payload(&mut out, &manifest, &mut &data[..], len, &key).unwrap();
        assert_eq!(props.file_size, out.len() as u64);
        assert_eq!(props.file_hash[..], Sha256::digest(&out)[..]);
        assert_eq!(
            props.metadata_hash[..],
            Sha256::digest(&out[..props.metadata_size as usize])[..]
        );
        assert!(props.text().starts_with("FILE_HASH="));

        let mut r2 = io::Cursor::new(out.clone());
        let info2 = Payload::read(&mut r2).unwrap();
        let m2 = &info2.manifest;
        m2.check_full().unwrap();
        assert_eq!(m2.groups, groups);
        assert_eq!(info2.data_offset, props.metadata_with_signature);
        let dir = std::env::temp_dir().join(format!("jancox-repack-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, want) in [("boot", &boot), ("system", &new_system)] {
            let path = dir.join(name);
            let mut f = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            dump_partition(&mut r2, &info2, m2.partition(name).unwrap(), &mut f, 2).unwrap();
            assert_eq!(&std::fs::read(&path).unwrap(), want);
        }
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
