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
//! Two kinds of ROM zip are understood:
//!
//! - recovery ROMs with block-based OTA data (`<part>.transfer.list` +
//!   `<part>.new.dat[.br]`). They are streamed zip -> brotli -> sdat2img ->
//!   image and back, so no `.new.dat` is ever written to disk.
//! - fastboot ROMs such as Pixel factory images (see factory.rs): raw
//!   images inside `image-*.zip`, read straight out of the zip. The rest of
//!   the inner zip goes to `rom/<image zip without .zip>/`.
//! - A/B OTA zips with a `payload.bin` (see payload.rs), full OTAs only.
//!   The logical partitions are dumped and extracted, the other images
//!   (boot, vendor_boot, vbmeta, firmware) go to `rom/payload/<name>.img`.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::build::{self, Size};
use crate::fs::invalid;
use crate::sign::{self, Key};
use crate::{br, extract, factory, ota, payload, sdat};

const STATE: &str = "jancox_rom";
const OP_LIST: &str = "dynamic_partitions_op_list";

#[derive(Debug, Clone)]
pub struct RepackOptions {
    /// Brotli quality (0-11) for `.new.dat.br`.
    pub brotli_quality: u32,
    /// Deflate level (0-9) for the other files in the zip.
    pub zip_level: i64,
    /// payload.bin ROMs: which zips repack makes.
    pub payload_output: PayloadOutput,
    /// xz preset (0-9) for rebuilt images in a new payload.bin.
    pub xz_level: u32,
    /// Private key (`.pk8`) and certificate for a new payload.bin; the AOSP
    /// test key when both are `None`.
    pub sign_key: Option<PathBuf>,
    pub sign_cert: Option<PathBuf>,
}

impl Default for RepackOptions {
    fn default() -> Self {
        RepackOptions {
            brotli_quality: br::DEFAULT_QUALITY,
            zip_level: 1,
            payload_output: PayloadOutput::Payload,
            xz_level: 1,
            sign_key: None,
            sign_cert: None,
        }
    }
}

/// What `repack` makes from a payload.bin ROM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadOutput {
    /// A new OTA zip with payload.bin, for recovery / adb sideload.
    Payload,
    /// The images with flash-all.sh / flash-all.bat.
    Fastboot,
    Both,
}

impl PayloadOutput {
    pub fn parse(s: &str) -> Option<PayloadOutput> {
        match s {
            "payload" => Some(PayloadOutput::Payload),
            "fastboot" => Some(PayloadOutput::Fastboot),
            "both" => Some(PayloadOutput::Both),
            _ => None,
        }
    }

    fn payload(self) -> bool {
        self != PayloadOutput::Fastboot
    }

    fn fastboot(self) -> bool {
        self != PayloadOutput::Payload
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
    /// Bytes the transfer list covers (the partition size), or the size of
    /// the image in a fastboot ROM (with its AVB hashtree and footer).
    pub size: u64,
    /// Filesystem of the image: "ext4" or "erofs".
    pub fs: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `<part>.transfer.list` + `<part>.new.dat[.br]`
    Sdat,
    /// raw images in a fastboot `image-*.zip`
    Fastboot,
    /// A/B OTA `payload.bin`
    Payload,
}

/// What unpack found, for repack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub format: Format,
    pub partitions: Vec<Partition>,
    /// Fastboot ROMs: the image zip in the ROM zip (empty when the ROM is
    /// the image zip itself), and its entries in their original order.
    /// Payload ROMs: the entries of the ROM zip in their original order.
    pub image_zip: String,
    pub image_entries: Vec<String>,
}

fn state_path(work: &Path) -> PathBuf {
    work.join(STATE)
}

/// Folder holding the extracted partitions and their `config/`.
pub fn partition_dir(work: &Path) -> PathBuf {
    work.join("partition")
}

fn write_state(work: &Path, input: &Path, state: &State) -> io::Result<()> {
    let mut s = format!("input={}\n", input.display());
    let format = match state.format {
        Format::Sdat => "sdat",
        Format::Fastboot => "fastboot",
        Format::Payload => "payload",
    };
    s.push_str(&format!("format={}\n", format));
    if state.format != Format::Sdat {
        s.push_str(&format!("image_zip={}\n", state.image_zip));
        s.push_str(&format!(
            "image_entries={}\n",
            state.image_entries.join("\t")
        ));
    }
    let parts = &state.partitions;
    let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
    s.push_str(&format!("partitions={}\n", names.join(" ")));
    for p in parts {
        s.push_str(&format!("{}.version={}\n", p.name, p.version));
        s.push_str(&format!("{}.brotli={}\n", p.name, p.brotli));
        s.push_str(&format!("{}.size={}\n", p.name, p.size));
        s.push_str(&format!("{}.fs={}\n", p.name, p.fs));
    }
    fs::write(state_path(work), s)
}

/// Partitions recorded by unpack, or `None` when `work` is not unpacked.
pub fn read_state(work: &Path) -> io::Result<Option<Vec<Partition>>> {
    Ok(read_full_state(work)?.map(|s| s.partitions))
}

