use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::time::Instant;

use jancox_core::{br, build, dat, extract, img2sdat, lp, payload, rom, sdat};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Like `println!`, but ignores write errors (e.g. a closed pipe) instead of panicking.
macro_rules! out {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stdout(), $($arg)*);
    }};
}

fn usage() {
    out!("Jancox tool {} by wahyu6070\n", VERSION);
    out!("Usage: jancox <command> [args]\n");
    out!(
        "ROM commands (work folder: -w, default {}):",
        default_workdir().display()
    );
    out!("  init [-w workdir]");
    out!("      Make input/, output/ and jancox.prop (brotli/zip levels) if missing");
    out!("  unpack [rom.zip] [-w workdir]");
    out!("      Unpack a ROM zip (default: <workdir>/input/*.zip or input.zip) into editable folders");
    out!("  repack [-w workdir] [-o out.zip] [-b brotli_quality] [-z zip_level] [-t type]");
    out!("      Build a new ROM zip in <workdir>/output/ (default: jancox.prop, else -b 1 -z 1)");
    out!(
        "      -t: output formats (auto = like the input; fastboot, sdat, payload, super; a list)"
    );
    out!("  cleanup [-w workdir] [--all]");
    out!("      Remove the unpacked files (--all: also <workdir>/output); input/ is kept\n");
    out!("Tools:");
    out!("  sdat2img <transfer_list> <new_dat> [output_img]");
    out!("      Convert *.new.dat or *.new.dat.br into a raw image (default: system.img)");
    out!("  img2sdat <image> [-o outdir] [-v version] [-p prefix] [-b quality]");
    out!("      Convert a sparse or raw image into <prefix>.new.dat + .transfer.list");
    out!("      (default: -o . -v 4 -p system; version 1-4)");
    out!("      -b writes <prefix>.new.dat.br directly (brotli quality 0-11)");
    out!("  brotli [-d] [-q quality | -0..-11] [-w window] [-o output] [-j] [-f] <file>");
    out!("      Compress to <file>.br, or with -d decompress <file>.br");
    out!(
        "      (default: -q {} -w {}; -j removes <file> after success, -f overwrites)",
        br::DEFAULT_QUALITY,
        br::DEFAULT_LGWIN
    );
    out!("  extract <image> [-o outdir] [-p name]");
    out!("      Extract an ext4 or EROFS image to <outdir>/<name>/ plus metadata in <outdir>/config/");
    out!("      (default: -o . -p <image name>)");
    out!("  payload <payload.bin|ota.zip> [-o outdir] [-p name,...] [-l] [-t threads]");
    out!("      Dump the partition images of a full A/B OTA payload to <outdir>/<name>.img");
    out!("      (default: -o . and all partitions; -l only lists them)");
    out!("  super <super.img> [-o outdir] [-p name,...] [-l] [-s slot]");
    out!(
        "      Dump the logical partitions of a super image (raw or sparse) to <outdir>/<name>.img"
    );
    out!("      (default: -o . -s 0, all partitions with data; -l only lists them)");
    out!("  build <workdir> <part> [-o image] [-s size|auto] [-f]");
    out!("      Build an ext4 or EROFS image (fs_type in <part>_info) from <workdir>/<part>/");
    out!("      and <workdir>/config/<part>_*");
    out!("      (default: -o <workdir>/<part>.img, -s = original size; size in bytes or K/M/G)");
    out!("  help     Show this help");
    out!("  version  Show version");
}

fn sdat2img(args: &[String]) -> Result<(), String> {
    if args.len() < 2 || args.len() > 3 {
        return Err("usage: jancox sdat2img <transfer_list> <new_dat> [output_img]".into());
    }
    let output = args.get(2).map(String::as_str).unwrap_or("system.img");

    let list = sdat::dat_to_img(&args[0], &args[1], output, |msg| out!("{}", msg))
        .map_err(|e| format!("sdat2img failed: {}", e))?;
    out!("{}", sdat::android_version_name(list.version));
    out!("Done! Output image: {}", output);
    Ok(())
}

