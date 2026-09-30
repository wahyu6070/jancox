//! ROMs whose logical partitions come in one `super.img`: Xiaomi fastboot
//! ROMs (`<device>_images_<version>.tgz` with `images/super.img`, sparse)
//! and zips with a `super.img`.
//!
//! Unpack keeps the archive as it is under `rom/` except `super.img`: its
//! ext4/EROFS partitions go to `partition/`, its metadata and any other
//! logical partition to `super/`. Repack builds the partitions, lays out a
//! new `super.img` with the same metadata (groups, slots, device size),
//! writes it sparse or raw as the original was, patches `vbmeta.img`,
//! renews Xiaomi's CRC lists, and writes the archive in its original order.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::build::{self, Size};
use crate::fs::{invalid, Segments};
use crate::rom::{self, Partition, RepackOptions, State};
use crate::sparse::{self, Fill};
use crate::{extract, factory, lp, tar, xiaomi};

/// `<work>/super/`: the super metadata and the logical partitions that are
/// not extracted.
pub(crate) fn super_dir(work: &Path) -> PathBuf {
    work.join("super")
}
const META: &str = "metadata.img";

/// True for an archive entry that is a super image.
pub(crate) fn is_super_entry(name: &str) -> bool {
    name.rsplit('/').next() == Some("super.img")
}

/// Relative path of an archive entry, or `None` when it would leave the
/// folder (absolute, `..`).
fn safe_rel(path: &str) -> Option<PathBuf> {
    let p = Path::new(path.trim_end_matches('/'));
    let ok = p.components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    });
    (ok && !p.as_os_str().is_empty()).then(|| p.to_path_buf())
}

/// What a super image unpacked to: the partitions, (partition, liblp
/// name) pairs, and whether the image was sparse.
type Unpacked = (Vec<Partition>, Vec<(String, String)>, bool);

/// Extracts the logical partitions of `super_file`.
pub(crate) fn unpack_super_image(
    work: &Path,
    super_file: &Path,
    log: &mut impl FnMut(&str),
) -> io::Result<Unpacked> {
    let file = File::open(super_file)?;
    let mut img = lp::SuperReader::open(BufReader::with_capacity(1 << 20, file))?;
    let sparse = img.is_sparse();
    let m = lp::read_super(&mut img, 0)?;
    let used: Vec<&lp::Partition> = m.partitions.iter().filter(|p| p.size() > 0).collect();
    log(&format!(
        "- super.img: {} MiB{}, {} metadata slots, {} partitions with data",
        m.super_size() >> 20,
        if sparse { " (sparse)" } else { "" },
        m.geometry.slot_count,
        used.len()
    ));
    let sdir = super_dir(work);
    fs::create_dir_all(&sdir)?;
    let mut meta = m.encode_geometry();
    meta.extend(m.encode()?);
    fs::write(sdir.join(META), meta)?;

    let mut parts: Vec<Partition> = Vec::new();
    let mut map = Vec::new();
    let mut kept = Vec::new();
    for p in used {
        let name = factory::strip_slot(&p.name).to_string();
        let segs = m.segments(p)?;
        let mut head = vec![0u8; 2048];
        let n = rom::read_up_to(&mut Segments::new(&mut img, &segs), &mut head)?;
        let fs_type = match extract::detect(&head[..n]) {
            Some(t @ ("ext4" | "erofs")) if !parts.iter().any(|x| x.name == name) => Some(t),
            _ => None,
        };
        match fs_type {
            Some(t) => {
                log(&format!(
                    "- {}: {} image, {} MiB",
                    p.name,
                    t,
                    p.size() >> 20
                ));
                let sum = extract::extract_reader(
                    Segments::new(&mut img, &segs),
                    &rom::partition_dir(work),
                    &name,
                    &mut *log,
                )?;
                rom::log_extracted(&sum, log);
                parts.push(Partition {
                    name: name.clone(),
                    version: 0,
                    brotli: false,
                    size: p.size(),
                    fs: t.to_string(),
                });
                map.push((name, p.name.clone()));
            }
            None => {
                let path = sdir.join(format!("{}.img", p.name));
                let mut out = BufWriter::new(File::create(&path)?);
                io::copy(&mut Segments::new(&mut img, &segs), &mut out)?;
                out.flush()?;
                kept.push(p.name.clone());
            }
        }
    }
    if !kept.is_empty() {
        log(&format!(
            "- Kept as images in {}: {}",
            sdir.display(),
            kept.join(" ")
        ));
    }
    if parts.is_empty() {
        return Err(invalid("no ext4 or EROFS partitions in super.img"));
    }
    Ok((parts, map, sparse))
}