/// What unpack recorded, or `None` when `work` is not unpacked.
pub fn read_full_state(work: &Path) -> io::Result<Option<State>> {
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
            // states from 3.0.0 only had ext4
            fs: kv
                .get(format!("{}.fs", name).as_str())
                .map_or("ext4", |v| v)
                .to_string(),
        });
    }
    let format = match kv.get("format").copied() {
        None | Some("sdat") => Format::Sdat,
        Some("fastboot") => Format::Fastboot,
        Some("payload") => Format::Payload,
        Some(f) => return Err(invalid(format!("{}: unknown format {}", STATE, f))),
    };
    Ok(Some(State {
        format,
        partitions: parts,
        image_zip: kv.get("image_zip").unwrap_or(&"").to_string(),
        image_entries: kv
            .get("image_entries")
            .map(|v| {
                v.split('\t')
                    .filter(|e| !e.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    }))
}

const CONFIG: &str = "jancox.prop";

const DEFAULT_CONFIG: &str = "\
# Jancox tool settings, read by `jancox repack`.
# Command line options (-b, -z) win over these.

# brotli quality for <partition>.new.dat.br: 0 (fastest) - 11 (smallest)
brotli.level=1

# deflate level for the other files in the ROM zip: 0 (store) - 9 (smallest)
zip.level=1

# payload.bin (A/B OTA) ROMs: what repack makes
#   payload  = a new OTA zip with payload.bin (custom recovery or adb sideload)
#   fastboot = the images with flash-all.sh / flash-all.bat (fastboot)
#   both     = both zips
payload.output=payload

# xz level for rebuilt partitions in payload.bin: 0 (fastest) - 9 (smallest)
payload.xz_level=1

# key for the new payload.bin and the zip signature (paths from this folder).
# Unset = AOSP test key, trusted by TWRP, OrangeFox and other test-keys
# recoveries. Stock recoveries only take the vendor's own key.
#sign.key=keys/releasekey.pk8
#sign.cert=keys/releasekey.x509.pem
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
            "payload.output" => {
                opts.payload_output = PayloadOutput::parse(value).ok_or_else(bad)?
            }
            "payload.xz_level" => {
                opts.xz_level = value.parse().ok().filter(|l| *l <= 9).ok_or_else(bad)?
            }
            "sign.key" if !value.is_empty() => opts.sign_key = Some(work.join(value)),
            "sign.cert" if !value.is_empty() => opts.sign_cert = Some(work.join(value)),
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
    let has_payload = names.iter().any(|n| n == payload::ENTRY);
    let has_sdat = names.iter().any(|n| n.ends_with(".transfer.list"));
    let image_zip = factory::find_image_zip(names.iter().map(String::as_str)).map(str::to_string);
    let is_image_zip = factory::is_image_zip(names.iter().map(String::as_str));
    if !has_payload && !has_sdat && image_zip.is_none() && !is_image_zip {
        return Err(invalid(
            "no payload.bin, no *.transfer.list + *.new.dat[.br] partitions and no fastboot image-*.zip in this zip",
        ));
    }
    log(&format!("- ROM: {}", input.display()));
    fs::create_dir_all(work)?;
    if !extract::symlinks_supported(work) {
        log("[!] This partition/storage does not support symlinks (e.g. /sdcard).");
        log("    Symlinks are kept in config/<part>_symlinks and put back on repack.");
        log("    To see them as real symlinks, work in a folder like the Termux home (~).");
    }
    let (state, other_files) = if has_payload {
        unpack_payload(input, &mut zip, &names, work, &mut log)?
    } else if has_sdat {
        unpack_sdat(&mut zip, &names, work, &mut log)?
    } else {
        unpack_fastboot(input, &mut zip, image_zip, work, &mut log)?
    };
    write_state(work, input, &state)?;
    Ok(UnpackSummary {
        rom_info: rom_info(work),
        other_files,
        partitions: state.partitions,
    })
}

/// Writes the entries of `zip` for which `keep` is true into `dir`.
/// Returns the number of files written.
fn extract_entries<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    dir: &Path,
    keep: impl Fn(&str) -> bool,
    log: &mut impl FnMut(&str),
) -> io::Result<usize> {
    fs::create_dir_all(dir)?;
    let mut files = 0;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(zip_err)?;
        if !keep(entry.name()) {
            continue;
        }
        let Some(rel) = entry.enclosed_name() else {
            log(&format!("  [warning] skipped unsafe path {}", entry.name()));
            continue;
        };
        let path = dir.join(rel);
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
        files += 1;
    }
    Ok(files)
}

fn log_extracted(sum: &extract::Summary, log: &mut impl FnMut(&str)) {
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
}

fn unpack_sdat<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    names: &[String],
    work: &Path,
    log: &mut impl FnMut(&str),
) -> io::Result<(State, usize)> {
    // partitions: <part>.transfer.list with <part>.new.dat[.br]
    let mut parts = Vec::new();
    let mut part_files = std::collections::HashSet::new();
    for n in names {
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
            fs: String::new(),
        });
    }
    if parts.is_empty() {
        return Err(invalid(
            "no *.transfer.list + *.new.dat[.br] partitions in this zip",
        ));
    }

    // everything else goes to <work>/rom as is
    let rom_dir = work.join("rom");
    let other_files = extract_entries(zip, &rom_dir, |n| !part_files.contains(n), log)?;
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
        let sum = extract::extract(&img, &partition_dir(work), Some(&p.name), &mut *log)?;
        p.fs = sum.fs_type.clone();
        log_extracted(&sum, log);
        fs::remove_file(&img)?;
    }
    let _ = fs::remove_dir(&tmp);
    let state = State {
        format: Format::Sdat,
        partitions: parts,
        image_zip: String::new(),
        image_entries: Vec::new(),
    };
    Ok((state, other_files))
}

/// Images of a fastboot ROM that are left as they are: the other slot's
/// system (preopted code, flashed with --slot-other).
const FASTBOOT_KEEP: &[&str] = &["system_other.img"];