fn img2sdat(args: &[String]) -> Result<(), String> {
    const USAGE: &str =
        "usage: jancox img2sdat <image> [-o outdir] [-v version] [-p prefix] [-b quality]";
    let mut image = None;
    let (mut out_dir, mut version, mut prefix) = (".".to_string(), 4u32, "system".to_string());
    let mut brotli = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{} needs a value\n{}", arg, USAGE))
        };
        match arg.as_str() {
            "-o" | "--outdir" => out_dir = value()?,
            "-v" | "--version" => {
                version = value()?
                    .parse()
                    .map_err(|_| format!("invalid version for {}\n{}", arg, USAGE))?
            }
            "-p" | "--prefix" => prefix = value()?,
            "-b" | "--brotli" => {
                brotli = Some(
                    value()?
                        .parse()
                        .map_err(|_| format!("invalid quality for {}\n{}", arg, USAGE))?,
                )
            }
            _ if image.is_none() && !arg.starts_with('-') => image = Some(arg.clone()),
            _ => return Err(format!("unexpected argument: {}\n{}", arg, USAGE)),
        }
    }
    let image = image.ok_or(USAGE)?;

    out!(
        "img2sdat - transfer list version {} ({})",
        version,
        img2sdat::android_version_name(version)
    );
    let plan = dat::img_to_dat(&image, &out_dir, &prefix, version, brotli, |msg| {
        out!("{}", msg)
    })
    .map_err(|e| format!("img2sdat failed: {}", e))?;
    out!(
        "{} blocks written, {} of them stored in {}.new.dat{}",
        plan.written_blocks(),
        plan.new_blocks(),
        prefix,
        if brotli.is_some() { ".br" } else { "" }
    );
    Ok(())
}

fn brotli(args: &[String]) -> Result<(), String> {
    const USAGE: &str =
        "usage: jancox brotli [-d] [-q quality | -0..-11] [-w window] [-o output] [-j] [-f] <file>";
    let (mut decompress, mut remove_input, mut force) = (false, false, false);
    let (mut quality, mut lgwin) = (br::DEFAULT_QUALITY, br::DEFAULT_LGWIN);
    let (mut input, mut output) = (None, None);

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut number = || -> Result<u32, String> {
            let v = it
                .next()
                .ok_or_else(|| format!("{} needs a value\n{}", arg, USAGE))?;
            v.parse()
                .map_err(|_| format!("invalid number for {}: {}\n{}", arg, v, USAGE))
        };
        match arg.as_str() {
            "-d" | "--decompress" => decompress = true,
            "-j" | "--rm" => remove_input = true,
            "-f" | "--force" => force = true,
            "-q" | "--quality" => quality = number()?,
            "-w" | "--lgwin" => lgwin = number()?,
            "-o" | "--output" => {
                output =
                    Some(PathBuf::from(it.next().ok_or_else(|| {
                        format!("{} needs a value\n{}", arg, USAGE)
                    })?))
            }
            // -0 .. -11, like the brotli command
            _ if arg.len() > 1 && arg[1..].parse::<u32>().is_ok() && arg.starts_with('-') => {
                quality = arg[1..].parse().unwrap_or(quality)
            }
            _ if input.is_none() && !arg.starts_with('-') => input = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument: {}\n{}", arg, USAGE)),
        }
    }
    let input = input.ok_or(USAGE)?;
    let output = match output {
        Some(o) => o,
        None if decompress => match input.to_str().and_then(|s| s.strip_suffix(".br")) {
            Some(stem) => PathBuf::from(stem),
            None => {
                return Err(format!(
                    "{}: no .br extension, give the output with -o",
                    input.display()
                ))
            }
        },
        None => {
            let mut name = input.clone().into_os_string();
            name.push(".br");
            PathBuf::from(name)
        }
    };
    if output.exists() && !force {
        return Err(format!(
            "{} already exists, use -f to overwrite",
            output.display()
        ));
    }

    let start = Instant::now();
    let in_size = file_size(&input);
    let out_size = if decompress {
        br::decompress_file(&input, &output)
    } else {
        br::compress_file(&input, &output, quality, lgwin)
    }
    .map_err(|e| format!("brotli failed: {}", e))?;

    out!(
        "{} ({} bytes) -> {} ({} bytes) in {:.2}s",
        input.display(),
        in_size,
        output.display(),
        out_size,
        start.elapsed().as_secs_f64()
    );
    if remove_input {
        fs::remove_file(&input).map_err(|e| format!("{}: {}", input.display(), e))?;
    }
    Ok(())
}