/// Unpacks a `.tgz` / `.tar` ROM holding a super image.
pub(crate) fn unpack_tar(
    input: &Path,
    gz: bool,
    work: &Path,
    log: &mut impl FnMut(&str),
) -> io::Result<(State, usize)> {
    let file = BufReader::with_capacity(1 << 20, File::open(input)?);
    let r: Box<dyn Read> = if gz {
        Box::new(flate2::read::MultiGzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = tar::Reader::new(r);
    let rom_dir = work.join("rom");
    let tmp = work.join("tmp");
    fs::create_dir_all(&rom_dir)?;
    fs::create_dir_all(&tmp)?;
    let super_tmp = tmp.join("super.img");
    let mut entries = Vec::new();
    let mut super_entry = None;
    let mut files = 0;
    log("- Reading the archive");
    while let Some(h) = archive.next_entry()? {
        let Some(rel) = safe_rel(&h.path) else {
            log(&format!("  [warning] skipped unsafe path {}", h.path));
            continue;
        };
        let name = h.path.trim_end_matches('/').to_string();
        let dest = rom_dir.join(&rel);
        match h.kind {
            tar::Kind::Dir => {
                fs::create_dir_all(&dest)?;
            }
            tar::Kind::File if is_super_entry(&name) && super_entry.is_none() => {
                log(&format!("- {}: {} MiB", name, h.size >> 20));
                let mut out = BufWriter::with_capacity(1 << 20, File::create(&super_tmp)?);
                archive.copy_data(h.size, &mut out)?;
                out.flush()?;
                super_entry = Some(name.clone());
            }
            tar::Kind::File => {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut out = BufWriter::new(File::create(&dest)?);
                archive.copy_data(h.size, &mut out)?;
                out.flush()?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(h.mode & 0o777));
                }
                files += 1;
            }
            tar::Kind::Symlink => {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(&h.link, &dest)?;
                #[cfg(not(unix))]
                log(&format!("  [warning] symlink {} left out", name));
            }
            tar::Kind::Other => {
                log(&format!("  [warning] skipped special entry {}", name));
                continue;
            }
        }
        entries.push(name);
    }
    let super_entry = super_entry.ok_or_else(|| {
        invalid("no super.img in this archive; only super.img fastboot ROMs are supported")
    })?;
    log(&format!(
        "- Extracted {} other files to {}",
        files,
        rom_dir.display()
    ));
    let (parts, map, sparse) = unpack_super_image(work, &super_tmp, log)?;
    fs::remove_file(&super_tmp)?;
    let _ = fs::remove_dir(&tmp);
    Ok((
        State {
            format: rom::Format::Super,
            partitions: parts,
            image_zip: super_entry,
            image_entries: entries,
            container: if gz { "tgz" } else { "tar" }.to_string(),
            super_sparse: sparse,
            super_map: map,
        },
        files,
    ))
}

/// Unpacks a zip ROM holding a super image.
pub(crate) fn unpack_zip<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    names: &[String],
    work: &Path,
    log: &mut impl FnMut(&str),
) -> io::Result<(State, usize)> {
    let super_entry = names
        .iter()
        .find(|n| is_super_entry(n))
        .cloned()
        .ok_or_else(|| invalid("no super.img"))?;
    let rom_dir = work.join("rom");
    let files = rom::extract_entries(zip, &rom_dir, |n| n != super_entry, log)?;
    log(&format!(
        "- Extracted {} other files to {}",
        files,
        rom_dir.display()
    ));
    let tmp = work.join("tmp");
    fs::create_dir_all(&tmp)?;
    let super_tmp = tmp.join("super.img");
    {
        let mut entry = zip.by_name(&super_entry).map_err(io::Error::from)?;
        log(&format!("- {}: {} MiB", super_entry, entry.size() >> 20));
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&super_tmp)?);
        io::copy(&mut entry, &mut out)?;
        out.flush()?;
    }
    let (parts, map, sparse) = unpack_super_image(work, &super_tmp, log)?;
    fs::remove_file(&super_tmp)?;
    let _ = fs::remove_dir(&tmp);
    Ok((
        State {
            format: rom::Format::Super,
            partitions: parts,
            image_zip: super_entry,
            image_entries: names.to_vec(),
            container: "zip".to_string(),
            super_sparse: sparse,
            super_map: map,
        },
        files,
    ))
}