fn unpack_fastboot<R: Read + Seek>(
    input: &Path,
    outer: &mut ZipArchive<R>,
    image_zip: Option<String>,
    work: &Path,
    log: &mut impl FnMut(&str),
) -> io::Result<(State, usize)> {
    let rom_dir = work.join("rom");
    let mut other_files = 0;
    // the image zip is read in place: it must be stored, as in factory zips
    let (base, image_dir) = match &image_zip {
        Some(name) => {
            let index = outer
                .index_for_name(name)
                .ok_or_else(|| invalid(format!("{} not found", name)))?;
            let range = factory::stored_range(outer, index)?.ok_or_else(|| {
                invalid(format!(
                    "{} is compressed inside the ROM zip; extract it and unpack it directly",
                    name
                ))
            })?;
            let others = extract_entries(outer, &rom_dir, |n| n != name, log)?;
            other_files += others;
            log(&format!(
                "- Extracted {} files to {}",
                others,
                rom_dir.display()
            ));
            (
                Some(range),
                rom_dir.join(name.strip_suffix(".zip").unwrap_or(name)),
            )
        }
        None => (None, rom_dir.clone()),
    };
    let (base_start, base_len) = base.unwrap_or((0, fs::metadata(input)?.len()));
    let mut inner =
        ZipArchive::new(factory::open_window(input, base_start, base_len)?).map_err(zip_err)?;
    if image_zip.is_some() {
        log(&format!(
            "- Image zip: {} ({} entries)",
            image_zip.as_deref().unwrap_or(""),
            inner.len()
        ));
    }

    // partitions: ext4 / EROFS images at the root of the image zip; with a
    // super_empty.img only the logical partitions (not firmware like modem)
    let dynamic: Option<std::collections::HashSet<String>> = match inner.by_name("super_empty.img")
    {
        Ok(mut entry) => {
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            let groups = factory::super_groups(&data)?;
            Some(
                groups
                    .iter()
                    .flat_map(|g| &g.partitions)
                    .map(|p| factory::strip_slot(p).to_string())
                    .collect(),
            )
        }
        Err(_) => None,
    };
    let mut parts = Vec::new();
    let mut entries: Vec<String> = inner.file_names().map(str::to_string).collect();
    entries.sort_by_key(|n| inner.index_for_name(n));
    let tmp = work.join("tmp");
    for name in &entries {
        let Some(part) = name.strip_suffix(".img").filter(|p| !p.contains('/')) else {
            continue;
        };
        if FASTBOOT_KEEP.contains(&name.as_str())
            || dynamic.as_ref().is_some_and(|d| !d.contains(part))
        {
            continue;
        }
        let index = inner.index_for_name(name).unwrap();
        let mut head = vec![0u8; 2048];
        let n = {
            let mut entry = inner.by_index(index).map_err(zip_err)?;
            read_up_to(&mut entry, &mut head)?
        };
        let fs_type = match extract::detect(&head[..n]) {
            Some(t @ ("ext4" | "erofs")) => t,
            _ => continue,
        };
        let size = inner.by_index(index).map_err(zip_err)?.size();
        log(&format!(
            "- {}: {} image, {} MiB",
            part,
            fs_type,
            size >> 20
        ));
        let out = partition_dir(work);
        let sum = match factory::stored_range(&mut inner, index)? {
            Some((start, len)) => {
                let image = factory::open_window(input, base_start + start, len)?;
                extract::extract_reader(image, &out, part, &mut *log)?
            }
            None => {
                // compressed in the zip: copy it out first
                fs::create_dir_all(&tmp)?;
                let img = tmp.join(name);
                {
                    let mut entry = inner.by_index(index).map_err(zip_err)?;
                    let mut w = BufWriter::new(File::create(&img)?);
                    io::copy(&mut entry, &mut w)?;
                    w.flush()?;
                }
                let sum = extract::extract(&img, &out, Some(part), &mut *log)?;
                fs::remove_file(&img)?;
                sum
            }
        };
        log_extracted(&sum, log);
        parts.push(Partition {
            name: part.to_string(),
            version: 0,
            brotli: false,
            size,
            fs: fs_type.to_string(),
        });
    }
    let _ = fs::remove_dir(&tmp);
    if parts.is_empty() {
        return Err(invalid(
            "no ext4 or EROFS partition images in the image zip",
        ));
    }
    let images: std::collections::HashSet<String> =
        parts.iter().map(|p| format!("{}.img", p.name)).collect();
    let others = extract_entries(&mut inner, &image_dir, |n| !images.contains(n), log)?;
    log(&format!(
        "- Extracted {} other files to {}",
        others,
        image_dir.display()
    ));
    let state = State {
        format: Format::Fastboot,
        partitions: parts,
        image_zip: image_zip.unwrap_or_default(),
        image_entries: entries,
    };
    Ok((state, other_files + others))
}

/// Folder under `rom/` for the images of a payload.bin that are not
/// extracted.
const PAYLOAD_IMAGES: &str = "payload";

/// Partitions extracted from a payload without dynamic partitions.
const PAYLOAD_FS_PARTS: &[&str] = &[
    "system",
    "system_ext",
    "product",
    "vendor",
    "odm",
    "system_dlkm",
    "vendor_dlkm",
    "odm_dlkm",
];