fn extract(args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: jancox extract <image> [-o outdir] [-p name]";
    let (mut image, mut out_dir, mut part) = (None, PathBuf::from("."), None);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{} needs a value\n{}", arg, USAGE))
        };
        match arg.as_str() {
            "-o" | "--outdir" => out_dir = PathBuf::from(value()?),
            "-p" | "--part" => part = Some(value()?),
            _ if image.is_none() && !arg.starts_with('-') => image = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument: {}\n{}", arg, USAGE)),
        }
    }
    let image = image.ok_or(USAGE)?;

    let start = Instant::now();
    let sum = extract::extract(&image, &out_dir, part.as_deref(), |msg| out!("{}", msg))
        .map_err(|e| format!("extract failed: {}", e))?;
    for w in sum.warnings.iter().take(10) {
        out!("  [warning] {}", w);
    }
    if sum.warnings.len() > 10 {
        out!("  [warning] ... {} warnings in total", sum.warnings.len());
    }
    out!(
        "- Done: {} dirs, {} files ({} bytes), {} symlinks, {} special in {:.2}s",
        sum.dirs,
        sum.files,
        sum.bytes,
        sum.symlinks,
        sum.special,
        start.elapsed().as_secs_f64()
    );
    out!(
        "- Metadata: {}/config/{}_{{fs_config,file_contexts,symlinks,info}}",
        out_dir.display(),
        sum.part
    );
    Ok(())
}

fn payload(args: &[String]) -> Result<(), String> {
    const USAGE: &str =
        "usage: jancox payload <payload.bin|ota.zip> [-o outdir] [-p name,...] [-l] [-t threads]";
    let (mut input, mut out_dir, mut names, mut list) = (None, PathBuf::from("."), None, false);
    let mut threads = payload::default_threads();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{} needs a value\n{}", arg, USAGE))
        };
        match arg.as_str() {
            "-o" | "--outdir" => out_dir = PathBuf::from(value()?),
            "-p" | "--part" => {
                names = Some(value()?.split(',').map(str::to_string).collect::<Vec<_>>())
            }
            "-l" | "--list" => list = true,
            "-t" | "--threads" => {
                threads = value()?
                    .parse()
                    .map_err(|_| format!("bad thread count\n{}", USAGE))?
            }
            _ if input.is_none() && !arg.starts_with('-') => input = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument: {}\n{}", arg, USAGE)),
        }
    }
    let input = input.ok_or(USAGE)?;
    let fail = |e: io::Error| format!("payload failed: {}", e);
    let (mut reader, info) = payload::open(&input).map_err(fail)?;
    let m = &info.manifest;
    if list {
        for p in &m.partitions {
            out!(
                "{:<20} {:>12} bytes  {} operations",
                p.name,
                p.size,
                p.ops.len()
            );
        }
        for g in &m.groups {
            out!(
                "- group {} ({} bytes): {}",
                g.name,
                g.max_size,
                g.partitions.join(" ")
            );
        }
        return Ok(());
    }
    m.check_full().map_err(fail)?;
    let parts: Vec<&payload::PartitionUpdate> = match &names {
        None => m.partitions.iter().collect(),
        Some(names) => names
            .iter()
            .map(|n| {
                m.partition(n)
                    .ok_or_else(|| format!("no partition {} in the payload", n))
            })
            .collect::<Result<_, _>>()?,
    };
    fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {}", out_dir.display(), e))?;
    let start = Instant::now();
    for p in parts {
        let path = out_dir.join(format!("{}.img", p.name));
        out!("- {}: {} MiB -> {}", p.name, p.size >> 20, path.display());
        let mut file = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| format!("{}: {}", path.display(), e))?;
        payload::dump_partition(&mut reader, &info, p, &mut file, threads).map_err(fail)?;
    }
    out!("- Done in {:.1}s", start.elapsed().as_secs_f64());
    Ok(())
}