/// Builds the partitions and writes a new super image to `out`. Returns
/// the partitions' images (for other outputs).
pub(crate) fn build_super(
    work: &Path,
    state: &State,
    tmp: &Path,
    out: &Path,
    log: &mut impl FnMut(&str),
) -> io::Result<Vec<(String, PathBuf)>> {
    let sdir = super_dir(work);
    let mut m = lp::read_empty(&fs::read(sdir.join(META))?)?;
    let lbs = m.geometry.logical_block_size as u64;
    let parts_dir = rom::partition_dir(work);
    let mut sources: Vec<Option<PathBuf>> = Vec::new();
    let mut sizes = Vec::new();
    for p in &m.partitions {
        let built = state
            .super_map
            .iter()
            .find(|(_, lpn)| *lpn == p.name)
            .map(|(n, _)| n.clone());
        let src = match built {
            Some(name) => {
                let img = tmp.join(format!("{}.img", name));
                match build::build(&parts_dir, &name, &img, Size::Original, &mut *log) {
                    // logical partitions take the size of their image
                    Err(e) if e.kind() == io::ErrorKind::StorageFull => {
                        log(&format!(
                            "- {} is full, growing it (dynamic partition)",
                            name
                        ));
                        build::build(&parts_dir, &name, &img, Size::Auto, &mut *log)?;
                    }
                    other => {
                        other?;
                    }
                }
                Some(img)
            }
            None => Some(sdir.join(format!("{}.img", p.name))).filter(|k| k.is_file()),
        };
        let size = match &src {
            Some(f) => fs::metadata(f)?.len().next_multiple_of(lbs),
            None => 0,
        };
        sizes.push(size);
        sources.push(src);
    }
    m.relayout(&sizes)
        .map_err(|e| rom::compressed_hint(e, work, &state.partitions))?;
    let used: u64 = sizes.iter().sum();
    log(&format!(
        "- super.img: {} MiB of partitions in a {} MiB super partition{}",
        used >> 20,
        m.super_size() >> 20,
        if state.super_sparse { ", sparse" } else { "" }
    ));
    write_super_image(&m, &sources, out, state.super_sparse)?;
    Ok(m.partitions
        .iter()
        .zip(sources)
        .filter_map(|(p, s)| s.map(|s| (p.name.clone(), s)))
        .collect())
}

/// Writes a super image with metadata `m` and each partition's data from
/// `sources` (images, zero-padded to their extents).
pub(crate) fn write_super_image(
    m: &lp::Metadata,
    sources: &[Option<PathBuf>],
    out: &Path,
    sparse_out: bool,
) -> io::Result<()> {
    let head = m.encode_head()?;
    let size = m.super_size();
    // (start, length, file) of each partition's data
    let mut pieces: Vec<(u64, u64, File)> = Vec::new();
    for (p, src) in m.partitions.iter().zip(sources) {
        if let (Some(e), Some(src)) = (p.extents.first(), src) {
            pieces.push((
                e.target_data * lp::SECTOR,
                e.sectors * lp::SECTOR,
                File::open(src)?,
            ));
        }
    }
    pieces.sort_by_key(|p| p.0);
    let mut w = BufWriter::with_capacity(1 << 20, File::create(out)?);
    if sparse_out {
        const BS: u32 = 4096;
        let head_len = (head.len() as u64).next_multiple_of(BS as u64);
        let mut ranges = vec![(0, head_len, Fill::Data)];
        ranges.extend(pieces.iter().map(|p| (p.0, p.1, Fill::Data)));
        let mut src = |pos: u64, buf: &mut [u8]| -> io::Result<()> {
            buf.fill(0);
            if pos < head.len() as u64 {
                let from = pos as usize;
                let n = buf.len().min(head.len() - from);
                buf[..n].copy_from_slice(&head[from..from + n]);
                return Ok(());
            }
            let i = pieces.partition_point(|p| p.0 + p.1 <= pos);
            if let Some((start, _, f)) = pieces.get_mut(i) {
                if *start <= pos {
                    f.seek(SeekFrom::Start(pos - *start))?;
                    let mut n = 0;
                    while n < buf.len() {
                        match f.read(&mut buf[n..])? {
                            0 => break,
                            k => n += k,
                        }
                    }
                }
            }
            Ok(())
        };
        sparse::write_sparse(&mut w, size, BS, &ranges, &mut src)?;
    } else {
        w.write_all(&head)?;
        for (start, _, f) in &mut pieces {
            w.seek(SeekFrom::Start(*start))?;
            io::copy(f, &mut w)?;
        }
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.set_len(size)?;
        return Ok(());
    }
    w.flush()
}