fn unpack_payload<R: Read + Seek>(
    input: &Path,
    zip: &mut ZipArchive<R>,
    names: &[String],
    work: &Path,
    log: &mut impl FnMut(&str),
) -> io::Result<(State, usize)> {
    let index = zip
        .index_for_name(payload::ENTRY)
        .ok_or_else(|| invalid("payload.bin not found"))?;
    let (start, len) = factory::stored_range(zip, index)?.ok_or_else(|| {
        invalid("payload.bin is compressed inside the ROM zip; extract it and zip it stored")
    })?;
    let mut reader = factory::open_window(input, start, len)?;
    let info = payload::Payload::read(&mut reader)?;
    let m = &info.manifest;
    m.check_full()?;
    if info.data_offset > len {
        return Err(invalid("payload.bin: truncated"));
    }
    log(&format!(
        "- payload.bin: {} partitions, {} MiB",
        m.partitions.len(),
        len >> 20
    ));

    let rom_dir = work.join("rom");
    let other_files = extract_entries(zip, &rom_dir, |n| n != payload::ENTRY, log)?;
    log(&format!(
        "- Extracted {} other files to {}",
        other_files,
        rom_dir.display()
    ));

    // logical partitions with ext4 / EROFS are extracted, the rest are kept
    // as images
    let logical: Vec<&str> = if m.groups.is_empty() {
        PAYLOAD_FS_PARTS.to_vec()
    } else {
        m.groups
            .iter()
            .flat_map(|g| &g.partitions)
            .map(|p| factory::strip_slot(p))
            .collect()
    };
    let images = rom_dir.join(PAYLOAD_IMAGES);
    let tmp = work.join("tmp");
    fs::create_dir_all(&images)?;
    fs::create_dir_all(&tmp)?;
    let threads = payload::default_threads();
    let mut parts = Vec::new();
    let mut kept = Vec::new();
    for p in &m.partitions {
        let file_name = format!("{}.img", p.name);
        let extract = logical.contains(&p.name.as_str());
        let path = if extract {
            tmp.join(&file_name)
        } else {
            images.join(&file_name)
        };
        if extract {
            log(&format!(
                "- {}: payload -> image, {} MiB",
                p.name,
                p.size >> 20
            ));
        }
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        payload::dump_partition(&mut reader, &info, p, &mut file, threads)?;
        if !extract {
            kept.push(p.name.as_str());
            continue;
        }
        let mut head = vec![0u8; 2048];
        file.seek(io::SeekFrom::Start(0))?;
        let n = read_up_to(&mut file, &mut head)?;
        let fs_type = match extract::detect(&head[..n]) {
            Some(t @ ("ext4" | "erofs")) => t,
            _ => {
                drop(file);
                fs::rename(&path, images.join(&file_name))?;
                kept.push(p.name.as_str());
                continue;
            }
        };
        log(&format!("- {}: {} image", p.name, fs_type));
        let sum = extract::extract_reader(
            BufReader::with_capacity(1 << 16, file),
            &partition_dir(work),
            &p.name,
            &mut *log,
        )?;
        log_extracted(&sum, log);
        fs::remove_file(&path)?;
        parts.push(Partition {
            name: p.name.clone(),
            version: 0,
            brotli: false,
            size: p.size,
            fs: fs_type.to_string(),
        });
    }
    let _ = fs::remove_dir(&tmp);
    log(&format!(
        "- {} images to {}: {}",
        kept.len(),
        images.display(),
        kept.join(" ")
    ));
    if parts.is_empty() {
        return Err(invalid("no ext4 or EROFS partitions in payload.bin"));
    }
    let state = State {
        format: Format::Payload,
        partitions: parts,
        image_zip: String::new(),
        image_entries: names.to_vec(),
    };
    Ok((state, other_files + kept.len()))
}

fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
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

/// Repacks `work` into a new ROM zip. Returns the zips it wrote (two for
/// a payload.bin ROM with `payload.output=both`).
pub fn repack(
    work: &Path,
    output: Option<&Path>,
    opts: RepackOptions,
    mut log: impl FnMut(&str),
) -> io::Result<Vec<PathBuf>> {
    let state = read_full_state(work)?
        .ok_or_else(|| invalid(format!("{} is not unpacked (no {})", work.display(), STATE)))?;
    if opts.brotli_quality > 11 || !(0..=9).contains(&opts.zip_level) || opts.xz_level > 9 {
        return Err(invalid(
            "brotli quality must be 0-11, zip level 0-9 and xz level 0-9",
        ));
    }
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

    if state.format == Format::Payload {
        let result = repack_payload(work, &state, &out_path, &tmp, &opts, &mut log);
        let _ = fs::remove_dir_all(&tmp);
        return result;
    }
    let result = match state.format {
        Format::Sdat => repack_sdat(work, &state.partitions, &partial, &tmp, &opts, &mut log),
        Format::Fastboot => repack_fastboot(work, &state, &partial, &tmp, &opts, &mut log),
        Format::Payload => unreachable!(),
    };
    let _ = fs::remove_dir_all(&tmp);
    match result {
        Ok(()) => {
            fs::rename(&partial, &out_path)?;
            Ok(vec![out_path])
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

fn zip_options(opts: &RepackOptions) -> (SimpleFileOptions, SimpleFileOptions) {
    let deflate = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(opts.zip_level))
        .unix_permissions(0o644);
    let stored = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o644);
    (deflate, stored)
}

/// Copies a file into the zip, keeping its executable bit.
fn add_file<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    name: &str,
    path: &Path,
    options: SimpleFileOptions,
) -> io::Result<()> {
    let meta = fs::metadata(path)?;
    let mode = if is_executable(&meta) { 0o755 } else { 0o644 };
    let options = options
        .large_file(meta.len() >= u32::MAX as u64)
        .unix_permissions(mode);
    zip.start_file(name, options).map_err(zip_err)?;
    io::copy(&mut BufReader::new(File::open(path)?), zip)?;
    Ok(())
}

