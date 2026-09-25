//! ROM workflow: unpack a flashable ROM zip into editable folders, repack
//! them into a new zip, and clean up.
//!
//! ```text
//! <work>/rom/                  the rest of the ROM zip (META-INF, boot.img, firmware, ...)
//! <work>/partition/<part>/     extracted partitions, and their metadata in
//! <work>/partition/config/     (see extract.rs)
//! <work>/jancox_rom            what unpack found, for repack (key=value)
//! <work>/tmp/                  images while building
//! <work>/output/NewROM-<date>.zip
//! ```
//!
//! Partitions are block-based OTA data (`<part>.transfer.list` +
//! `<part>.new.dat[.br]`). They are streamed zip -> brotli -> sdat2img ->
//! image and back, so no `.new.dat` is ever written to disk.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::build::{self, Size};
use crate::fs::invalid;
use crate::{br, extract, sdat};

const STATE: &str = "jancox_rom";
const OP_LIST: &str = "dynamic_partitions_op_list";

#[derive(Debug, Clone, Copy)]
pub struct RepackOptions {
    /// Brotli quality (0-11) for `.new.dat.br`.
    pub brotli_quality: u32,
    /// Deflate level (0-9) for the other files in the zip.
    pub zip_level: i64,
}

impl Default for RepackOptions {
    fn default() -> Self {
        RepackOptions {
            brotli_quality: br::DEFAULT_QUALITY,
            zip_level: 1,
        }
    }
}

/// What unpack found about one partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub name: String,
    /// Transfer list version (1-4).
    pub version: u32,
    /// `.new.dat.br` (true) or plain `.new.dat`.
    pub brotli: bool,
    /// Bytes the transfer list covers (the partition size).
    pub size: u64,
}

fn state_path(work: &Path) -> PathBuf {
    work.join(STATE)
}

/// Folder holding the extracted partitions and their `config/`.
pub fn partition_dir(work: &Path) -> PathBuf {
    work.join("partition")
}

fn write_state(work: &Path, input: &Path, parts: &[Partition]) -> io::Result<()> {
    let mut s = format!("input={}\n", input.display());
    let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
    s.push_str(&format!("partitions={}\n", names.join(" ")));
    for p in parts {
        s.push_str(&format!("{}.version={}\n", p.name, p.version));
        s.push_str(&format!("{}.brotli={}\n", p.name, p.brotli));
        s.push_str(&format!("{}.size={}\n", p.name, p.size));
    }
    fs::write(state_path(work), s)
}

/// Partitions recorded by unpack, or `None` when `work` is not unpacked.
pub fn read_state(work: &Path) -> io::Result<Option<Vec<Partition>>> {
    let text = match fs::read_to_string(state_path(work)) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let kv: BTreeMap<&str, &str> = text.lines().filter_map(|l| l.split_once('=')).collect();
    let get = |k: &str| {
        kv.get(k)
            .copied()
            .ok_or_else(|| invalid(format!("{}: missing {}", STATE, k)))
    };
    let mut parts = Vec::new();
    for name in get("partitions")?.split_whitespace() {
        let num = |k: &str| -> io::Result<u64> {
            get(&format!("{}.{}", name, k))?
                .parse()
                .map_err(|_| invalid(format!("{}: bad {}.{}", STATE, name, k)))
        };
        parts.push(Partition {
            name: name.to_string(),
            version: num("version")? as u32,
            brotli: get(&format!("{}.brotli", name))? == "true",
            size: num("size")?,
        });
    }
    Ok(Some(parts))
}

const CONFIG: &str = "jancox.prop";

const DEFAULT_CONFIG: &str = "\
# Jancox tool settings, read by `jancox repack`.
# Command line options (-b, -z) win over these.

# brotli quality for <partition>.new.dat.br: 0 (fastest) - 11 (smallest)
brotli.level=1

# deflate level for the other files in the ROM zip: 0 (store) - 9 (smallest)
zip.level=1
";

/// Creates `input/`, `output/` and a default `jancox.prop` in `work`, each
/// only when missing. Returns what was created.
pub fn init(work: &Path) -> io::Result<Vec<PathBuf>> {
    let mut created = Vec::new();
    for dir in ["input", "output"] {
        let p = work.join(dir);
        if !p.is_dir() {
            fs::create_dir_all(&p)?;
            created.push(p);
        }
    }
    let prop = work.join(CONFIG);
    if !prop.exists() {
        fs::write(&prop, DEFAULT_CONFIG)?;
        created.push(prop);
    }
    Ok(created)
}