fn super_img(args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: jancox super <super.img> [-o outdir] [-p name,...] [-l] [-s slot]";
    let (mut input, mut out_dir, mut names, mut list, mut slot) =
        (None, PathBuf::from("."), None, false, 0u32);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{} needs a value\n{}", arg, USAGE))
        };
        match arg.as_str() {
            "-o" | "--outdir" => out_dir = PathBuf::from(value()?),
            "-p" | "--part" => {
                names = Some(value()?.split(',').map(str::to_string).collect::<Vec<_>>())
            }
            "-l" | "--list" => list = true,
            "-s" | "--slot" => slot = value()?.parse().map_err(|_| USAGE.to_string())?,
            _ if input.is_none() && !arg.starts_with('-') => input = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument: {}\n{}", arg, USAGE)),
        }
    }
    let input = input.ok_or(USAGE)?;
    let fail = |e: io::Error| format!("super failed: {}", e);
    let file = fs::File::open(&input).map_err(|e| format!("{}: {}", input.display(), e))?;
    let mut img = lp::SuperReader::open(io::BufReader::new(file)).map_err(fail)?;
    let m = lp::read_super(&mut img, slot).map_err(fail)?;
    if list {
        out!(
            "- super: {} bytes, {} metadata slots, {}",
            m.super_size(),
            m.geometry.slot_count,
            if img.is_sparse() { "sparse" } else { "raw" }
        );
        for g in &m.groups {
            out!("- group {} ({} bytes)", g.name, g.max_size);
        }
        for p in &m.partitions {
            out!(
                "{:<24} {:>12} bytes  group {}",
                p.name,
                p.size(),
                m.groups[p.group as usize].name
            );
        }
        return Ok(());
    }
    let parts: Vec<&lp::Partition> = match &names {
        None => m.partitions.iter().filter(|p| p.size() > 0).collect(),
        Some(names) => names
            .iter()
            .map(|n| {
                m.partition(n)
                    .ok_or_else(|| format!("no partition {} in the super image", n))
            })
            .collect::<Result<_, _>>()?,
    };
    fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {}", out_dir.display(), e))?;
    let start = Instant::now();
    for p in parts {
        let path = out_dir.join(format!("{}.img", p.name));
        out!("- {}: {} MiB -> {}", p.name, p.size() >> 20, path.display());
        let segs = m.segments(p).map_err(fail)?;
        let mut src = jancox_core::fs::Segments::new(&mut img, &segs);
        let mut dst = io::BufWriter::new(
            fs::File::create(&path).map_err(|e| format!("{}: {}", path.display(), e))?,
        );
        io::copy(&mut src, &mut dst).map_err(fail)?;
        dst.flush().map_err(fail)?;
    }
    out!("- Done in {:.1}s", start.elapsed().as_secs_f64());
    Ok(())
}

fn build(args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: jancox build <workdir> <part> [-o image] [-s size|auto] [-f]";
    let mut positional = Vec::new();
    let (mut output, mut size, mut force) = (None, build::Size::Original, false);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{} needs a value\n{}", arg, USAGE))
        };
        match arg.as_str() {
            "-o" | "--output" => output = Some(PathBuf::from(value()?)),
            "-s" | "--size" => size = value()?.parse()?,
            "-f" | "--force" => force = true,
            _ if !arg.starts_with('-') => positional.push(arg.clone()),
            _ => return Err(format!("unexpected argument: {}\n{}", arg, USAGE)),
        }
    }
    let [work, part] = positional.as_slice() else {
        return Err(USAGE.into());
    };
    let work = PathBuf::from(work);
    let output = output.unwrap_or_else(|| work.join(format!("{}.img", part)));
    if output.exists() && !force {
        return Err(format!(
            "{} already exists, use -f to overwrite",
            output.display()
        ));
    }

    let start = Instant::now();
    let sum = build::build(&work, part, &output, size, |msg| out!("{}", msg))
        .map_err(|e| format!("build failed: {}", e))?;
    for w in sum.warnings.iter().take(10) {
        out!("  [warning] {}", w);
    }
    if !sum.new_entries.is_empty() {
        out!(
            "- {} new entries got default metadata, e.g. {}",
            sum.new_entries.len(),
            sum.new_entries
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if sum.removed > 0 {
        out!("- {} entries from fs_config no longer exist", sum.removed);
    }
    let st = &sum.stats;
    out!(
        "- Done: {} ({} dirs, {} files, {} symlinks) in {:.2}s",
        output.display(),
        sum.dirs,
        sum.files,
        sum.symlinks,
        start.elapsed().as_secs_f64()
    );
    out!(
        "  blocks {}/{} used, inodes {}/{} used",
        st.used_blocks,
        st.blocks,
        st.used_inodes,
        st.inodes
    );
    Ok(())
}

/// Splits `-w/--workdir` off the arguments.
/// Default work folder (holds input/ and output/): the folder jancox is run in.
fn default_workdir() -> PathBuf {
    PathBuf::from(".")
}

fn workdir(args: &[String], usage: &str) -> Result<(PathBuf, Vec<String>), String> {
    let mut work = default_workdir();
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "-w" || arg == "--workdir" {
            work = PathBuf::from(
                it.next()
                    .ok_or_else(|| format!("{} needs a value\n{}", arg, usage))?,
            );
        } else {
            rest.push(arg.clone());
        }
    }
    Ok((work, rest))
}