/// Repacks a super ROM into `out_path` (same container as the input).
pub(crate) fn repack(
    work: &Path,
    state: &State,
    out_path: &Path,
    tmp: &Path,
    opts: &RepackOptions,
    log: &mut impl FnMut(&str),
) -> io::Result<Vec<PathBuf>> {
    let super_img = tmp.join("super.img");
    build_super(work, state, tmp, &super_img, log)?;
    let rom_dir = work.join("rom");
    let super_rel = safe_rel(&state.image_zip).ok_or_else(|| invalid("bad super.img path"))?;
    let images = rom_dir
        .join(&super_rel)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| rom_dir.clone());

    // vbmeta flags and the CRC lists live next to super.img; the originals
    // in rom/ are kept, patched copies go to tmp/
    let mut replaced: Vec<(String, PathBuf)> = vec![(state.image_zip.clone(), super_img.clone())];
    let dir_rel = super_rel
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let entry_of = |file: &str| {
        if dir_rel.is_empty() {
            file.to_string()
        } else {
            format!("{}/{}", dir_rel, file)
        }
    };
    let mut changed = vec![("super.img".to_string(), super_img.clone())];
    let vbmeta = images.join("vbmeta.img");
    if vbmeta.is_file() {
        let mut data = fs::read(&vbmeta)?;
        if factory::disable_verification(&mut data)? {
            log("- vbmeta.img: dm-verity and AVB verification disabled");
            log("  (the rebuilt partitions have no hashtree; the device must be unlocked)");
            let p = tmp.join("vbmeta.img");
            fs::write(&p, &data)?;
            replaced.push((entry_of("vbmeta.img"), p.clone()));
            changed.push(("vbmeta.img".to_string(), p));
        }
    }
    // Xiaomi: the bootloader checks the CRCs in these lists
    let crc_lists: Vec<&str> = [xiaomi::CRCLIST, xiaomi::SPARSECRCLIST]
        .into_iter()
        .filter(|l| images.join(l).is_file())
        .collect();
    if !crc_lists.is_empty() {
        let crcdir = tmp.join("crc");
        fs::create_dir_all(&crcdir)?;
        for l in &crc_lists {
            fs::copy(images.join(l), crcdir.join(l))?;
        }
        let script = rom_dir
            .join(&super_rel)
            .ancestors()
            .skip(1)
            .map(|d| d.join(xiaomi::GEN_SCRIPT))
            .find(|p| p.is_file())
            .and_then(|p| fs::read_to_string(p).ok());
        let updated = xiaomi::update_crc_lists(&crcdir, script.as_deref(), &changed)?;
        for l in &crc_lists {
            replaced.push((entry_of(l), crcdir.join(l)));
        }
        if !updated.is_empty() {
            log(&format!(
                "- Xiaomi CRC lists updated: {}",
                updated.join(", ")
            ));
        }
    }

    let out = match (state.container.as_str(), out_path.extension()) {
        ("tgz", Some(e)) if e == "zip" => out_path.with_extension("tgz"),
        ("tar", Some(e)) if e == "zip" => out_path.with_extension("tar"),
        _ => out_path.to_path_buf(),
    };
    let partial = out.with_extension("part");
    let result = match state.container.as_str() {
        "zip" => write_zip(&rom_dir, state, &replaced, &partial, opts, log),
        c => write_tar(&rom_dir, state, &replaced, &partial, c == "tgz", opts, log),
    };
    match result {
        Ok(()) => fs::rename(&partial, &out)?,
        Err(e) => {
            let _ = fs::remove_file(&partial);
            return Err(e);
        }
    }
    Ok(vec![out])
}