/// Repack options from `<work>/jancox.prop`; defaults when it is missing.
pub fn load_config(work: &Path) -> io::Result<RepackOptions> {
    let path = work.join(CONFIG);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(RepackOptions::default()),
        Err(e) => return Err(e),
    };
    let mut opts = RepackOptions::default();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || invalid(format!("{} line {}: {}", CONFIG, n + 1, line));
        let (key, value) = line.split_once('=').ok_or_else(bad)?;
        let value = value.trim();
        match key.trim() {
            "brotli.level" => {
                opts.brotli_quality = value.parse().ok().filter(|q| *q <= 11).ok_or_else(bad)?
            }
            "zip.level" => {
                opts.zip_level = value
                    .parse()
                    .ok()
                    .filter(|z| (0..=9).contains(z))
                    .ok_or_else(bad)?
            }
            // unknown keys are ignored, so newer settings don't break older builds
            _ => {}
        }
    }
    Ok(opts)
}

/// Looks for the ROM zip: `<work>/input/*.zip`, then `<work>/input.zip`.
pub fn find_input(work: &Path) -> Option<PathBuf> {
    if let Ok(dir) = fs::read_dir(work.join("input")) {
        let mut zips: Vec<PathBuf> = dir
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")))
            .collect();
        zips.sort();
        if let Some(z) = zips.into_iter().next() {
            return Some(z);
        }
    }
    Some(work.join("input.zip")).filter(|p| p.is_file())
}

fn zip_err(e: zip::result::ZipError) -> io::Error {
    e.into()
}

#[derive(Debug, Default, Clone)]
pub struct UnpackSummary {
    pub partitions: Vec<Partition>,
    pub other_files: usize,
    /// e.g. ("Android version", "15")
    pub rom_info: Vec<(String, String)>,
}

/// Unpacks `input` into `work`.
pub fn unpack(input: &Path, work: &Path, mut log: impl FnMut(&str)) -> io::Result<UnpackSummary> {
    if state_path(work).exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} is already unpacked; run cleanup first", work.display()),
        ));
    }
    let file = File::open(input)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", input.display(), e)))?;
    let mut zip = ZipArchive::new(BufReader::new(file)).map_err(zip_err)?;
    let names: Vec<String> = zip.file_names().map(str::to_string).collect();
    if names.iter().any(|n| n == "payload.bin") {
        return Err(invalid("payload.bin ROMs are not supported yet"));
    }

    // partitions: <part>.transfer.list with <part>.new.dat[.br]
    let mut parts = Vec::new();
    let mut part_files = std::collections::HashSet::new();
    for n in &names {
        let Some(part) = n.strip_suffix(".transfer.list") else {
            continue;
        };
        let brotli = if names.iter().any(|x| *x == format!("{}.new.dat.br", part)) {
            true
        } else if names.iter().any(|x| *x == format!("{}.new.dat", part)) {
            false
        } else {
            continue;
        };
        for ext in ["transfer.list", "new.dat.br", "new.dat", "patch.dat"] {
            part_files.insert(format!("{}.{}", part, ext));
        }
        parts.push(Partition {
            name: part.to_string(),
            version: 0,
            brotli,
            size: 0,
        });
    }
    if parts.is_empty() {
        return Err(invalid(
            "no *.transfer.list + *.new.dat[.br] partitions in this zip",
        ));
    }
    log(&format!("- ROM: {}", input.display()));
    fs::create_dir_all(work)?;
    if !extract::symlinks_supported(work) {
        log("[!] This partition/storage does not support symlinks (e.g. /sdcard).");
        log("    Symlinks are kept in config/<part>_symlinks and put back on repack.");
        log("    To see them as real symlinks, work in a folder like the Termux home (~).");
    }

    // everything else goes to <work>/rom as is
    let rom_dir = work.join("rom");
    fs::create_dir_all(&rom_dir)?;
    let mut other_files = 0;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(zip_err)?;
        if part_files.contains(entry.name()) {
            continue;
        }
        let Some(rel) = entry.enclosed_name() else {
            log(&format!("  [warning] skipped unsafe path {}", entry.name()));
            continue;
        };
        let path = rom_dir.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = BufWriter::new(File::create(&path)?);
        io::copy(&mut entry, &mut out)?;
        out.flush()?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode & 0o777));
        }
        other_files += 1;
    }
    log(&format!(
        "- Extracted {} other files to {}",
        other_files,
        rom_dir.display()
    ));

    let tmp = work.join("tmp");
    fs::create_dir_all(&tmp)?;
    for p in &mut parts {
        let list = {
            let entry = zip
                .by_name(&format!("{}.transfer.list", p.name))
                .map_err(zip_err)?;
            sdat2img::parse_transfer_list(BufReader::new(entry))
                .map_err(|e| invalid(format!("{}.transfer.list: {}", p.name, e)))?
        };
        p.version = list.version;
        p.size = list.max_file_size().map_err(|e| invalid(e.to_string()))?;
        let dat = format!("{}.new.dat{}", p.name, if p.brotli { ".br" } else { "" });
        log(&format!("- {}: {} -> image", p.name, dat));
        let img = tmp.join(format!("{}.img", p.name));
        {
            let entry = zip.by_name(&dat).map_err(zip_err)?;
            let mut reader: Box<dyn Read> = if p.brotli {
                Box::new(br::decoder(entry))
            } else {
                Box::new(entry)
            };
            sdat::write_img(&list, &mut reader, &img, |_| {})
                .map_err(|e| invalid(format!("{}: {}", dat, e)))?;
        }
        let sum = extract::extract(&img, &partition_dir(work), Some(&p.name), &mut log)?;
        for w in sum.warnings.iter().take(5) {
            log(&format!("  [warning] {}", w));
        }
        let kept = if sum.symlinks_not_created > 0 {
            " (in config only)"
        } else {
            ""
        };
        log(&format!(
            "  {} dirs, {} files, {} symlinks{}",
            sum.dirs, sum.files, sum.symlinks, kept
        ));
        fs::remove_file(&img)?;
    }
    let _ = fs::remove_dir(&tmp);
    write_state(work, input, &parts)?;

    Ok(UnpackSummary {
        rom_info: rom_info(work),
        partitions: parts,
        other_files,
    })
}

