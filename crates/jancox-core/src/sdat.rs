//! `*.new.dat[.br]` + `*.transfer.list` -> raw image, built on the
//! [sdat2img](https://github.com/wahyu6070/sdat2img-rust) crate.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use crate::br;

pub use sdat2img::{android_version_name, Error, Result, TransferList};

const BUF_SIZE: usize = 1 << 20;

/// Converts `new_data` into a raw image at `output`, following `transfer_list`.
///
/// A brotli compressed `new_data` (`*.new.dat.br`) is decompressed on the fly,
/// so the intermediate `*.new.dat` is never written to disk.
pub fn dat_to_img<P, Q, S>(
    transfer_list: P,
    new_data: Q,
    output: S,
    log: impl FnMut(&str),
) -> Result<TransferList>
where
    P: AsRef<Path>,
    Q: AsRef<Path>,
    S: AsRef<Path>,
{
    let list = sdat2img::parse_transfer_list(BufReader::new(open(transfer_list.as_ref())?))?;

    let new_data = new_data.as_ref();
    let file = open(new_data)?;
    let mut reader: Box<dyn Read> = if is_brotli(new_data) {
        Box::new(br::decoder(file))
    } else {
        Box::new(BufReader::with_capacity(BUF_SIZE, file))
    };

    write_img(&list, &mut reader, output, log)?;
    Ok(list)
}

/// Writes the image described by `list` to `output`, taking block data from
/// any reader (plain, brotli, ...).
pub fn write_img<R: Read>(
    list: &TransferList,
    new_data: &mut R,
    output: impl AsRef<Path>,
    log: impl FnMut(&str),
) -> Result<()> {
    let output = output.as_ref();
    let mut img = File::create(output).map_err(|e| with_path(e, output))?;
    sdat2img::write_image(list, new_data, &mut img, log)?;

    // Same as sdat2img::convert: make file larger if necessary
    let size = list.max_file_size()?;
    if img.metadata()?.len() < size {
        img.set_len(size)?;
    }
    Ok(())
}

fn open(path: &Path) -> Result<File> {
    File::open(path).map_err(|e| with_path(e, path))
}

/// Adds the file name to an I/O error, e.g. "system.new.dat: No such file or directory".
fn with_path(e: io::Error, path: &Path) -> Error {
    Error::Io(io::Error::new(
        e.kind(),
        format!("{}: {}", path.display(), e),
    ))
}

fn is_brotli(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("br"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdat2img::BLOCK_SIZE;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("jancox-{}-{}", name, std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // Blocks 4..6 then 0..2 come from new.dat; zero reaches block 10, past the
    // last new block, so the image must be extended with set_len.
    const TRANSFER_LIST: &str = "4\n4\n0\n0\nerase 2,0,10\nnew 4,4,6,0,2\nzero 2,6,10\n";

    fn new_dat() -> Vec<u8> {
        (1..=4u8)
            .flat_map(|b| std::iter::repeat_n(b, BLOCK_SIZE as usize))
            .collect()
    }

    fn expected_img() -> Vec<u8> {
        let bs = BLOCK_SIZE as usize;
        let mut img = vec![0u8; 10 * bs];
        img[4 * bs..5 * bs].fill(1);
        img[5 * bs..6 * bs].fill(2);
        img[..bs].fill(3);
        img[bs..2 * bs].fill(4);
        img
    }

    #[test]
    fn plain_dat() {
        let tmp = TempDir::new("plain");
        fs::write(tmp.0.join("system.transfer.list"), TRANSFER_LIST).unwrap();
        fs::write(tmp.0.join("system.new.dat"), new_dat()).unwrap();

        let list = dat_to_img(
            tmp.0.join("system.transfer.list"),
            tmp.0.join("system.new.dat"),
            tmp.0.join("system.img"),
            |_| {},
        )
        .unwrap();

        assert_eq!(list.version, 4);
        assert_eq!(fs::read(tmp.0.join("system.img")).unwrap(), expected_img());
    }

    #[test]
    fn brotli_dat() {
        let tmp = TempDir::new("brotli");
        fs::write(tmp.0.join("system.transfer.list"), TRANSFER_LIST).unwrap();
        {
            let out = File::create(tmp.0.join("system.new.dat.br")).unwrap();
            let mut w = brotli::CompressorWriter::new(out, BUF_SIZE, 1, 24);
            w.write_all(&new_dat()).unwrap();
        }

        dat_to_img(
            tmp.0.join("system.transfer.list"),
            tmp.0.join("system.new.dat.br"),
            tmp.0.join("system.img"),
            |_| {},
        )
        .unwrap();

        assert_eq!(fs::read(tmp.0.join("system.img")).unwrap(), expected_img());
    }

    #[test]
    fn truncated_brotli_is_an_error() {
        let tmp = TempDir::new("truncated");
        fs::write(tmp.0.join("system.transfer.list"), TRANSFER_LIST).unwrap();
        let mut br = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut br, BUF_SIZE, 1, 24);
            w.write_all(&new_dat()).unwrap();
        }
        br.truncate(br.len() / 2);
        fs::write(tmp.0.join("system.new.dat.br"), br).unwrap();

        assert!(dat_to_img(
            tmp.0.join("system.transfer.list"),
            tmp.0.join("system.new.dat.br"),
            tmp.0.join("system.img"),
            |_| {},
        )
        .is_err());
    }
}