fn repack_sdat(
    work: &Path,
    parts: &[Partition],
    partial: &Path,
    tmp: &Path,
    opts: &RepackOptions,
    log: &mut impl FnMut(&str),
) -> io::Result<()> {
    let rom_dir = work.join("rom");
    let op_list_path = rom_dir.join(OP_LIST);
    let op_list = fs::read_to_string(&op_list_path).ok();
    {
        let mut zip = ZipWriter::new(BufWriter::with_capacity(1 << 20, File::create(partial)?));
        let (deflate, stored) = zip_options(opts);

        // partitions: build, then img2sdat + brotli straight into the zip
        let mut new_sizes = BTreeMap::new();
        for p in parts {
            let img = tmp.join(format!("{}.img", p.name));
            let parts_dir = partition_dir(work);
            let built = build::build(&parts_dir, &p.name, &img, Size::Original, &mut *log);
            let sum = match built {
                Err(e) if e.kind() == io::ErrorKind::StorageFull && op_list.is_some() => {
                    log(&format!(
                        "- {} is full, growing it (dynamic partition)",
                        p.name
                    ));
                    build::build(&parts_dir, &p.name, &img, Size::Auto, &mut *log)?
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
                if op_list.is_none() {
                    // EROFS images are made to measure and never fail to build
                    return Err(io::Error::new(
                        io::ErrorKind::StorageFull,
                        format!(
                            "{} needs {} bytes, more than its {} byte partition, and this ROM has no {}; remove some files",
                            p.name, fs_bytes, p.size, OP_LIST
                        ),
                    ));
                }
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
            add_file(&mut zip, &rel, &path, deflate)?;
        }
        zip.finish().map_err(zip_err)?.flush()?;
    }
    Ok(())
}

/// Folder holding the unpacked image zip of a fastboot ROM.
fn image_dir(work: &Path, state: &State) -> PathBuf {
    let rom_dir = work.join("rom");
    match state.image_zip.as_str() {
        "" => rom_dir,
        z => rom_dir.join(z.strip_suffix(".zip").unwrap_or(z)),
    }
}

fn repack_fastboot(
    work: &Path,
    state: &State,
    partial: &Path,
    tmp: &Path,
    opts: &RepackOptions,
    log: &mut impl FnMut(&str),
) -> io::Result<()> {
    let rom_dir = work.join("rom");
    let image_dir = image_dir(work, state);
    let super_empty = image_dir.join("super_empty.img");
    let groups = if super_empty.is_file() {
        Some(factory::super_groups(&fs::read(&super_empty)?)?)
    } else {
        None
    };

    // partitions: build the images
    let parts_dir = partition_dir(work);
    let mut sizes = BTreeMap::new();
    let mut built = BTreeMap::new();
    for p in &state.partitions {
        let img = tmp.join(format!("{}.img", p.name));
        let sum = match build::build(&parts_dir, &p.name, &img, Size::Original, &mut *log) {
            // logical partitions take the size of their image
            Err(e) if e.kind() == io::ErrorKind::StorageFull && groups.is_some() => {
                log(&format!(
                    "- {} is full, growing it (dynamic partition)",
                    p.name
                ));
                build::build(&parts_dir, &p.name, &img, Size::Auto, &mut *log)?
            }
            other => other?,
        };
        let bytes = sum.stats.blocks * sum.block_size;
        if groups.is_none() && bytes > p.size {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!(
                    "{} needs {} bytes, more than its {} byte image; remove some files",
                    p.name, bytes, p.size
                ),
            ));
        }
        sizes.insert(p.name.clone(), bytes);
        built.insert(format!("{}.img", p.name), img);
    }
    if let Some(groups) = &groups {
        // images that were not rebuilt count with their own size
        for g in groups {
            for name in g.partitions.iter().map(|p| factory::strip_slot(p)) {
                let img = image_dir.join(format!("{}.img", name));
                if !sizes.contains_key(name) && img.is_file() {
                    sizes.insert(name.to_string(), fs::metadata(&img)?.len());
                }
            }
        }
        factory::check_groups(groups, &sizes)?;
    }

    // the image zip: entries in their original order, then new files
    let (deflate, stored) = zip_options(opts);
    let inner_path = if state.image_zip.is_empty() {
        partial.to_path_buf()
    } else {
        tmp.join("image.zip")
    };
    let mut files = Vec::new();
    collect_files(&image_dir, &image_dir, &mut files)?;
    let on_disk: BTreeMap<String, PathBuf> = files.into_iter().collect();
    let mut order: Vec<String> = state
        .image_entries
        .iter()
        .filter(|n| built.contains_key(*n) || on_disk.contains_key(*n))
        .cloned()
        .collect();
    order.extend(
        on_disk
            .keys()
            .filter(|n| !state.image_entries.contains(n))
            .cloned(),
    );
    {
        let mut zip = ZipWriter::new(BufWriter::with_capacity(
            1 << 20,
            File::create(&inner_path)?,
        ));
        for name in &order {
            if let Some(img) = built.get(name) {
                log(&format!("- {}: adding to the image zip", name));
                add_file(&mut zip, name, img, stored)?;
                fs::remove_file(img)?;
                continue;
            }
            let path = &on_disk[name];
            if name == "vbmeta.img" {
                let mut data = fs::read(path)?;
                if factory::disable_verification(&mut data)? {
                    log("- vbmeta.img: dm-verity and AVB verification disabled");
                    log("  (the rebuilt partitions have no hashtree; the device must be unlocked)");
                }
                zip.start_file(name.as_str(), deflate).map_err(zip_err)?;
                zip.write_all(&data)?;
                continue;
            }
            // raw images (system_other, userdata, ...) stay uncompressed
            let big = fs::metadata(path)?.len() >= 128 << 20;
            add_file(&mut zip, name, path, if big { stored } else { deflate })?;
        }
        zip.finish().map_err(zip_err)?.flush()?;
    }
    if state.image_zip.is_empty() {
        return Ok(());
    }

    // the ROM zip: the rest of rom/, and the image zip stored
    let image_rel = image_dir
        .strip_prefix(&rom_dir)
        .unwrap_or(&image_dir)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    let mut files = Vec::new();
    collect_files(&rom_dir, &rom_dir, &mut files)?;
    let mut zip = ZipWriter::new(BufWriter::with_capacity(1 << 20, File::create(partial)?));
    for (rel, path) in files {
        if rel.starts_with(&format!("{}/", image_rel)) {
            continue;
        }
        add_file(&mut zip, &rel, &path, deflate)?;
    }
    log(&format!("- Adding {}", state.image_zip));
    add_file(&mut zip, &state.image_zip, &inner_path, stored)?;
    fs::remove_file(&inner_path)?;
    zip.finish().map_err(zip_err)?.flush()?;
    Ok(())
}

