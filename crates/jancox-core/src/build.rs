//! Builds an ext4 image from a folder made by `extract` and its metadata:
//!
//! ```text
//! <work>/<part>/                     files (added or removed files are picked up)
//! <work>/config/<part>_fs_config      owners, modes, capabilities
//! <work>/config/<part>_file_contexts  SELinux labels
//! <work>/config/<part>_symlinks       symlinks (also those the folder can't hold)
//! <work>/config/<part>_info           size, UUID, hash seed, volume name, ...
//! ```
//!
//! The folder decides which files exist. Files without metadata get
//! Android-like defaults and the SELinux label of their directory. A
//! symlink listed in `_symlinks` is kept unless something else now sits at
//! its path; remove its line to delete it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use crate::extract::{config_path, device_path, mount_point};
use crate::fs::invalid;
use crate::fs::mkext4::{self, Node, NodeKind, Params};

/// Android build timestamp (2009-01-01), used when `_info` has none.
const DEFAULT_TIMESTAMP: u32 = 1_230_768_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// The size recorded in `_info` (the original partition size).
    Original,
    /// Just big enough for the content, plus a small margin.
    Auto,
    Bytes(u64),
}

impl std::str::FromStr for Size {
    type Err = String;

    /// "auto", or a byte count with an optional K/M/G suffix (powers of 1024).
    fn from_str(s: &str) -> Result<Size, String> {
        if s.eq_ignore_ascii_case("auto") {
            return Ok(Size::Auto);
        }
        let (num, mult) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
            Some('K') => (&s[..s.len() - 1], 1u64 << 10),
            Some('M') => (&s[..s.len() - 1], 1 << 20),
            Some('G') => (&s[..s.len() - 1], 1 << 30),
            _ => (s, 1),
        };
        num.parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(mult))
            .map(Size::Bytes)
            .ok_or_else(|| format!("bad size: {} (use auto, or bytes with K/M/G)", s))
    }
}

#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub mount_point: String,
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    /// Entries without fs_config metadata (new files), with defaults applied.
    pub new_entries: Vec<String>,
    /// fs_config entries whose path no longer exists (deleted files).
    pub removed: usize,
    pub stats: mkext4::Stats,
    pub block_size: u64,
    pub warnings: Vec<String>,
}

struct Meta {
    fs_config: HashMap<String, (u32, u32, u32, Option<u64>)>,
    contexts: HashMap<String, String>,
    symlinks: BTreeMap<String, Vec<u8>>,
    info: HashMap<String, String>,
}

fn read_lines(path: &Path) -> io::Result<Vec<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s.lines().map(str::to_string).collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(io::Error::new(
            e.kind(),
            format!("{}: {}", path.display(), e),
        )),
    }
}

fn load_meta(config: &Path, part: &str) -> io::Result<Meta> {
    let file = |name: &str| config.join(format!("{}_{}", part, name));
    let bad = |name: &str, line: &str| invalid(format!("{}_{}: bad line: {}", part, name, line));

    let mut fs_config = HashMap::new();
    for line in read_lines(&file("fs_config"))? {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.is_empty() {
            continue;
        }
        if f.len() < 4 {
            return Err(bad("fs_config", &line));
        }
        let num =
            |s: &str, radix| u32::from_str_radix(s, radix).map_err(|_| bad("fs_config", &line));
        let caps = match f.get(4).and_then(|c| c.strip_prefix("capabilities=")) {
            Some(c) => Some(
                u64::from_str_radix(c.trim_start_matches("0x"), 16)
                    .map_err(|_| bad("fs_config", &line))?,
            ),
            None => None,
        };
        fs_config.insert(
            f[0].to_string(),
            (num(f[1], 10)?, num(f[2], 10)?, num(f[3], 8)?, caps),
        );
    }

    let mut contexts = HashMap::new();
    for line in read_lines(&file("file_contexts"))? {
        let Some((path, label)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        // extract writes exact paths with regex metacharacters escaped
        let mut unescaped = String::with_capacity(path.len());
        let mut chars = path.chars();
        while let Some(c) = chars.next() {
            unescaped.push(if c == '\\' {
                chars.next().unwrap_or('\\')
            } else {
                c
            });
        }
        contexts.insert(unescaped, label.trim().to_string());
    }

    let mut symlinks = BTreeMap::new();
    for line in read_lines(&file("symlinks"))? {
        if let Some((path, target)) = line.split_once(' ') {
            symlinks.insert(path.to_string(), target.as_bytes().to_vec());
        }
    }

    let mut info = HashMap::new();
    for line in read_lines(&file("info"))? {
        if let Some((k, v)) = line.split_once('=') {
            info.insert(k.to_string(), v.to_string());
        }
    }
    Ok(Meta {
        fs_config,
        contexts,
        symlinks,
        info,
    })
}

struct Walker<'a> {
    meta: &'a Meta,
    mount: String,
    seen: HashSet<String>,
    sum: Summary,
}

