//! Xiaomi fastboot ROMs (`<device>_images_<version>.tgz`): `flash_all.sh`
//! first flashes `images/crclist.txt` and `images/sparsecrclist.txt`, and
//! the bootloader then refuses images whose CRC32 doesn't match. A
//! repacked ROM needs new lists; they are made as the ROM's own
//! `flash_gen_crc_list.py` does:
//!
//! ```text
//! crclist.txt:        CRC-LIST / <partition> 0x<crc32 of the file>
//! sparsecrclist.txt:  SPARSECRC-LIST / <partition> <n> <crc32 of each part>...
//! ```
//!
//! A sparse image is flashed in parts of at most 768 MiB (fastboot's
//! max-download-size); the script works out the same split and CRCs each
//! part's decoded data.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use crate::fs::invalid;

pub const CRCLIST: &str = "crclist.txt";
pub const SPARSECRCLIST: &str = "sparsecrclist.txt";
pub const GEN_SCRIPT: &str = "flash_gen_crc_list.py";

const DEFAULT_MAX_DOWNLOAD: i64 = 768 * 1024 * 1024;
const MAX_SPARSE_PARTS: usize = 20;
const IMAGE_HEAD: i64 = 28;
const CHUNK_HEAD: i64 = 12;
const OVERHEAD: i64 = IMAGE_HEAD + 2 * CHUNK_HEAD + 4;

/// Partition -> image file name, from the `"ptn": "file"` pairs of
/// `flash_gen_crc_list.py`, and its MAX_DOWNLOAD_SIZE.
pub fn script_mapping(script: &str) -> (BTreeMap<String, String>, i64) {
    let mut map = BTreeMap::new();
    let mut max = DEFAULT_MAX_DOWNLOAD;
    for line in script.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("MAX_DOWNLOAD_SIZE") {
            // "= 768 * 1024 * 1024;"
            let expr = v.trim_start_matches([' ', '=']).trim_end_matches(';');
            let product: Option<i64> = expr
                .split('*')
                .map(|f| f.trim().parse::<i64>().ok())
                .product();
            if let Some(p) = product.filter(|&p| p > 0) {
                max = p;
            }
            continue;
        }
        let q: Vec<&str> = t.split('"').collect();
        // "ptn": "file",
        if q.len() >= 5 && q[0].is_empty() && q[2].trim() == ":" {
            map.insert(q[1].to_string(), q[3].to_string());
        }
    }
    (map, max)
}

/// CRC32 of a whole file (`binascii.crc32`).
pub fn file_crc(path: &Path) -> io::Result<u32> {
    let mut crc = flate2::Crc::new();
    crate::sign::hash_reader(&mut File::open(path)?, |b| crc.update(b))?;
    Ok(crc.sum())
}

#[derive(Clone, Copy)]
struct Chunk {
    kind: u16,
    blocks: i64,
    total: i64,
    data_len: i64,
    /// Where this piece's bytes start in the chunk's data.
    data_off: usize,
}

/// The script's `split_sparse_chunk`: fits `c` into the current part or
/// splits it. Returns (split, part step, the rest when split).
fn split_chunk(
    backed: &mut i64,
    c: &mut Chunk,
    split_max: i64,
    bs: i64,
) -> (bool, u8, Option<Chunk>) {
    let mut backed_real = *backed - CHUNK_HEAD - 4;
    if backed_real + c.data_len <= split_max {
        *backed += c.total;
        return (false, 0, None);
    }
    let over = if backed_real >= split_max * 7 / 8 {
        if c.total <= split_max {
            *backed = c.total + IMAGE_HEAD + CHUNK_HEAD;
            return (false, 1, None);
        }
        backed_real = 0;
        true
    } else {
        false
    };
    let split_sz = bs * ((split_max - backed_real) / bs);
    let rest_len = c.data_len - split_sz;
    let rest = Chunk {
        kind: c.kind,
        blocks: (rest_len + CHUNK_HEAD) / bs,
        total: rest_len + CHUNK_HEAD,
        data_len: rest_len,
        data_off: c.data_off + split_sz.max(0) as usize,
    };
    c.data_len = split_sz;
    c.total = split_sz + CHUNK_HEAD;
    c.blocks = c.total / bs;
    if over {
        *backed = c.total;
        (true, 1, Some(rest))
    } else {
        *backed = CHUNK_HEAD + IMAGE_HEAD;
        (true, 2, Some(rest))
    }
}

/// The script's `calulate_sparse_chunk_crc`.
fn chunk_crc(crc: &mut flate2::Crc, c: &Chunk, data: &[u8], bs: i64, fill: &mut Vec<u8>) {
    match c.kind {
        0xCAC1 => {
            let end = (c.data_off + c.data_len.max(0) as usize).min(data.len());
            crc.update(&data[c.data_off.min(end)..end]);
        }
        0xCAC2 if data.len() >= 4 => {
            // the 4-byte pattern repeated to a block, once per block
            fill.clear();
            while (fill.len() as i64) < bs {
                fill.extend_from_slice(&data[..4]);
            }
            for _ in 0..c.blocks {
                crc.update(fill);
            }
        }
        _ => {}
    }
}