/// The ROM zip that was unpacked: the recorded path, else the zip in
/// `input/`. Repack of a payload.bin ROM copies unchanged partitions from it.
fn recorded_input(work: &Path) -> io::Result<PathBuf> {
    let text = fs::read_to_string(state_path(work))?;
    if let Some(p) = text.lines().find_map(|l| l.strip_prefix("input=")) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    find_input(work).ok_or_else(|| {
        invalid(format!(
            "the unpacked ROM zip is gone; put it back in {} (repack copies the unchanged partitions from it)",
            work.join("input").display()
        ))
    })
}

fn load_key(opts: &RepackOptions) -> io::Result<Key> {
    match (&opts.sign_key, &opts.sign_cert) {
        (None, None) => Key::test_key(),
        (Some(k), Some(c)) => Key::load(k, c),
        _ => Err(invalid(format!(
            "set both sign.key and sign.cert in {}",
            CONFIG
        ))),
    }
}

/// How a partition of a payload is repacked.
enum Source {
    /// Unchanged: its operations and blobs are copied from the old payload.
    Keep(PathBuf),
    /// A new or changed image.
    Image(PathBuf),
}

impl Source {
    fn path(&self) -> &Path {
        match self {
            Source::Keep(p) | Source::Image(p) => p,
        }
    }
}

fn repack_payload(
    work: &Path,
    state: &State,
    out_path: &Path,
    tmp: &Path,
    opts: &RepackOptions,
    log: &mut impl FnMut(&str),
) -> io::Result<Vec<PathBuf>> {
    let input = recorded_input(work)?;
    let (mut reader, info) = payload::open(&input)?;
    let m = &info.manifest;
    m.check_full()?;
    for p in &state.partitions {
        if m.partition(&p.name).is_none() {
            return Err(invalid(format!(
                "{} has no partition {}; was {} unpacked from another ROM?",
                input.display(),
                p.name,
                work.display()
            )));
        }
    }
    // fail before the slow part when the key is wrong
    let key = match opts.payload_output.payload() {
        true => Some(load_key(opts)?),
        false => None,
    };
    let bs = m.block_size as u64;
    let logical: std::collections::HashSet<&str> = m
        .groups
        .iter()
        .flat_map(|g| &g.partitions)
        .map(|p| factory::strip_slot(p))
        .collect();
    let images = work.join("rom").join(PAYLOAD_IMAGES);
    let parts_dir = partition_dir(work);

    // the images: rebuilt partitions, changed images, unchanged images
    let mut sources = Vec::new();
    let mut sizes = BTreeMap::new();
    for p in &m.partitions {
        let is_logical = logical.contains(p.name.as_str());
        let source = if state.partitions.iter().any(|x| x.name == p.name) {
            let img = tmp.join(format!("{}.img", p.name));
            match build::build(&parts_dir, &p.name, &img, Size::Original, &mut *log) {
                // logical partitions take the size of their image
                Err(e) if e.kind() == io::ErrorKind::StorageFull && is_logical => {
                    log(&format!(
                        "- {} is full, growing it (dynamic partition)",
                        p.name
                    ));
                    build::build(&parts_dir, &p.name, &img, Size::Auto, &mut *log)?;
                }
                other => {
                    other?;
                }
            }
            Source::Image(img)
        } else {
            let path = images.join(format!("{}.img", p.name));
            if !path.is_file() {
                return Err(invalid(format!(
                    "{} is missing; the new ROM needs every partition of the payload",
                    path.display()
                )));
            }
            if p.name == "vbmeta" {
                let mut data = fs::read(&path)?;
                if factory::disable_verification(&mut data)? {
                    log("- vbmeta: dm-verity and AVB verification disabled");
                    log("  (the rebuilt partitions have no hashtree; the device must be unlocked)");
                    let patched = tmp.join("vbmeta.img");
                    fs::write(&patched, &data)?;
                    sources.push(Source::Image(patched));
                    sizes.insert(p.name.clone(), data.len() as u64);
                    continue;
                }
            }
            let len = fs::metadata(&path)?.len();
            if len == p.size && sign::sha256_file(&path)?[..] == p.hash[..] {
                Source::Keep(path)
            } else {
                log(&format!("- {}: changed, it will be re-encoded", p.name));
                Source::Image(path)
            }
        };
        let size = fs::metadata(source.path())?.len().div_ceil(bs) * bs;
        if !is_logical && size > p.size {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!(
                    "{} is {} bytes, more than its {} byte partition",
                    p.name, size, p.size
                ),
            ));
        }
        sizes.insert(p.name.clone(), size);
        sources.push(source);
    }
    factory::check_groups(&m.groups, &sizes)?;

    let fastboot_path = match opts.payload_output {
        PayloadOutput::Both => {
            let stem = out_path
                .file_stem()
                .map_or("NewROM".into(), |s| s.to_string_lossy().into_owned());
            out_path.with_file_name(format!("{}-fastboot.zip", stem))
        }
        _ => out_path.to_path_buf(),
    };
    let mut written = Vec::new();
    if let Some(key) = &key {
        let partial = out_path.with_extension("zip.part");
        let result = write_ota(
            work,
            state,
            &info,
            &mut reader,
            &sources,
            &partial,
            tmp,
            opts,
            key,
            log,
        );
        match result {
            Ok(()) => fs::rename(&partial, out_path)?,
            Err(e) => {
                let _ = fs::remove_file(&partial);
                return Err(e);
            }
        }
        written.push(out_path.to_path_buf());
    }
    if opts.payload_output.fastboot() {
        let partial = fastboot_path.with_extension("zip.part");
        match write_payload_fastboot(m, &sources, &partial, log) {
            Ok(()) => fs::rename(&partial, &fastboot_path)?,
            Err(e) => {
                let _ = fs::remove_file(&partial);
                return Err(e);
            }
        }
        written.push(fastboot_path);
    }
    Ok(written)
}

