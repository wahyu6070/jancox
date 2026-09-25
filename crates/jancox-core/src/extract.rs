//! Extracts a filesystem image into a folder and saves the metadata a
//! folder can't hold (owners, modes, SELinux labels, capabilities, symlinks)
//! next to it, so the image can be rebuilt later:
//!
//! ```text
//! <out>/<part>/                    files
//! <out>/config/<part>_fs_config     <path> <uid> <gid> <mode> [capabilities=0x..]
//! <out>/config/<part>_file_contexts <path regex> <selinux label>
//! <out>/config/<part>_symlinks      <path> <target>
//! <out>/config/<part>_info          filesystem parameters, key=value
//! ```
//!
//! Paths are device paths: the image root is the mount point, taken from
//! the volume name ("/" on system-as-root, "vendor" -> "/vendor", ...).
//! fs_config paths drop the leading "/" (the root of "/" is written as "/").

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::fs::ext4::{self, Ext4};
use crate::fs::{invalid, Filesystem, Kind, Meta};

const SPARSE_MAGIC: u32 = 0xED26_FF3A;
const EROFS_MAGIC: u32 = 0xE0F5_E1E2;

#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub fs_type: String,
    pub part: String,
    pub mount_point: String,
    pub dirs: u64,
    pub files: u64,
    pub symlinks: u64,
    /// Device nodes, fifos and sockets: only recorded in fs_config.
    pub special: u64,
    pub bytes: u64,
    /// Things that could not be represented, e.g. symlinks on a filesystem
    /// without symlink support. Their metadata is still saved.
    pub warnings: Vec<String>,
}

struct Entry {
    meta: Meta,
    link: Option<Vec<u8>>,
}

/// Extracts `image` into `out_dir`. `part` defaults to the image file stem
/// (`system.img` -> `system`).
pub fn extract(
    image: &Path,
    out_dir: &Path,
    part: Option<&str>,
    mut log: impl FnMut(&str),
) -> io::Result<Summary> {
    let with_path =
        |e: io::Error, p: &Path| io::Error::new(e.kind(), format!("{}: {}", p.display(), e));
    let mut file = File::open(image).map_err(|e| with_path(e, image))?;
    let mut head = vec![0u8; 2048];
    let n = read_up_to(&mut file, &mut head)?;
    head.truncate(n);

    let magic = |off: usize| {
        head.get(off..off + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    if magic(0) == Some(SPARSE_MAGIC) {
        return Err(invalid(
            "this is an Android sparse image; convert it to a raw image first (simg2img)",
        ));
    }
    if magic(1024) == Some(EROFS_MAGIC) {
        return Err(invalid("EROFS images are not supported yet"));
    }
    if !ext4::is_ext4(&head) {
        return Err(invalid("unknown filesystem (not ext4 or EROFS)"));
    }

    let part = match part {
        Some(p) => p.to_string(),
        None => image
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "system".into()),
    };
    let mut fs = Ext4::open(file)?;
    extract_fs(&mut fs, out_dir, &part, &mut log)
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

/// Mount point from the volume name: "/" stays, "vendor" -> "/vendor",
/// empty -> "/<part>".
fn mount_point(volume: &str, part: &str) -> String {
    let v = volume.trim();
    if v.starts_with('/') {
        v.to_string()
    } else if !v.is_empty() {
        format!("/{}", v)
    } else {
        format!("/{}", part)
    }
}

pub fn extract_fs<F: Filesystem>(
    fs: &mut F,
    out_dir: &Path,
    part: &str,
    log: &mut impl FnMut(&str),
) -> io::Result<Summary> {
    let mount = mount_point(&fs.volume_name(), part);
    let root_dir = out_dir.join(part);
    if root_dir.exists() && fs::read_dir(&root_dir)?.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} is not empty", root_dir.display()),
        ));
    }
    let config_dir = out_dir.join("config");
    fs::create_dir_all(&root_dir)?;
    fs::create_dir_all(&config_dir)?;
    log(&format!(
        "- Extracting {} ({}, mount point {}) to {}",
        part,
        fs.fs_type(),
        mount,
        root_dir.display()
    ));

    let mut sum = Summary {
        fs_type: fs.fs_type().to_string(),
        part: part.to_string(),
        mount_point: mount.clone(),
        ..Default::default()
    };
    // relative path (raw bytes, "" for the root) -> entry
    let mut entries: BTreeMap<Vec<u8>, Entry> = BTreeMap::new();
    let mut stack = vec![(Vec::new(), fs.root())];

    while let Some((rel, node)) = stack.pop() {
        let host = host_path(&root_dir, &rel);
        let (meta, link) =
            extract_one(fs, node, &rel, &host, part, &mut stack, &mut sum).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("{}: {}", String::from_utf8_lossy(&rel), e),
                )
            })?;
        entries.insert(rel, Entry { meta, link });
    }

    write_config(&config_dir, part, &mount, &entries, &mut sum)?;
    let mut info = fs.info();
    info.insert(0, ("part".into(), part.to_string()));
    info.insert(1, ("mount_point".into(), mount.clone()));
    let mut text = String::new();
    for (k, v) in info {
        text.push_str(&format!("{}={}\n", k, v));
    }
    fs::write(config_dir.join(format!("{}_info", part)), text)?;
    Ok(sum)
}

