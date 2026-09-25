//! Fastboot ROMs such as Pixel factory images:
//!
//! ```text
//! <device>-<build>/flash-all.sh, bootloader-*.img, radio-*.img, ...
//! <device>-<build>/image-<device>-<build>.zip   stored (not compressed), holds
//!     android-info.txt, fastboot-info.txt, super_empty.img, vbmeta*.img,
//!     boot.img, ..., system.img, vendor.img, product.img, ... (raw images)
//! ```
//!
//! `fastboot update image-*.zip` (used by flash-all.sh) flashes the inner
//! zip. Partition images are raw ext4 or EROFS followed by their AVB
//! hashtree and footer. A plain `image-*.zip` is accepted as a ROM too.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek};
use std::path::Path;

use zip::{CompressionMethod, ZipArchive};

use crate::fs::{invalid, Window};

/// The inner `image-*.zip` of a factory zip, by entry name.
pub fn find_image_zip<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    names.into_iter().find(|n| {
        let base = n.rsplit('/').next().unwrap_or(n);
        base.starts_with("image-") && base.ends_with(".zip")
    })
}

/// True for a zip that `fastboot update` flashes: android-info.txt and
/// partition images at its root.
pub fn is_image_zip<'a>(names: impl IntoIterator<Item = &'a str> + Clone) -> bool {
    names.clone().into_iter().any(|n| n == "android-info.txt")
        && names
            .into_iter()
            .any(|n| !n.contains('/') && n.ends_with(".img"))
}

/// Byte range of a stored (uncompressed) zip entry within the zip file,
/// or `None` when the entry is compressed.
pub fn stored_range<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    index: usize,
) -> io::Result<Option<(u64, u64)>> {
    let entry = zip.by_index(index).map_err(io::Error::from)?;
    if entry.compression() != CompressionMethod::Stored || entry.encrypted() {
        return Ok(None);
    }
    let start = entry
        .data_start()
        .ok_or_else(|| invalid(format!("{}: no data offset", entry.name())))?;
    Ok(Some((start, entry.size())))
}

/// Opens `len` bytes of `path` at `start` as a reader of its own.
pub fn open_window(path: &Path, start: u64, len: u64) -> io::Result<Window<BufReader<File>>> {
    let file = File::open(path)?;
    if file.metadata()?.len() < start + len {
        return Err(invalid(format!("{}: truncated zip", path.display())));
    }
    Window::new(BufReader::with_capacity(1 << 16, file), start, len)
}

const AVB_MAGIC: &[u8; 4] = b"AVB0";
/// Offset of the big-endian `flags` field in the vbmeta header.
const AVB_FLAGS_OFFSET: usize = 120;
/// AVB_VBMETA_IMAGE_FLAGS_HASHTREE_DISABLED | ..._VERIFICATION_DISABLED
const AVB_FLAGS_DISABLED: u32 = 3;

/// Sets the vbmeta flags that disable dm-verity and AVB verification in a
/// vbmeta image, like `fastboot --disable-verity --disable-verification
/// flash vbmeta`. Returns false when they were already set. The device
/// must be unlocked.
pub fn disable_verification(vbmeta: &mut [u8]) -> io::Result<bool> {
    let at = AVB_FLAGS_OFFSET;
    if vbmeta.len() < 256 || &vbmeta[..4] != AVB_MAGIC {
        return Err(invalid("not a vbmeta image (no AVB0 magic)"));
    }
    let flags = u32::from_be_bytes(vbmeta[at..at + 4].try_into().unwrap());
    if flags & AVB_FLAGS_DISABLED == AVB_FLAGS_DISABLED {
        return Ok(false);
    }
    vbmeta[at..at + 4].copy_from_slice(&(flags | AVB_FLAGS_DISABLED).to_be_bytes());
    Ok(true)
}

/// Partition name without its slot suffix (`system_a` -> `system`).
pub fn strip_slot(name: &str) -> &str {
    name.strip_suffix("_a")
        .or_else(|| name.strip_suffix("_b"))
        .unwrap_or(name)
}

/// Checks that the partitions of every group fit its maximum size.
/// `sizes` holds the image size of each partition, by name without slot.
pub fn check_groups(groups: &[Group], sizes: &BTreeMap<String, u64>) -> io::Result<()> {
    for g in groups.iter().filter(|g| g.max_size > 0) {
        let total: u64 = g
            .partitions
            .iter()
            .filter_map(|p| sizes.get(strip_slot(p)))
            .sum();
        if total > g.max_size {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!(
                    "the partitions in group {} need {} bytes, more than its {} bytes; remove some files",
                    g.name, total, g.max_size
                ),
            ));
        }
    }
    Ok(())
}

/// A partition group from `super_empty.img`: name, maximum size (0 = no
/// limit) and its partitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub max_size: u64,
    pub partitions: Vec<String>,
}