/// Writes the new OTA zip: payload.bin, payload_properties.txt, metadata
/// with new property files, the other entries of the old zip in their
/// original order, then the whole-file signature.
#[allow(clippy::too_many_arguments)]
fn write_ota<R: Read + Seek + Send>(
    work: &Path,
    state: &State,
    info: &payload::Payload,
    reader: &mut R,
    sources: &[Source],
    partial: &Path,
    tmp: &Path,
    opts: &RepackOptions,
    key: &Key,
    log: &mut impl FnMut(&str),
) -> io::Result<()> {
    let m = &info.manifest;
    let bs = m.block_size as u64;
    let threads = payload::default_threads();
    let data_path = tmp.join("payload.data");
    let mut parts = Vec::new();
    let mut data_len = 0u64;
    {
        let mut data = BufWriter::with_capacity(1 << 20, File::create(&data_path)?);
        for (p, source) in m.partitions.iter().zip(sources) {
            match source {
                Source::Keep(_) => {
                    parts.push(payload::copy_partition(
                        reader,
                        info,
                        p,
                        &mut data,
                        &mut data_len,
                    )?);
                }
                Source::Image(path) => {
                    log(&format!(
                        "- {}: image -> payload.bin (xz level {})",
                        p.name, opts.xz_level
                    ));
                    let mut image = BufReader::with_capacity(1 << 20, File::open(path)?);
                    parts.push(payload::encode_image(
                        &p.name,
                        &mut image,
                        bs,
                        opts.xz_level,
                        threads,
                        &mut data,
                        &mut data_len,
                    )?);
                }
            }
        }
        data.flush()?;
    }
    let kept = parts.iter().filter(|p| !p.rebuilt).count();
    log(&format!(
        "- payload.bin: {} partitions ({} copied unchanged), {} MiB of data",
        parts.len(),
        kept,
        data_len >> 20
    ));
    let sig_size = payload::signatures_size(key);
    let manifest = payload::encode_manifest(m, &parts, data_len, sig_size)?;
    let payload_size = payload::payload_size(&manifest, data_len, key);
    let metadata_with_sig = 24 + manifest.len() as u64 + sig_size;

    // entries: the old order, payload_properties.txt after payload.bin,
    // then files added to rom/
    let rom_dir = work.join("rom");
    let mut names: Vec<String> = Vec::new();
    for n in &state.image_entries {
        match n.as_str() {
            "care_map.pb" => {
                // hashtree ranges of the old images: wrong for rebuilt ones
                log("- care_map.pb left out (it describes the old images)");
            }
            ota::PROPERTIES => {}
            payload::ENTRY => {
                names.push(n.clone());
                names.push(ota::PROPERTIES.to_string());
            }
            ota::OTACERT => names.push(n.clone()),
            _ if rom_dir.join(n).is_file() => names.push(n.clone()),
            _ => {}
        }
    }
    let mut files = Vec::new();
    collect_files(&rom_dir, &rom_dir, &mut files)?;
    for (rel, _) in &files {
        let generated = [payload::ENTRY, ota::PROPERTIES, "care_map.pb"];
        if !rel.starts_with(&format!("{}/", PAYLOAD_IMAGES))
            && !names.contains(rel)
            && !generated.contains(&rel.as_str())
        {
            names.push(rel.clone());
        }
    }
    let dummy = payload::Properties {
        file_hash: [0; 32],
        file_size: payload_size,
        metadata_hash: [0; 32],
        metadata_size: 24 + manifest.len() as u64,
        metadata_with_signature: metadata_with_sig,
    };
    let old_meta = fs::read_to_string(rom_dir.join(ota::METADATA)).unwrap_or_default();
    let old_pb = fs::read(rom_dir.join(ota::METADATA_PB)).unwrap_or_default();
    let cert = key.cert_pem();
    let stored = zip_options(opts).1;
    let options = |size: u64, exec: bool| {
        stored
            .large_file(size >= u32::MAX as u64)
            .unix_permissions(if exec { 0o755 } else { 0o644 })
    };
    // every entry is stored, so each offset is known before writing; the
    // metadata lists its own offset, so its size is settled by iterating
    let (mut meta, mut pb) = (old_meta.clone(), old_pb.clone());
    let mut plan = Vec::new();
    let mut offsets = Vec::new();
    for round in 0.. {
        plan.clear();
        for n in &names {
            let (size, exec) = match n.as_str() {
                payload::ENTRY => (payload_size, false),
                ota::PROPERTIES => (dummy.text().len() as u64, false),
                ota::METADATA => (meta.len() as u64, false),
                ota::METADATA_PB => (pb.len() as u64, false),
                ota::OTACERT => (cert.len() as u64, false),
                _ => {
                    let md = fs::metadata(rom_dir.join(n))?;
                    (md.len(), is_executable(&md))
                }
            };
            plan.push((n.clone(), size, options(size, exec)));
        }
        offsets = ota::data_offsets(&plan)?;
        let locate = |name: &str| -> Option<(u64, u64)> {
            if name == ota::PAYLOAD_METADATA {
                let i = names.iter().position(|n| n == payload::ENTRY)?;
                return Some((offsets[i], metadata_with_sig));
            }
            let i = names
                .iter()
                .position(|n| n == name || n.ends_with(&format!("/{}", name)))?;
            Some((offsets[i], plan[i].1))
        };
        let new_meta = ota::update_metadata(&old_meta, &locate);
        let new_pb = if old_pb.is_empty() {
            Vec::new()
        } else {
            ota::update_metadata_pb(&old_pb, &locate)?
        };
        let settled = new_meta.len() == meta.len() && new_pb.len() == pb.len();
        meta = new_meta;
        pb = new_pb;
        if settled && round > 0 {
            break;
        }
        if round > 8 {
            return Err(io::Error::other("zip metadata offsets do not settle"));
        }
    }

    {
        let mut zip = ZipWriter::new(BufWriter::with_capacity(1 << 20, File::create(partial)?));
        let mut props = None;
        for (name, size, options) in &plan {
            match name.as_str() {
                payload::ENTRY => {
                    log("- Writing payload.bin (signed)");
                    zip.start_file(name.as_str(), *options).map_err(zip_err)?;
                    let mut data = BufReader::with_capacity(1 << 20, File::open(&data_path)?);
                    props = Some(payload::write_payload(
                        &mut zip, &manifest, &mut data, data_len, key,
                    )?);
                }
                ota::PROPERTIES => {
                    let text = props
                        .as_ref()
                        .ok_or_else(|| io::Error::other("payload.bin not written yet"))?
                        .text();
                    ota::write_stored(&mut zip, name, *options, text.as_bytes())?;
                }
                ota::METADATA => ota::write_stored(&mut zip, name, *options, meta.as_bytes())?,
                ota::METADATA_PB => ota::write_stored(&mut zip, name, *options, &pb)?,
                ota::OTACERT => ota::write_stored(&mut zip, name, *options, cert.as_bytes())?,
                _ => {
                    zip.start_file(name.as_str(), *options).map_err(zip_err)?;
                    let n = io::copy(&mut File::open(rom_dir.join(name))?, &mut zip)?;
                    if n != *size {
                        return Err(io::Error::other(format!("{} changed while writing", name)));
                    }
                }
            }
        }
        zip.finish().map_err(zip_err)?.flush()?;
    }
    fs::remove_file(&data_path)?;

    // the property files must point at the entries
    let mut check = ZipArchive::new(BufReader::new(File::open(partial)?)).map_err(zip_err)?;
    for (i, (name, _, _)) in plan.iter().enumerate() {
        let entry = check.by_name(name).map_err(zip_err)?;
        if entry.data_start() != Some(offsets[i]) {
            return Err(io::Error::other(format!(
                "{}: planned at {}, written at {:?}",
                name,
                offsets[i],
                entry.data_start()
            )));
        }
    }
    drop(check);
    log("- Signing the zip");
    sign::sign_zip(partial, key)
}