type Stack<N> = Vec<(Vec<u8>, N)>;

/// Extracts one node to `host`; directories push their children on `stack`.
fn extract_one<F: Filesystem>(
    fs: &mut F,
    node: F::Node,
    rel: &[u8],
    host: &Path,
    part: &str,
    stack: &mut Stack<F::Node>,
    sum: &mut Summary,
) -> io::Result<(Meta, Option<Vec<u8>>)> {
    let meta = fs.meta(node)?;
    let mut link = None;
    match meta.kind {
        Kind::Dir => {
            fs::create_dir_all(host)?;
            sum.dirs += 1;
            let mut children = fs.read_dir(node)?;
            children.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, child) in children.into_iter().rev() {
                // mke2fs / e2fsdroid recreate lost+found
                if rel.is_empty() && name == b"lost+found" {
                    continue;
                }
                let mut path = rel.to_vec();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(&name);
                stack.push((path, child));
            }
        }
        Kind::File => {
            let mut out = BufWriter::with_capacity(1 << 20, File::create(host)?);
            sum.bytes += fs.read_file(node, &mut out)?;
            out.flush()?;
            sum.files += 1;
        }
        Kind::Symlink => {
            let target = fs.read_link(node)?;
            if let Err(e) = make_symlink(&target, host) {
                sum.warnings.push(format!(
                    "symlink {} not created ({}), kept in {}_symlinks",
                    String::from_utf8_lossy(rel),
                    e,
                    part
                ));
            }
            link = Some(target);
            sum.symlinks += 1;
        }
        _ => sum.special += 1,
    }
    Ok((meta, link))
}

fn write_config(
    dir: &Path,
    part: &str,
    mount: &str,
    entries: &BTreeMap<Vec<u8>, Entry>,
    sum: &mut Summary,
) -> io::Result<()> {
    let create = |name: &str| -> io::Result<BufWriter<File>> {
        Ok(BufWriter::new(File::create(
            dir.join(format!("{}_{}", part, name)),
        )?))
    };
    let mut fs_config = create("fs_config")?;
    let mut contexts = create("file_contexts")?;
    let mut symlinks = create("symlinks")?;

    for (rel, e) in entries {
        let rel_str = String::from_utf8_lossy(rel);
        if std::str::from_utf8(rel).is_err() || rel_str.chars().any(char::is_whitespace) {
            sum.warnings.push(format!(
                "path {:?} has whitespace or invalid UTF-8; its metadata lines may not parse",
                rel_str
            ));
        }
        // device path, e.g. /vendor/bin/sh or /system/bin/sh on system-as-root
        let device = match (mount, rel.is_empty()) {
            (_, true) => mount.to_string(),
            ("/", false) => format!("/{}", rel_str),
            (_, false) => format!("{}/{}", mount, rel_str),
        };
        let config_path = match device.trim_start_matches('/') {
            "" => "/",
            p => p,
        };

        let m = &e.meta;
        write!(
            fs_config,
            "{} {} {} {:04o}",
            config_path, m.uid, m.gid, m.mode
        )?;
        if let Some(caps) = m.capabilities {
            write!(fs_config, " capabilities=0x{:x}", caps)?;
        }
        writeln!(fs_config)?;
        if let Some(label) = &m.selinux {
            writeln!(contexts, "{} {}", regex_escape(&device), label)?;
        }
        if let Some(target) = &e.link {
            writeln!(
                symlinks,
                "{} {}",
                config_path,
                String::from_utf8_lossy(target)
            )?;
        }
    }
    fs_config.flush()?;
    contexts.flush()?;
    symlinks.flush()
}

/// Escapes regex metacharacters for file_contexts, e.g. `libc++.so` -> `libc\+\+\.so`.
fn regex_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if ".^$*+?()[]{}|\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(unix)]
fn host_name(name: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(name).to_os_string()
}

/// Windows can't hold every Linux name: replace the characters it rejects.
#[cfg(not(unix))]
fn host_name(name: &[u8]) -> OsString {
    String::from_utf8_lossy(name)
        .chars()
        .map(|c| {
            if "<>:\"\\|?*".contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect::<String>()
        .into()
}

fn host_path(root: &Path, rel: &[u8]) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.split(|&c| c == b'/').filter(|s| !s.is_empty()) {
        p.push(host_name(part));
    }
    p
}

#[cfg(unix)]
fn make_symlink(target: &[u8], at: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(host_name(target), at)
}

#[cfg(not(unix))]
fn make_symlink(_target: &[u8], _at: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no symlinks on this system",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_points() {
        assert_eq!(mount_point("/", "system"), "/");
        assert_eq!(mount_point("vendor", "vendor"), "/vendor");
        assert_eq!(mount_point("", "product"), "/product");
        assert_eq!(mount_point("/odm", "x"), "/odm");
    }

    #[test]
    fn escape() {
        assert_eq!(
            regex_escape("/system/lib/libc++.so"),
            "/system/lib/libc\\+\\+\\.so"
        );
        assert_eq!(regex_escape("/a(b)[c]"), "/a\\(b\\)\\[c\\]");
    }
}