fn prop(path: &Path, key: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(str::to_string))
}

/// Android version, ROM name and device from the extracted build.props.
pub fn rom_info(work: &Path) -> Vec<(String, String)> {
    let work = &partition_dir(work);
    let system = [
        work.join("system/system/build.prop"),
        work.join("system/build.prop"),
    ]
    .into_iter()
    .find(|p| p.is_file());
    let vendor = work.join("vendor/build.prop");
    let mut out = Vec::new();
    if let Some(sys) = &system {
        for (label, key) in [
            ("Android version", "ro.build.version.release"),
            ("ROM", "ro.build.display.id"),
        ] {
            if let Some(v) = prop(sys, key) {
                out.push((label.to_string(), v));
            }
        }
        out.push((
            "System-as-root".to_string(),
            sys.ends_with("system/system/build.prop").to_string(),
        ));
    }
    if let Some(v) = prop(&vendor, "ro.product.vendor.device") {
        out.push(("Device".to_string(), v));
    }
    out
}

/// `NewROM-YYYYMMDD-HHMMSS` (UTC).
fn rom_name() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil_from_days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "NewROM-{:04}{:02}{:02}-{:02}{:02}{:02}",
        y,
        m,
        d,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Sets `resize <part> <bytes>` lines and checks the group size limits.
fn update_op_list(text: &str, sizes: &BTreeMap<String, u64>) -> io::Result<String> {
    let mut out = String::new();
    let mut group_max: BTreeMap<String, u64> = BTreeMap::new();
    let mut group_of: BTreeMap<String, String> = BTreeMap::new();
    let mut size_of: BTreeMap<String, u64> = BTreeMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            ["add_group", g, max] => {
                group_max.insert(g.to_string(), max.parse().unwrap_or(u64::MAX));
            }
            ["add", p, g] => {
                group_of.insert(p.to_string(), g.to_string());
            }
            ["resize", p, n] => {
                let new = sizes
                    .get(*p)
                    .copied()
                    .unwrap_or_else(|| n.parse().unwrap_or(0));
                size_of.insert(p.to_string(), new);
                out.push_str(&format!("resize {} {}\n", p, new));
                continue;
            }
            _ => {}
        }
        out.push_str(line);
        out.push('\n');
    }
    for (g, max) in &group_max {
        let total: u64 = group_of
            .iter()
            .filter(|(_, pg)| *pg == g)
            .filter_map(|(p, _)| size_of.get(p))
            .sum();
        if total > *max {
            return Err(invalid(format!(
                "partitions in group {} need {} bytes, more than its {} bytes",
                g, total, max
            )));
        }
    }
    Ok(out)
}