/// Entries of the archive in their original order, then files added to
/// `rom/`: (entry name, file to write or None for a folder).
fn plan(
    rom_dir: &Path,
    state: &State,
    replaced: &[(String, PathBuf)],
) -> io::Result<Vec<(String, Option<PathBuf>)>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for name in &state.image_entries {
        let src = match replaced.iter().find(|(n, _)| n == name) {
            Some((_, p)) => Some(p.clone()),
            None => {
                let Some(rel) = safe_rel(name) else { continue };
                let p = rom_dir.join(rel);
                match fs::symlink_metadata(&p) {
                    Ok(m) if m.is_dir() => None,
                    Ok(_) => Some(p),
                    Err(_) => continue,
                }
            }
        };
        seen.insert(name.clone());
        out.push((name.clone(), src));
    }
    let mut files = Vec::new();
    rom::collect_files(rom_dir, rom_dir, &mut files)?;
    for (rel, p) in files {
        if !seen.contains(&rel) {
            out.push((rel, Some(p)));
        }
    }
    Ok(out)
}

fn write_zip(
    rom_dir: &Path,
    state: &State,
    replaced: &[(String, PathBuf)],
    partial: &Path,
    opts: &RepackOptions,
    log: &mut impl FnMut(&str),
) -> io::Result<()> {
    let (deflate, stored) = rom::zip_options(opts);
    let mut zip = zip::ZipWriter::new(BufWriter::with_capacity(1 << 20, File::create(partial)?));
    for (name, src) in plan(rom_dir, state, replaced)? {
        match src {
            None => {
                zip.add_directory(name.as_str(), deflate)
                    .map_err(io::Error::from)?;
            }
            Some(p) => {
                let big = fs::metadata(&p)?.len() >= 64 << 20;
                if big {
                    log(&format!("- Adding {}", name));
                }
                rom::add_file(&mut zip, &name, &p, if big { stored } else { deflate })?;
            }
        }
    }
    zip.finish().map_err(io::Error::from)?.flush()
}

fn write_tar(
    rom_dir: &Path,
    state: &State,
    replaced: &[(String, PathBuf)],
    partial: &Path,
    gz: bool,
    opts: &RepackOptions,
    log: &mut impl FnMut(&str),
) -> io::Result<()> {
    let file = BufWriter::with_capacity(1 << 20, File::create(partial)?);
    let entries = plan(rom_dir, state, replaced)?;
    if gz {
        let enc =
            flate2::write::GzEncoder::new(file, flate2::Compression::new(opts.zip_level as u32));
        tar_entries(enc, &entries, log)?.finish()?.flush()
    } else {
        tar_entries(file, &entries, log)?.flush()
    }
}

fn tar_entries<W: Write>(
    w: W,
    entries: &[(String, Option<PathBuf>)],
    log: &mut impl FnMut(&str),
) -> io::Result<W> {
    let mut t = tar::Writer::new(w);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    for (name, src) in entries {
        match src {
            None => t.dir(name, 0o755, now)?,
            Some(p) => {
                let md = fs::symlink_metadata(p)?;
                if md.file_type().is_symlink() {
                    let target = fs::read_link(p)?;
                    t.symlink(name, &target.to_string_lossy(), now)?;
                    continue;
                }
                if md.len() >= 64 << 20 {
                    log(&format!("- Adding {}", name));
                }
                let mode = if rom::is_executable(&md) {
                    0o755
                } else {
                    0o644
                };
                t.file(
                    name,
                    mode,
                    now,
                    md.len(),
                    &mut BufReader::with_capacity(1 << 20, File::open(p)?),
                )?;
            }
        }
    }
    t.finish()
}