const LP_GEOMETRY_MAGIC: u32 = 0x616c_4467;
const LP_HEADER_MAGIC: u32 = 0x414c_5030;
const LP_METADATA_GEOMETRY_SIZE: usize = 4096;

fn cname(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Partition groups of a `super_empty.img` (liblp metadata: geometry, then
/// the metadata). Names carry the slot suffix on A/B devices
/// (`system_a`, `google_dynamic_partitions_a`).
pub fn super_groups(image: &[u8]) -> io::Result<Vec<Group>> {
    let bad = |what: &str| invalid(format!("super_empty.img: {}", what));
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    let u64_at = |b: &[u8], i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
    if image.len() < 64 || u32_at(image, 0) != LP_GEOMETRY_MAGIC {
        return Err(bad("no liblp geometry"));
    }
    let h = image
        .get(LP_METADATA_GEOMETRY_SIZE..)
        .ok_or_else(|| bad("no metadata"))?;
    if h.len() < 128 || u32_at(h, 0) != LP_HEADER_MAGIC {
        return Err(bad("no liblp header"));
    }
    let header_size = u32_at(h, 8) as usize;
    let tables = h.get(header_size..).ok_or_else(|| bad("truncated"))?;
    // table descriptors: offset, count, entry size (partitions, extents,
    // groups, block devices), starting at byte 80 of the header
    let desc = |i: usize| {
        let d = 80 + i * 12;
        (
            u32_at(h, d) as usize,
            u32_at(h, d + 4) as usize,
            u32_at(h, d + 8) as usize,
        )
    };
    let table = |i: usize| -> io::Result<Vec<&[u8]>> {
        let (off, n, size) = desc(i);
        (0..n)
            .map(|k| {
                tables
                    .get(off + k * size..off + (k + 1) * size)
                    .ok_or_else(|| bad("truncated table"))
            })
            .collect()
    };
    let mut groups: Vec<Group> = table(2)?
        .iter()
        .map(|g| Group {
            name: cname(&g[..36]),
            max_size: u64_at(g, 40),
            partitions: Vec::new(),
        })
        .collect();
    for p in table(0)? {
        let group = u32_at(p, 48) as usize;
        let g = groups
            .get_mut(group)
            .ok_or_else(|| bad("bad group index"))?;
        g.partitions.push(cname(&p[..36]));
    }
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_zip_names() {
        let outer = [
            "cubs-cd1a/flash-all.sh",
            "cubs-cd1a/image-cubs-cd1a.zip",
            "cubs-cd1a/radio-cubs.img",
        ];
        assert_eq!(find_image_zip(outer), Some("cubs-cd1a/image-cubs-cd1a.zip"));
        assert_eq!(find_image_zip(["system.new.dat.br"]), None);
        assert!(is_image_zip(["android-info.txt", "boot.img"]));
        assert!(!is_image_zip(["android-info.txt", "a/boot.img"]));
    }

    #[test]
    fn pixel_super_empty() {
        // super_empty.img of a Pixel factory image (cubs, CD1A.260905.001.B1)
        let groups = super_groups(include_bytes!("../testdata/pixel_super_empty.img")).unwrap();
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "default",
                "google_dynamic_partitions_a",
                "google_dynamic_partitions_b"
            ]
        );
        let a = &groups[1];
        assert!(a.max_size > 0);
        assert_eq!(
            a.partitions,
            [
                "system_a",
                "system_dlkm_a",
                "system_ext_a",
                "product_a",
                "vendor_a",
                "vendor_dlkm_a"
            ]
        );
        assert!(super_groups(&[0u8; 8192]).is_err());
    }

    #[test]
    fn vbmeta_flags() {
        let mut img = vec![0u8; 256];
        img[..4].copy_from_slice(AVB_MAGIC);
        img[123] = 0x10;
        assert!(disable_verification(&mut img).unwrap());
        assert!(!disable_verification(&mut img).unwrap());
        assert_eq!(&img[120..124], &[0, 0, 0, 0x13]);
        assert!(disable_verification(&mut [0u8; 256]).is_err());
    }

    #[test]
    fn group_limits() {
        let groups = [Group {
            name: "main_a".into(),
            max_size: 100,
            partitions: vec!["system_a".into(), "vendor_a".into()],
        }];
        let mut sizes = BTreeMap::from([("system".to_string(), 60u64), ("vendor".to_string(), 40)]);
        sizes.insert("odm".into(), 500);
        assert!(check_groups(&groups, &sizes).is_ok());
        sizes.insert("vendor".into(), 41);
        assert!(check_groups(&groups, &sizes).is_err());
        assert_eq!(strip_slot("system_b"), "system");
        assert_eq!(strip_slot("vendor_dlkm"), "vendor_dlkm");
    }
}
