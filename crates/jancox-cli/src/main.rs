use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::time::Instant;

use jancox_core::{br, dat, extract, img2sdat, sdat};

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
    out!("Commands:");
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
    out!("      Extract an ext4 image to <outdir>/<name>/ plus metadata in <outdir>/config/");
    out!("      (default: -o . -p <image name>)");
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

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    let result = match args.first().map(String::as_str) {
        Some("sdat2img") => sdat2img(&args[1..]),
        Some("img2sdat") => img2sdat(&args[1..]),
        Some("brotli") => brotli(&args[1..]),
        Some("extract") => extract(&args[1..]),
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
