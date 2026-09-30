//! Tar archives (`.tar`, `.tgz`): Xiaomi fastboot ROMs ship as
//! `<device>_images_<version>.tgz`. Reading streams the entries (ustar,
//! GNU long names, pax `path`); writing makes ustar entries with GNU long
//! names when a path doesn't fit.

use std::io::{self, Read, Write};

use crate::fs::invalid;

const BLOCK: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    /// Hard links, devices, fifos: kept out of the unpacked ROM.
    Other,
}

#[derive(Debug, Clone)]
pub struct Header {
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub mode: u32,
    pub mtime: u64,
    pub link: String,
}

fn octal(field: &[u8]) -> io::Result<u64> {
    // GNU base-256 for big numbers
    if field.first().is_some_and(|&b| b & 0x80 != 0) {
        let mut v = (field[0] & 0x7f) as u64;
        for &b in &field[1..] {
            v = v
                .checked_mul(256)
                .ok_or_else(|| invalid("tar: number too big"))?
                | b as u64;
        }
        return Ok(v);
    }
    let s: String = field
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as char)
        .collect();
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(s, 8).map_err(|_| invalid("tar: bad number"))
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Reads the entries of a tar stream one after another.
pub struct Reader<R> {
    r: R,
    /// Bytes left of the current entry's data plus padding.
    left: u64,
}

impl<R: Read> Reader<R> {
    pub fn new(r: R) -> Self {
        Reader { r, left: 0 }
    }

    fn skip(&mut self, mut n: u64) -> io::Result<()> {
        let mut buf = [0u8; 8192];
        while n > 0 {
            let k = n.min(buf.len() as u64) as usize;
            self.r.read_exact(&mut buf[..k])?;
            n -= k as u64;
        }
        Ok(())
    }

    fn read_data(&mut self, size: u64) -> io::Result<Vec<u8>> {
        let mut v = vec![0u8; size as usize];
        self.r.read_exact(&mut v)?;
        self.skip(size.next_multiple_of(BLOCK as u64) - size)?;
        Ok(v)
    }

    /// The next entry, or `None` at the end. The previous entry's data is
    /// skipped if it was not read.
    pub fn next_entry(&mut self) -> io::Result<Option<Header>> {
        let left = self.left;
        self.skip(left)?;
        self.left = 0;
        let mut long_name: Option<String> = None;
        let mut long_link: Option<String> = None;
        loop {
            let mut h = [0u8; BLOCK];
            match self.r.read_exact(&mut h) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(e),
            }
            if h.iter().all(|&b| b == 0) {
                return Ok(None);
            }
            let sum: u64 = h
                .iter()
                .enumerate()
                .map(|(i, &b)| {
                    if (148..156).contains(&i) {
                        32
                    } else {
                        b as u64
                    }
                })
                .sum();
            if octal(&h[148..156])? != sum {
                return Err(invalid("tar: bad header checksum"));
            }
            let size = octal(&h[124..136])?;
            let flag = h[156];
            match flag {
                b'L' => {
                    long_name = Some(cstr(&self.read_data(size)?));
                    continue;
                }
                b'K' => {
                    long_link = Some(cstr(&self.read_data(size)?));
                    continue;
                }
                b'x' => {
                    // pax: "len key=value\n" records
                    let data = self.read_data(size)?;
                    let text = String::from_utf8_lossy(&data).into_owned();
                    for rec in text.lines() {
                        if let Some((_, kv)) = rec.split_once(' ') {
                            match kv.split_once('=') {
                                Some(("path", v)) => long_name = Some(v.to_string()),
                                Some(("linkpath", v)) => long_link = Some(v.to_string()),
                                _ => {}
                            }
                        }
                    }
                    continue;
                }
                b'g' => {
                    self.read_data(size)?;
                    continue;
                }
                _ => {}
            }
            let ustar = &h[257..262] == b"ustar";
            let mut path = cstr(&h[..100]);
            if ustar && h[345] != 0 {
                path = format!("{}/{}", cstr(&h[345..500]), path);
            }
            let path = long_name.take().unwrap_or(path);
            let kind = match flag {
                b'0' | 0 | b'7' => Kind::File,
                b'5' => Kind::Dir,
                b'2' => Kind::Symlink,
                _ => Kind::Other,
            };
            let has_data = matches!(kind, Kind::File | Kind::Other) && flag != b'1';
            self.left = if has_data {
                size.next_multiple_of(BLOCK as u64)
            } else {
                0
            };
            return Ok(Some(Header {
                path,
                kind,
                size: if has_data { size } else { 0 },
                mode: octal(&h[100..108])? as u32,
                mtime: octal(&h[136..148])?,
                link: long_link.take().unwrap_or_else(|| cstr(&h[157..257])),
            }));
        }
    }

    /// Copies the data of the entry just returned by `next_entry`.
    pub fn copy_data(&mut self, size: u64, out: &mut impl Write) -> io::Result<()> {
        let n = io::copy(&mut (&mut self.r).take(size), out)?;
        if n != size {
            return Err(invalid("tar: truncated entry"));
        }
        self.left -= size;
        Ok(())
    }
}