impl Walker<'_> {
    /// Node with metadata for `rel` (relative path, "" for the root).
    fn node(
        &mut self,
        name: Vec<u8>,
        rel: &str,
        kind: NodeKind,
        parent_label: Option<&str>,
    ) -> Node {
        let device = device_path(&self.mount, rel);
        let cfg = config_path(&device).to_string();
        let known = self.meta.fs_config.get(&cfg).copied();
        let (uid, gid, mode, capabilities) = match known {
            Some(m) => m,
            None => {
                self.sum.new_entries.push(cfg.clone());
                default_meta(rel, &kind)
            }
        };
        self.seen.insert(cfg);
        // only new entries inherit their directory's label; a known entry
        // without one stays unlabeled
        let selinux = match (self.meta.contexts.get(&device), known) {
            (Some(label), _) => Some(label.clone()),
            (None, Some(_)) => None,
            (None, None) => parent_label.map(str::to_string),
        };
        Node {
            name,
            kind,
            mode,
            uid,
            gid,
            selinux,
            capabilities,
        }
    }

    fn dir(
        &mut self,
        host: &Path,
        rel: &str,
        name: Vec<u8>,
        parent_label: Option<&str>,
    ) -> io::Result<Node> {
        // metadata first so children inherit this directory's label
        let mut node = self.node(name, rel, NodeKind::Dir(Vec::new()), parent_label);
        let label = node.selinux.clone();
        let mut children = Vec::new();
        let mut entries: Vec<_> = fs::read_dir(host)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", host.display(), e)))?
            .collect::<io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let child_name = os_name(&entry.file_name());
            let child_rel = join(rel, &String::from_utf8_lossy(&child_name));
            let path = entry.path();
            let ft = fs::symlink_metadata(&path)?;
            if ft.file_type().is_symlink() {
                let target = os_name(fs::read_link(&path)?.as_os_str());
                children.push(self.node(
                    child_name,
                    &child_rel,
                    NodeKind::Symlink(target),
                    label.as_deref(),
                ));
                self.sum.symlinks += 1;
            } else if ft.is_dir() {
                children.push(self.dir(&path, &child_rel, child_name, label.as_deref())?);
            } else if ft.is_file() {
                let kind = NodeKind::File {
                    source: path.clone(),
                    size: ft.len(),
                };
                let mut n = self.node(child_name, &child_rel, kind, label.as_deref());
                if !self
                    .meta
                    .fs_config
                    .contains_key(config_path(&device_path(&self.mount, &child_rel)))
                {
                    n.mode = default_file_mode(&child_rel, &ft);
                }
                children.push(n);
                self.sum.files += 1;
            } else {
                self.sum.warnings.push(format!(
                    "{}: not a file, directory or symlink, skipped",
                    child_rel
                ));
            }
        }
        self.sum.dirs += 1;
        node.kind = NodeKind::Dir(children);
        Ok(node)
    }
}

fn join(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", rel, name)
    }
}