/// Writes a fastboot ROM from the images of a payload: the images and
/// flash-all.sh / flash-all.bat.
fn write_payload_fastboot(
    m: &payload::Manifest,
    sources: &[Source],
    partial: &Path,
    log: &mut impl FnMut(&str),
) -> io::Result<()> {
    let logical: std::collections::HashSet<&str> = m
        .groups
        .iter()
        .flat_map(|g| &g.partitions)
        .map(|p| factory::strip_slot(p))
        .collect();
    let (logical_parts, firmware): (Vec<String>, Vec<String>) = m
        .partitions
        .iter()
        .map(|p| p.name.clone())
        .partition(|n| logical.contains(n.as_str()));
    let (sh, bat) = ota::flash_scripts(&firmware, &logical_parts, m.snapshot_enabled);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut zip = ZipWriter::new(BufWriter::with_capacity(1 << 20, File::create(partial)?));
    zip.start_file("flash-all.sh", stored.unix_permissions(0o755))
        .map_err(zip_err)?;
    zip.write_all(sh.as_bytes())?;
    zip.start_file("flash-all.bat", stored.unix_permissions(0o644))
        .map_err(zip_err)?;
    zip.write_all(bat.as_bytes())?;
    log("- Writing the fastboot ROM (images + flash-all.sh/.bat)");
    for (p, source) in m.partitions.iter().zip(sources) {
        add_file(&mut zip, &format!("{}.img", p.name), source.path(), stored)?;
    }
    zip.finish().map_err(zip_err)?.flush()?;
    Ok(())
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