fn put_octal(field: &mut [u8], v: u64) {
    let s = format!("{:0width$o}", v, width = field.len() - 1);
    if s.len() < field.len() {
        field[..s.len()].copy_from_slice(s.as_bytes());
    } else {
        // GNU base-256
        field.fill(0);
        field[0] = 0x80;
        let n = field.len();
        for (i, b) in v.to_be_bytes().iter().rev().enumerate() {
            if i + 1 < n {
                field[n - 1 - i] = *b;
            }
        }
    }
}

/// Writes a tar stream.
pub struct Writer<W: Write> {
    w: W,
}

impl<W: Write> Writer<W> {
    pub fn new(w: W) -> Self {
        Writer { w }
    }

    fn header(
        &mut self,
        path: &str,
        flag: u8,
        size: u64,
        mode: u32,
        mtime: u64,
        link: &str,
    ) -> io::Result<()> {
        if path.len() > 100 {
            self.header("././@LongLink", b'L', path.len() as u64 + 1, 0o644, 0, "")?;
            self.w.write_all(path.as_bytes())?;
            self.w.write_all(&[0])?;
            self.pad(path.len() as u64 + 1)?;
        }
        let mut h = [0u8; BLOCK];
        let name = &path.as_bytes()[..path.len().min(100)];
        h[..name.len()].copy_from_slice(name);
        put_octal(&mut h[100..108], mode as u64 & 0o7777);
        put_octal(&mut h[108..116], 0);
        put_octal(&mut h[116..124], 0);
        put_octal(&mut h[124..136], size);
        put_octal(&mut h[136..148], mtime);
        h[156] = flag;
        let l = &link.as_bytes()[..link.len().min(100)];
        h[157..157 + l.len()].copy_from_slice(l);
        h[257..263].copy_from_slice(b"ustar ");
        h[263..265].copy_from_slice(b" \0");
        h[148..156].fill(b' ');
        let sum: u64 = h.iter().map(|&b| b as u64).sum();
        let s = format!("{:06o}\0 ", sum);
        h[148..156].copy_from_slice(s.as_bytes());
        self.w.write_all(&h)
    }

    fn pad(&mut self, size: u64) -> io::Result<()> {
        let pad = (size.next_multiple_of(BLOCK as u64) - size) as usize;
        self.w.write_all(&[0u8; BLOCK][..pad])
    }

    pub fn dir(&mut self, path: &str, mode: u32, mtime: u64) -> io::Result<()> {
        let p = if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{}/", path)
        };
        self.header(&p, b'5', 0, mode, mtime, "")
    }

    pub fn symlink(&mut self, path: &str, target: &str, mtime: u64) -> io::Result<()> {
        if target.len() > 100 {
            self.header("././@LongLink", b'K', target.len() as u64 + 1, 0o644, 0, "")?;
            self.w.write_all(target.as_bytes())?;
            self.w.write_all(&[0])?;
            self.pad(target.len() as u64 + 1)?;
        }
        self.header(path, b'2', 0, 0o777, mtime, target)
    }

    /// A file of `size` bytes read from `data`.
    pub fn file(
        &mut self,
        path: &str,
        mode: u32,
        mtime: u64,
        size: u64,
        data: &mut impl Read,
    ) -> io::Result<()> {
        self.header(path, b'0', size, mode, mtime, "")?;
        let n = io::copy(&mut data.take(size), &mut self.w)?;
        if n != size {
            return Err(invalid(format!("{} changed while writing", path)));
        }
        self.pad(size)
    }

    /// Ends the archive (two zero blocks) and returns the writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.w.write_all(&[0u8; 2 * BLOCK])?;
        Ok(self.w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let long = format!("{}/images/super.img", "d".repeat(120));
        let mut w = Writer::new(Vec::new());
        w.dir("rom", 0o755, 5).unwrap();
        w.file("rom/flash_all.sh", 0o755, 5, 6, &mut &b"#!/sh\n"[..])
            .unwrap();
        w.file(&long, 0o644, 5, 3, &mut &b"abc"[..]).unwrap();
        w.symlink("rom/link", &"t".repeat(150), 5).unwrap();
        let data = w.finish().unwrap();
        let mut r = Reader::new(&data[..]);
        let e = r.next_entry().unwrap().unwrap();
        assert_eq!((e.path.as_str(), e.kind), ("rom/", Kind::Dir));
        let e = r.next_entry().unwrap().unwrap();
        assert_eq!(
            (e.path.as_str(), e.mode, e.size),
            ("rom/flash_all.sh", 0o755, 6)
        );
        // not read: skipped by the next call
        let e = r.next_entry().unwrap().unwrap();
        assert_eq!(e.path, long);
        let mut v = Vec::new();
        r.copy_data(e.size, &mut v).unwrap();
        assert_eq!(v, b"abc");
        let e = r.next_entry().unwrap().unwrap();
        assert_eq!((e.kind, e.link.len()), (Kind::Symlink, 150));
        assert!(r.next_entry().unwrap().is_none());
        // system tar reads ours: checked by hand with `tar tvf`
    }
}