#[cfg(unix)]
fn os_name(s: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_name(s: &std::ffi::OsStr) -> Vec<u8> {
    s.to_string_lossy().replace('\\', "/").into_bytes()
}

/// Android-like defaults for entries without fs_config metadata.
fn default_meta(rel: &str, kind: &NodeKind) -> (u32, u32, u32, Option<u64>) {
    match kind {
        NodeKind::Dir(_) if rel == "lost+found" => (0, 0, 0o700, None),
        NodeKind::Dir(_) => (0, 0, 0o755, None),
        NodeKind::Symlink(_) => (0, 0, 0o777, None),
        NodeKind::File { .. } if in_bin_dir(rel) => (0, 2000, 0o755, None),
        NodeKind::File { .. } => (0, 0, 0o644, None),
    }
}

/// True for files directly in a `bin` or `xbin` directory.
fn in_bin_dir(rel: &str) -> bool {
    let parent = rel.rsplit_once('/').map_or("", |(p, _)| p);
    let dir = parent.rsplit('/').next().unwrap_or(parent);
    dir == "bin" || dir == "xbin"
}

/// New files: executable on disk (or in a bin directory) -> 0755, else 0644.
fn default_file_mode(rel: &str, meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return 0o755;
        }
    }
    let _ = meta;
    if in_bin_dir(rel) {
        0o755
    } else {
        0o644
    }
}

/// Inserts a symlink from `_symlinks` into the tree unless its path is
/// already taken. Returns false when its directory doesn't exist.
fn insert_symlink(root: &mut Node, rel: &str, node: Node) -> bool {
    let mut dir = root;
    let parts: Vec<&str> = rel.split('/').collect();
    for part in &parts[..parts.len() - 1] {
        let NodeKind::Dir(children) = &mut dir.kind else {
            return false;
        };
        match children
            .iter_mut()
            .find(|c| c.name == part.as_bytes() && matches!(c.kind, NodeKind::Dir(_)))
        {
            Some(c) => dir = c,
            None => return false,
        }
    }
    let NodeKind::Dir(children) = &mut dir.kind else {
        return false;
    };
    if !children.iter().any(|c| c.name == node.name) {
        children.push(node);
    }
    true
}

fn parse_hex16(s: Option<&String>) -> Option<[u8; 16]> {
    let hex: String = s?.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Stand-in for a random UUID when `_info` has none: derived from the
/// partition name and the current time.
fn made_up_uuid(part: &str) -> [u8; 16] {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h: u128 = 0x6a09_e667_f3bc_c908_bb67_ae85_84ca_a73b ^ now;
    for b in part.bytes() {
        h = h.rotate_left(13) ^ b as u128;
        h = h.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013B);
    }
    let mut out = h.to_le_bytes();
    out[6] = (out[6] & 0x0F) | 0x40; // version 4
    out[8] = (out[8] & 0x3F) | 0x80; // variant
    out
}