fn unpack(args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: jancox unpack [rom.zip] [-w workdir]";
    let (work, rest) = workdir(args, USAGE)?;
    let input = match rest.as_slice() {
        [] => match rom::find_input(&work) {
            Some(zip) => zip,
            None => {
                // set up the folder so the user knows where the ROM goes
                let _ = rom::init(&work);
                return Err(format!(
                    "no ROM zip found; put it in {} or give its path\n{}",
                    work.join("input").display(),
                    USAGE
                ));
            }
        },
        [zip] if !zip.starts_with('-') => PathBuf::from(zip),
        _ => return Err(USAGE.into()),
    };
    let start = Instant::now();
    let sum = rom::unpack(&input, &work, |msg| out!("{}", msg))
        .map_err(|e| format!("unpack failed: {}", e))?;
    out!(" ");
    for (k, v) in &sum.rom_info {
        out!("  {:<16}: {}", k, v);
    }
    out!(" ");
    out!(
        "- Done in {:.1}s: edit {}/<name>/, then run: jancox repack",
        start.elapsed().as_secs_f64(),
        rom::partition_dir(&work).display()
    );
    Ok(())
}

fn repack(args: &[String]) -> Result<(), String> {
    const USAGE: &str =
        "usage: jancox repack [-w workdir] [-o out.zip] [-b brotli_quality] [-z zip_level] [-t auto|fastboot|sdat|payload|super,...]";
    let (work, rest) = workdir(args, USAGE)?;
    let mut opts = rom::load_config(&work).map_err(|e| format!("repack failed: {}", e))?;
    let mut output = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{} needs a value\n{}", arg, USAGE))
        };
        match arg.as_str() {
            "-o" | "--output" => output = Some(PathBuf::from(value()?)),
            "-b" | "--brotli" => {
                opts.brotli_quality = value()?.parse().map_err(|_| USAGE.to_string())?
            }
            "-z" | "--zip-level" => {
                opts.zip_level = value()?.parse().map_err(|_| USAGE.to_string())?
            }
            "-t" | "--type" => {
                opts.output = rom::Target::parse_list(&value()?).ok_or_else(|| USAGE.to_string())?
            }
            _ => return Err(format!("unexpected argument: {}\n{}", arg, USAGE)),
        }
    }
    let start = Instant::now();
    let zips = rom::repack(&work, output.as_deref(), opts, |msg| out!("{}", msg))
        .map_err(|e| format!("repack failed: {}", e))?;
    out!("- Done in {:.1}s:", start.elapsed().as_secs_f64());
    for zip in &zips {
        out!("  {} ({} bytes)", zip.display(), file_size(zip));
    }
    Ok(())
}

fn init(args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: jancox init [-w workdir]";
    let (work, rest) = workdir(args, USAGE)?;
    if !rest.is_empty() {
        return Err(USAGE.into());
    }
    let made = rom::init(&work).map_err(|e| format!("init failed: {}", e))?;
    for p in &made {
        out!("   Created -> {}", p.display());
    }
    if made.is_empty() {
        out!("- Nothing to do: input/, output/ and jancox.prop already exist");
    }
    out!(
        "- Put the ROM zip in {}, then run: jancox unpack",
        work.join("input").display()
    );
    Ok(())
}

fn cleanup(args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: jancox cleanup [-w workdir] [--all]";
    let (work, rest) = workdir(args, USAGE)?;
    let all = match rest.as_slice() {
        [] => false,
        [a] if a == "--all" => true,
        _ => return Err(USAGE.into()),
    };
    let removed = rom::cleanup(&work, all).map_err(|e| format!("cleanup failed: {}", e))?;
    for p in &removed {
        out!("   Removing -> {}", p.display());
    }
    out!("- Done ({} removed)", removed.len());
    Ok(())
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    let result = match args.first().map(String::as_str) {
        Some("init") => init(&args[1..]),
        Some("unpack") => unpack(&args[1..]),
        Some("repack") => repack(&args[1..]),
        Some("cleanup") => cleanup(&args[1..]),
        Some("sdat2img") => sdat2img(&args[1..]),
        Some("img2sdat") => img2sdat(&args[1..]),
        Some("brotli") => brotli(&args[1..]),
        Some("extract") => extract(&args[1..]),
        Some("build") => build(&args[1..]),
        Some("payload") => payload(&args[1..]),
        Some("super") => super_img(&args[1..]),
        Some("-V" | "--version" | "version") => {
            out!("jancox {}", VERSION);
            Ok(())
        }
        Some("-h" | "--help" | "help") => {
            usage();
            Ok(())
        }
        Some(cmd) => Err(format!("unknown command: {} (see: jancox help)", cmd)),
        None => {
            usage();
            process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("[!] {}", e);
        process::exit(1);
    }
}