/// CRC32s of the parts fastboot splits a sparse image into (the ROM
/// script's `gen_sparse_crc`, with its integer arithmetic).
pub fn sparse_crcs(path: &Path, max_download: i64) -> io::Result<Vec<u32>> {
    let mut r = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut h = [0u8; 28];
    r.read_exact(&mut h)?;
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    if u32_at(&h, 0) != crate::sparse::MAGIC || u16_at(&h, 8) != 28 || u16_at(&h, 10) != 12 {
        return Err(invalid(format!("{}: not a sparse image", path.display())));
    }
    let bs = u32_at(&h, 12) as i64;
    let total_chunks = u32_at(&h, 20);
    let split_max = max_download - OVERHEAD;
    let mut crcs: Vec<flate2::Crc> = (0..MAX_SPARSE_PARTS).map(|_| flate2::Crc::new()).collect();
    let mut parts = 0usize;
    let mut backed = IMAGE_HEAD;
    let mut fill = Vec::new();
    let mut data = Vec::new();
    for _ in 0..total_chunks {
        let mut c = [0u8; 12];
        r.read_exact(&mut c)?;
        let total = u32_at(&c, 8) as i64;
        let mut chunk = Chunk {
            kind: u16_at(&c, 0),
            blocks: u32_at(&c, 4) as i64,
            total,
            data_len: total - CHUNK_HEAD,
            data_off: 0,
        };
        data.resize(chunk.data_len.max(0) as usize, 0);
        r.read_exact(&mut data)?;
        loop {
            let (split, step, rest) = split_chunk(&mut backed, &mut chunk, split_max, bs);
            if step == 1 {
                parts += 1;
            }
            let crc = crcs
                .get_mut(parts)
                .ok_or_else(|| invalid("sparse image splits into too many parts"))?;
            chunk_crc(crc, &chunk, &data, bs, &mut fill);
            if step == 2 {
                parts += 1;
            }
            match rest {
                Some(r) if split => chunk = r,
                _ => break,
            }
        }
    }
    if parts >= MAX_SPARSE_PARTS {
        return Err(invalid("sparse image splits into too many parts"));
    }
    Ok(crcs[..=parts].iter().map(|c| c.sum()).collect())
}

/// Rewrites `crclist.txt` and `sparsecrclist.txt` in `lists` for the
/// partitions whose image file changed: `changed` holds (file name in the
/// ROM's images folder, where the new file is). Other lines stay.
pub fn update_crc_lists(
    lists: &Path,
    script: Option<&str>,
    changed: &[(String, std::path::PathBuf)],
) -> io::Result<Vec<String>> {
    let (map, max) = script
        .map(script_mapping)
        .unwrap_or((BTreeMap::new(), DEFAULT_MAX_DOWNLOAD));
    // partition of a file: from the script, else "<file stem>" / "<stem>_ab"
    let file_of = |ptn: &str| -> String {
        map.get(ptn)
            .cloned()
            .unwrap_or_else(|| format!("{}.img", ptn.strip_suffix("_ab").unwrap_or(ptn)))
    };
    let mut updated = Vec::new();
    for (list, sparse) in [(CRCLIST, false), (SPARSECRCLIST, true)] {
        let path = lists.join(list);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut out = String::new();
        for line in text.lines() {
            let ptn = line.split_whitespace().next().unwrap_or("");
            let file = file_of(ptn);
            let is_entry = !ptn.is_empty() && !line.ends_with("-LIST");
            let Some((_, img)) = changed
                .iter()
                .find(|(f, _)| *f == file)
                .filter(|_| is_entry)
            else {
                out.push_str(line);
                out.push('\n');
                continue;
            };
            if sparse {
                let crcs = sparse_crcs(img, max)?;
                out.push_str(&format!("{} {}", ptn, crcs.len()));
                for c in crcs {
                    out.push_str(&format!(" {:#x}", c));
                }
                out.push('\n');
            } else {
                out.push_str(&format!("{} {:#x}\n", ptn, file_crc(img)?));
            }
            updated.push(format!("{} {}", list, ptn));
        }
        std::fs::write(&path, out)?;
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_from_script() {
        let script = "sparse_file_list = {\n    \"super\": \"super.img\",\n}\n\
                      unsparse_file_list = {\n    \"vbmeta_ab\": \"vbmeta.img\",\n    \"modem_ab\": \"NON-HLOS.bin\"\n}\n\
                      MAX_DOWNLOAD_SIZE = 512 * 1024 * 1024;\n";
        let (map, max) = script_mapping(script);
        assert_eq!(map["modem_ab"], "NON-HLOS.bin");
        assert_eq!(map["super"], "super.img");
        assert_eq!(max, 512 << 20);
    }
}

#[cfg(test)]
mod rom_check {
    /// JANCOX_XIAOMI_IMAGES=<dir with super.img and sparsecrclist.txt>
    #[test]
    #[ignore]
    fn original_lists() {
        let dir = std::path::PathBuf::from(std::env::var("JANCOX_XIAOMI_IMAGES").unwrap());
        let text = std::fs::read_to_string(dir.join("sparsecrclist.txt")).unwrap();
        let line = text.lines().find(|l| l.starts_with("super ")).unwrap();
        let crcs = super::sparse_crcs(&dir.join("super.img"), super::DEFAULT_MAX_DOWNLOAD).unwrap();
        let mine: Vec<String> = crcs.iter().map(|c| format!("{:#x}", c)).collect();
        assert_eq!(format!("super {} {}", crcs.len(), mine.join(" ")), line);
        let text = std::fs::read_to_string(dir.join("crclist.txt")).unwrap();
        let line = text.lines().find(|l| l.starts_with("vbmeta_ab ")).unwrap();
        assert_eq!(
            format!(
                "vbmeta_ab {:#x}",
                super::file_crc(&dir.join("vbmeta.img")).unwrap()
            ),
            line
        );
    }
}