/// Repacks `work` into a new ROM zip. Returns its path.
pub fn repack(
    work: &Path,
    output: Option<&Path>,
    opts: RepackOptions,
    mut log: impl FnMut(&str),
) -> io::Result<PathBuf> {
    let parts = read_state(work)?
        .ok_or_else(|| invalid(format!("{} is not unpacked (no {})", work.display(), STATE)))?;
    if opts.brotli_quality > 11 || !(0..=9).contains(&opts.zip_level) {
        return Err(invalid("brotli quality must be 0-11 and zip level 0-9"));
    }
    let rom_dir = work.join("rom");
    let op_list_path = rom_dir.join(OP_LIST);
    let op_list = fs::read_to_string(&op_list_path).ok();

    let out_path = match output {
        Some(p) => p.to_path_buf(),
        None => work.join("output").join(format!("{}.zip", rom_name())),
    };
    if let Some(dir) = out_path.parent() {
        fs::create_dir_all(dir)?;
    }
    let partial = out_path.with_extension("zip.part");
    let tmp = work.join("tmp");
    fs::create_dir_all(&tmp)?;

    let result = (|| -> io::Result<()> {
        let mut zip = ZipWriter::new(BufWriter::with_capacity(1 << 20, File::create(&partial)?));
        let deflate = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(opts.zip_level))
            .unix_permissions(0o644);
        let stored = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .unix_permissions(0o644);

        // partitions: build, then img2sdat + brotli straight into the zip
        let mut new_sizes = BTreeMap::new();
        for p in &parts {
            let img = tmp.join(format!("{}.img", p.name));
            let parts_dir = partition_dir(work);
            let built = build::build(&parts_dir, &p.name, &img, Size::Original, &mut log);
            let sum = match built {
                Err(e) if e.kind() == io::ErrorKind::StorageFull && op_list.is_some() => {
                    log(&format!(
                        "- {} is full, growing it (dynamic partition)",
                        p.name
                    ));
                    build::build(&parts_dir, &p.name, &img, Size::Auto, &mut log)?
                }
                Err(e) if e.kind() == io::ErrorKind::StorageFull => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!(
                        "{} does not fit its partition and this ROM has no {}; remove some files",
                        p.name, OP_LIST
                    ),
                    ))
                }
                other => other?,
            };
            let fs_bytes = sum.stats.blocks * sum.block_size;
            if fs_bytes > p.size {
                new_sizes.insert(p.name.clone(), fs_bytes);
            }

            let mut image = img2sdat::Image::open(&img).map_err(|e| invalid(e.to_string()))?;
            let plan =
                img2sdat::Plan::new(&mut image, p.version).map_err(|e| invalid(e.to_string()))?;
            zip.start_file(format!("{}.transfer.list", p.name), deflate)
                .map_err(zip_err)?;
            zip.write_all(plan.transfer_list().as_bytes())?;
            let big = plan.new_blocks() * img2sdat::BLOCK_SIZE >= u32::MAX as u64;
            if p.brotli {
                log(&format!("- {}: image -> {}.new.dat.br", p.name, p.name));
                zip.start_file(format!("{}.new.dat.br", p.name), stored.large_file(big))
                    .map_err(zip_err)?;
                let mut enc = br::Encoder::new(&mut zip, opts.brotli_quality, br::DEFAULT_LGWIN)?;
                plan.write_new_data(&mut image, &mut enc)
                    .map_err(|e| invalid(e.to_string()))?;
                enc.finish()?;
            } else {
                log(&format!("- {}: image -> {}.new.dat", p.name, p.name));
                zip.start_file(format!("{}.new.dat", p.name), deflate.large_file(big))
                    .map_err(zip_err)?;
                plan.write_new_data(&mut image, &mut zip)
                    .map_err(|e| invalid(e.to_string()))?;
            }
            zip.start_file(format!("{}.patch.dat", p.name), stored)
                .map_err(zip_err)?;
            drop(image);
            fs::remove_file(&img)?;
        }

        // the rest of the ROM
        let mut files = Vec::new();
        collect_files(&rom_dir, &rom_dir, &mut files)?;
        for (rel, path) in files {
            if rel == OP_LIST {
                if let Some(text) = &op_list {
                    let updated = update_op_list(text, &new_sizes)?;
                    for (p, n) in &new_sizes {
                        log(&format!("- {}: resize {} to {} bytes", OP_LIST, p, n));
                    }
                    zip.start_file(rel, deflate).map_err(zip_err)?;
                    zip.write_all(updated.as_bytes())?;
                    continue;
                }
            }
            let meta = fs::metadata(&path)?;
            let mode = if is_executable(&meta) { 0o755 } else { 0o644 };
            let opts = deflate
                .large_file(meta.len() >= u32::MAX as u64)
                .unix_permissions(mode);
            zip.start_file(rel, opts).map_err(zip_err)?;
            io::copy(&mut BufReader::new(File::open(&path)?), &mut zip)?;
        }
        zip.finish().map_err(zip_err)?.flush()?;
        Ok(())
    })();

    let _ = fs::remove_dir_all(&tmp);
    match result {
        Ok(()) => {
            fs::rename(&partial, &out_path)?;
            Ok(out_path)
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

fn is_executable(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

/// Files under `dir` as (zip path with "/", path), sorted.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        if e.file_type()?.is_dir() {
            collect_files(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel, path));
        }
    }
    Ok(())
}

