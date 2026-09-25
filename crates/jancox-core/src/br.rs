//! Brotli (`*.br`) compression, built on the `brotli` crate.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

/// Quality used by the old repack.sh (`brotli.level=1` in jancox.prop).
pub const DEFAULT_QUALITY: u32 = 1;
/// Window size used for ROMs (`brotli -w 24`), the largest standard one.
pub const DEFAULT_LGWIN: u32 = 24;

const BUF_SIZE: usize = 1 << 20;

/// Remembers the first write error. `brotli::CompressorWriter` drops the
/// errors of its final flush, which would leave a truncated file unnoticed.
struct ErrorTrap<W> {
    inner: W,
    error: Option<io::Error>,
}

impl<W: Write> ErrorTrap<W> {
    fn check<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        result.inspect_err(|e| {
            if self.error.is_none() {
                self.error = Some(io::Error::new(e.kind(), e.to_string()));
            }
        })
    }
}

impl<W: Write> Write for ErrorTrap<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let result = self.inner.write(buf);
        self.check(result)
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = self.inner.flush();
        self.check(result)
    }
}

/// Streaming brotli compressor. Call [`Encoder::finish`] at the end: it
/// writes the end of the stream and reports errors that `drop` would hide.
pub struct Encoder<W: Write> {
    inner: brotli::CompressorWriter<ErrorTrap<W>>,
}

impl<W: Write> Encoder<W> {
    /// `quality` is 0-11, `lgwin` (window size, log2) is 10-24.
    pub fn new(out: W, quality: u32, lgwin: u32) -> io::Result<Self> {
        check_params(quality, lgwin)?;
        let trap = ErrorTrap {
            inner: out,
            error: None,
        };
        Ok(Encoder {
            inner: brotli::CompressorWriter::new(trap, BUF_SIZE, quality, lgwin),
        })
    }

    pub fn finish(self) -> io::Result<W> {
        let mut trap = self.inner.into_inner();
        if let Some(e) = trap.error.take() {
            return Err(e);
        }
        trap.inner.flush()?;
        Ok(trap.inner)
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Streaming brotli decompressor.
pub fn decoder<R: Read>(input: R) -> impl Read {
    brotli::Decompressor::new(input, BUF_SIZE)
}

fn check_params(quality: u32, lgwin: u32) -> io::Result<()> {
    if quality > 11 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("brotli quality must be 0-11, not {}", quality),
        ));
    }
    if !(10..=24).contains(&lgwin) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("brotli window must be 10-24, not {}", lgwin),
        ));
    }
    Ok(())
}

fn with_path(e: io::Error, path: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {}", path.display(), e))
}

/// Runs `f` to fill `output`; removes the partial file when it fails.
fn write_file(output: &Path, f: impl FnOnce(File) -> io::Result<u64>) -> io::Result<u64> {
    let file = File::create(output).map_err(|e| with_path(e, output))?;
    f(file).inspect_err(|_| {
        let _ = fs::remove_file(output);
    })
}

/// Compresses `input` into `output`. Returns the size of `output`.
pub fn compress_file(input: &Path, output: &Path, quality: u32, lgwin: u32) -> io::Result<u64> {
    check_params(quality, lgwin)?;
    let mut src = BufReader::with_capacity(
        BUF_SIZE,
        File::open(input).map_err(|e| with_path(e, input))?,
    );
    write_file(output, |file| {
        let mut enc = Encoder::new(BufWriter::with_capacity(BUF_SIZE, file), quality, lgwin)?;
        io::copy(&mut src, &mut enc)?;
        let file = enc.finish()?.into_inner().map_err(|e| e.into_error())?;
        Ok(file.metadata()?.len())
    })
}

/// Decompresses `input` into `output`. Returns the size of `output`.
pub fn decompress_file(input: &Path, output: &Path) -> io::Result<u64> {
    let src = File::open(input).map_err(|e| with_path(e, input))?;
    write_file(output, |file| {
        let mut out = BufWriter::with_capacity(BUF_SIZE, file);
        let n = io::copy(&mut decoder(src), &mut out)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", input.display(), e)))?;
        out.flush()?;
        Ok(n)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn sample() -> Vec<u8> {
        (0..300_000u32)
            .flat_map(|i| (i % 251).to_le_bytes())
            .collect()
    }

    #[test]
    fn roundtrip() {
        for (quality, lgwin) in [(0, 10), (1, 24), (6, 22), (11, 24)] {
            let mut enc = Encoder::new(Vec::new(), quality, lgwin).unwrap();
            enc.write_all(&sample()).unwrap();
            let br = enc.finish().unwrap();
            assert!(br.len() < sample().len());

            let mut back = Vec::new();
            decoder(&br[..]).read_to_end(&mut back).unwrap();
            assert_eq!(back, sample(), "quality {} lgwin {}", quality, lgwin);
        }
    }

    #[test]
    fn bad_params() {
        assert!(Encoder::new(Vec::new(), 12, 24).is_err());
        assert!(Encoder::new(Vec::new(), 1, 25).is_err());
        assert!(Encoder::new(Vec::new(), 1, 9).is_err());
    }

    /// Fails like a full disk once `full` is set.
    struct Disk {
        full: Rc<Cell<bool>>,
    }

    impl Write for Disk {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.full.get() {
                return Err(io::Error::new(io::ErrorKind::StorageFull, "disk full"));
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_error_in_final_flush_is_reported() {
        // brotli::CompressorWriter ignores this error; Encoder::finish must not.
        let full = Rc::new(Cell::new(false));
        let mut enc = Encoder::new(Disk { full: full.clone() }, 1, 24).unwrap();
        enc.write_all(&sample()).unwrap();
        full.set(true);
        let err = enc.finish().err().expect("finish must fail");
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
    }

    #[test]
    fn corrupt_input_is_an_error() {
        let mut enc = Encoder::new(Vec::new(), 1, 24).unwrap();
        enc.write_all(&sample()).unwrap();
        let mut br = enc.finish().unwrap();
        br.truncate(br.len() / 2);
        assert!(decoder(&br[..]).read_to_end(&mut Vec::new()).is_err());
    }
}
