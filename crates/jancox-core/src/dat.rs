//! `*.img` -> `*.new.dat[.br]` + `*.transfer.list`, built on the
//! [img2sdat](https://github.com/wahyu6070/img2sdat-rust) crate.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

use img2sdat::{Image, Plan, Result, BLOCK_SIZE};

use crate::br;

/// Like `img2sdat::convert`, but with `brotli = Some(quality)` new.dat is
/// compressed on the fly into `<prefix>.new.dat.br`, so the uncompressed
/// new.dat is never written to disk.
pub fn img_to_dat<P, Q>(
    image: P,
    out_dir: Q,
    prefix: &str,
    version: u32,
    brotli: Option<u32>,
    mut log: impl FnMut(&str),
) -> Result<Plan>
where
    P: AsRef<Path>,
    Q: AsRef<Path>,
{
    let Some(quality) = brotli else {
        return img2sdat::convert(image, out_dir, prefix, version, log);
    };
    // fail on a bad quality before reading the whole image
    br::Encoder::new(Vec::new(), quality, br::DEFAULT_LGWIN)?;

    let out_dir = out_dir.as_ref();
    let mut image = Image::open(image)?;
    log(&format!(
        "Total of {} {}-byte output blocks in {} input chunks.",
        image.total_blocks, BLOCK_SIZE, image.total_chunks
    ));
    let plan = Plan::new(&mut image, version)?;

    fs::create_dir_all(out_dir).map_err(|e| with_path(e, out_dir))?;
    let create = |ext: &str| {
        let path = out_dir.join(format!("{}.{}", prefix, ext));
        File::create(&path).map_err(|e| with_path(e, &path))
    };

    let out = BufWriter::with_capacity(1 << 20, create("new.dat.br")?);
    let mut enc = br::Encoder::new(out, quality, br::DEFAULT_LGWIN)?;
    plan.write_new_data(&mut image, &mut enc)?;
    enc.finish()?.flush()?;
    create("patch.dat")?;
    create("transfer.list")?.write_all(plan.transfer_list().as_bytes())?;
    Ok(plan)
}

fn with_path(e: std::io::Error, path: &Path) -> img2sdat::Error {
    img2sdat::Error::Io(std::io::Error::new(
        e.kind(),
        format!("{}: {}", path.display(), e),
    ))
}