/// Removes what unpack and repack created in `work`. `input/` is never
/// touched; `output/` only with `all`. Returns the removed paths.
pub fn cleanup(work: &Path, all: bool) -> io::Result<Vec<PathBuf>> {
    let mut targets: Vec<PathBuf> = ["rom", "tmp", "partition"]
        .iter()
        .map(|d| work.join(d))
        .collect();
    if let Some(parts) = read_state(work)? {
        // an unpack by 3.0.0 beta put the partitions and config/ straight
        // into <work>; only then are those folders ours to remove
        if !partition_dir(work).exists() {
            targets.extend(parts.iter().map(|p| work.join(&p.name)));
            targets.push(work.join("config"));
        }
    }
    targets.push(state_path(work));
    if all {
        targets.push(work.join("output"));
    }
    let mut removed = Vec::new();
    for t in targets {
        let result = if t.is_dir() {
            fs::remove_dir_all(&t)
        } else if t.exists() {
            fs::remove_file(&t)
        } else {
            continue;
        };
        result.map_err(|e| io::Error::new(e.kind(), format!("{}: {}", t.display(), e)))?;
        removed.push(t);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPS: &str = "remove_all_groups\n\
        add_group qti 8000\n\
        add system qti\n\
        add vendor qti\n\
        resize system 3000\n\
        resize vendor 1000\n";

    #[test]
    fn op_list_resize() {
        let sizes = BTreeMap::from([("system".to_string(), 4000u64)]);
        let out = update_op_list(OPS, &sizes).unwrap();
        assert!(out.contains("resize system 4000\n"));
        assert!(out.contains("resize vendor 1000\n"));
        assert!(out.starts_with("remove_all_groups\nadd_group qti 8000\n"));
    }

    #[test]
    fn op_list_group_limit() {
        let sizes = BTreeMap::from([("system".to_string(), 7500u64)]);
        assert!(update_op_list(OPS, &sizes).is_err());
    }

    #[test]
    fn config_file() {
        let dir = std::env::temp_dir().join(format!("jancox-init-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // missing file: defaults
        let d = load_config(&dir).unwrap();
        assert_eq!((d.brotli_quality, d.zip_level), (1, 1));

        let made = init(&dir).unwrap();
        assert_eq!(made.len(), 3);
        assert!(init(&dir).unwrap().is_empty(), "init keeps what exists");
        let d = load_config(&dir).unwrap();
        assert_eq!((d.brotli_quality, d.zip_level), (1, 1));

        fs::write(
            dir.join(CONFIG),
            "# x\nbrotli.level = 6\nzip.level=9\nnew.key=1\n",
        )
        .unwrap();
        let c = load_config(&dir).unwrap();
        assert_eq!((c.brotli_quality, c.zip_level), (6, 9));
        fs::write(dir.join(CONFIG), "brotli.level=12\n").unwrap();
        assert!(load_config(&dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rom_name_format() {
        let n = rom_name();
        assert!(n.starts_with("NewROM-20"));
        assert_eq!(n.len(), "NewROM-20260925-171500".len());
    }
}