/// Builds `<work>/<part>` into `output`.
pub fn build(
    work: &Path,
    part: &str,
    output: &Path,
    size: Size,
    mut log: impl FnMut(&str),
) -> io::Result<Summary> {
    let meta = load_meta(&work.join("config"), part)?;
    let info = &meta.info;
    let mount = info
        .get("mount_point")
        .cloned()
        .unwrap_or_else(|| mount_point(info.get("volume_name").map_or("", |s| s), part));
    let root_dir = work.join(part);
    if !root_dir.is_dir() {
        return Err(invalid(format!(
            "{} is not a directory",
            root_dir.display()
        )));
    }
    log(&format!(
        "- Building {} (mount point {}) from {}",
        part,
        mount,
        root_dir.display()
    ));

    let mut w = Walker {
        meta: &meta,
        mount: mount.clone(),
        seen: HashSet::new(),
        sum: Summary {
            mount_point: mount.clone(),
            ..Default::default()
        },
    };
    let mut root = w.dir(&root_dir, "", Vec::new(), None)?;
    let root_label = root.selinux.clone();

    // symlinks the folder couldn't hold (Windows, /sdcard) or that were extracted as such
    for (cfg, target) in &meta.symlinks {
        if w.seen.contains(cfg) {
            continue;
        }
        let device = if cfg == "/" {
            "/".to_string()
        } else {
            format!("/{}", cfg)
        };
        let rel = match mount.as_str() {
            "/" => device.trim_start_matches('/').to_string(),
            m => match device.strip_prefix(m).and_then(|r| r.strip_prefix('/')) {
                Some(r) => r.to_string(),
                None => continue,
            },
        };
        let name = rel.rsplit('/').next().unwrap_or(&rel).as_bytes().to_vec();
        let parent_rel = rel.rsplit_once('/').map_or("", |(p, _)| p);
        let parent_label = meta.contexts.get(&device_path(&mount, parent_rel)).cloned();
        let node = w.node(
            name,
            &rel,
            NodeKind::Symlink(target.clone()),
            parent_label.as_deref(),
        );
        if insert_symlink(&mut root, &rel, node) {
            w.sum.symlinks += 1;
        } else {
            w.sum
                .warnings
                .push(format!("symlink {}: its directory is gone, skipped", rel));
        }
    }

    // lost+found: recorded by extract but not extracted
    let NodeKind::Dir(children) = &mut root.kind else {
        unreachable!()
    };
    if !children.iter().any(|c| c.name == b"lost+found") {
        let lf = w.node(
            b"lost+found".to_vec(),
            "lost+found",
            NodeKind::Dir(Vec::new()),
            root_label.as_deref(),
        );
        w.sum.new_entries.retain(|e| !e.ends_with("lost+found"));
        children.push(lf);
    }

    w.sum.removed = meta
        .fs_config
        .keys()
        .filter(|k| !w.seen.contains(*k))
        .count();
    let mut sum = w.sum;

    // image parameters
    let num = |k: &str| info.get(k).and_then(|v| v.parse::<u64>().ok());
    let bs = num("block_size").unwrap_or(4096);
    let (data, used_inodes) = mkext4::data_blocks_needed(&root, bs)?;
    let auto_inodes = (used_inodes as u64 + used_inodes as u64 / 50 + 64) as u32;
    let (blocks, inodes) = match (size, num("blocks")) {
        (Size::Bytes(b), _) => (
            b / bs,
            num("inodes")
                .map_or(auto_inodes, |i| i as u32)
                .max(auto_inodes),
        ),
        (Size::Original, Some(b)) => (
            b,
            num("inodes")
                .map_or(auto_inodes, |i| i as u32)
                .max(auto_inodes),
        ),
        (Size::Auto, _) | (Size::Original, None) => {
            // content + 2% for extent leaves and growth
            let with_margin = data + data / 50 + 256;
            (
                mkext4::blocks_for(with_margin, auto_inodes, bs)?,
                auto_inodes,
            )
        }
    };
    let params = Params {
        block_size: bs,
        blocks,
        inodes,
        reserved_blocks: num("reserved_blocks").unwrap_or(0),
        uuid: parse_hex16(info.get("uuid")).unwrap_or_else(|| made_up_uuid(part)),
        hash_seed: parse_hex16(info.get("hash_seed"))
            .unwrap_or_else(|| made_up_uuid(&format!("{}-seed", part))),
        volume_name: info
            .get("volume_name")
            .cloned()
            .unwrap_or_else(|| part.to_string()),
        last_mounted: mount.clone(),
        timestamp: num("created").map_or(DEFAULT_TIMESTAMP, |t| t as u32),
    };
    log(&format!(
        "- Image: {} blocks of {} bytes ({} MiB), {} blocks of content",
        blocks,
        bs,
        (blocks * bs) >> 20,
        data
    ));
    sum.block_size = bs;
    sum.stats = mkext4::write_image(output, &root, &params).inspect_err(|_| {
        let _ = fs::remove_file(output);
    })?;
    Ok(sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!("auto".parse(), Ok(Size::Auto));
        assert_eq!("4096".parse(), Ok(Size::Bytes(4096)));
        assert_eq!("2G".parse(), Ok(Size::Bytes(2 << 30)));
        assert_eq!("512m".parse(), Ok(Size::Bytes(512 << 20)));
        assert!("12X".parse::<Size>().is_err());
    }

    #[test]
    fn bin_dirs() {
        assert!(in_bin_dir("bin/sh"));
        assert!(in_bin_dir("system/bin/sh"));
        assert!(in_bin_dir("system/xbin/su"));
        assert!(!in_bin_dir("system/lib/libc.so"));
        assert!(!in_bin_dir("cabin/x"));
    }

    #[test]
    fn hex16() {
        let u = parse_hex16(Some(&"30dc3fef-c49c-53e2-9bae-38d5480ae009".to_string())).unwrap();
        assert_eq!(u[0], 0x30);
        assert_eq!(u[15], 0x09);
        assert!(parse_hex16(Some(&"xyz".to_string())).is_none());
    }
}
