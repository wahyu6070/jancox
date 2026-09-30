//! The parts of an A/B OTA zip around payload.bin, and fastboot scripts for
//! the same images.
//!
//! `META-INF/com/android/metadata` (key=value) and `metadata.pb`
//! (`OtaMetadata`) list `ota-property-files`: `name:offset:size` of stored
//! zip entries, so an updater can stream payload.bin out of the zip.
//! Recovery doesn't read them, but a new zip moves every entry, so they are
//! rewritten from the offsets the entries will have.

use std::io::{self, Cursor, Write};

use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

use crate::fs::invalid;
use crate::proto::{self, Writer};

pub const METADATA: &str = "META-INF/com/android/metadata";
pub const METADATA_PB: &str = "META-INF/com/android/metadata.pb";
pub const OTACERT: &str = "META-INF/com/android/otacert";
pub const PROPERTIES: &str = "payload_properties.txt";
/// Listed in the property files: the head of payload.bin up to the blobs.
pub const PAYLOAD_METADATA: &str = "payload_metadata.bin";
/// Property-file keys in the metadata.
const PROPERTY_KEYS: &[&str] = &["ota-property-files", "ota-streaming-property-files"];

/// Bytes of the local header the zip writer puts before an entry's data.
pub fn local_header_len(name: &str, options: SimpleFileOptions) -> io::Result<u64> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file(name, options).map_err(io::Error::from)?;
    let buf = zip.finish().map_err(io::Error::from)?.into_inner();
    let mut archive = ZipArchive::new(Cursor::new(buf)).map_err(io::Error::from)?;
    let entry = archive.by_index(0).map_err(io::Error::from)?;
    entry
        .data_start()
        .ok_or_else(|| invalid("zip: no data offset"))
}

/// Data offsets of entries written one after another from the start of a
/// zip, as (name, stored size, options).
pub fn data_offsets(entries: &[(String, u64, SimpleFileOptions)]) -> io::Result<Vec<u64>> {
    let mut pos = 0;
    let mut out = Vec::with_capacity(entries.len());
    for (name, size, options) in entries {
        pos += local_header_len(name, *options)?;
        out.push(pos);
        pos += size;
    }
    Ok(out)
}

/// A property-files value with new offsets: the entries `locate` knows keep
/// their place in the list, the others are dropped.
pub fn property_files(old: &str, locate: &impl Fn(&str) -> Option<(u64, u64)>) -> String {
    old.trim()
        .split(',')
        .filter_map(|token| {
            let name = token.split(':').next()?.trim();
            let (offset, size) = locate(name)?;
            Some(format!("{}:{}:{}", name, offset, size))
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// `META-INF/com/android/metadata` with new property files.
pub fn update_metadata(text: &str, locate: &impl Fn(&str) -> Option<(u64, u64)>) -> String {
    let mut out = String::new();
    for line in text.lines() {
        match line.split_once('=') {
            Some((k, v)) if PROPERTY_KEYS.contains(&k) => {
                out.push_str(&format!("{}={}", k, property_files(v, locate)));
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// `metadata.pb` (OtaMetadata; `property_files` is map field 4) with new
/// property files.
pub fn update_metadata_pb(
    pb: &[u8],
    locate: &impl Fn(&str) -> Option<(u64, u64)>,
) -> io::Result<Vec<u8>> {
    let mut w = Writer::new();
    for (f, v) in proto::fields(pb)? {
        if f == 4 {
            let mut key = String::new();
            let mut value = String::new();
            for (ef, ev) in proto::fields(v.as_bytes()?)? {
                match ef {
                    1 => key = ev.as_string()?,
                    2 => value = ev.as_string()?,
                    _ => {}
                }
            }
            if PROPERTY_KEYS.contains(&key.as_str()) {
                let mut entry = Writer::new();
                entry
                    .string(1, &key)
                    .string(2, &property_files(&value, locate));
                w.message(4, &entry);
                continue;
            }
        }
        w.value(f, &v);
    }
    Ok(w.buf)
}

/// `flash-all.sh` and `flash-all.bat` for a fastboot ROM made from a
/// payload: `firmware` images are flashed in the bootloader, `logical`
/// partitions (in super) in fastbootd.
pub fn flash_scripts(firmware: &[String], logical: &[String], snapshot: bool) -> (String, String) {
    let mut sh = String::from(
        "#!/bin/sh\n\
         # Flashes this ROM with fastboot (made by Jancox from a payload.bin OTA).\n\
         # The bootloader must be unlocked. vbmeta.img has dm-verity and AVB\n\
         # verification disabled. If the device doesn't boot, wipe data:\n\
         #   fastboot -w\n\
         # Set FASTBOOT=/path/to/fastboot to use another fastboot.\n\
         cd \"$(dirname \"$0\")\" || exit 1\n\
         fastboot=\"${FASTBOOT:-fastboot}\"\n\
         set -e\n",
    );
    let mut bat = String::from(
        "@echo off\r\n\
         rem Flashes this ROM with fastboot (made by Jancox from a payload.bin OTA).\r\n\
         rem The bootloader must be unlocked. vbmeta.img has dm-verity and AVB\r\n\
         rem verification disabled. If the device doesn't boot, wipe data:\r\n\
         rem   fastboot -w\r\n\
         cd /d \"%~dp0\"\r\n",
    );
    // a failing optional step doesn't stop the script
    let mut step = |cmd: &str, optional: bool| {
        if optional {
            sh.push_str(&format!("\"$fastboot\" {} || true\n", cmd));
            bat.push_str(&format!("fastboot {}\r\n", cmd));
        } else {
            sh.push_str(&format!("\"$fastboot\" {}\n", cmd));
            bat.push_str(&format!("fastboot {} || goto :error\r\n", cmd));
        }
    };
    for p in firmware {
        step(&format!("flash {} {}.img", p, p), false);
    }
    if !logical.is_empty() {
        // logical partitions only exist in fastbootd (userspace fastboot)
        step("reboot fastboot", false);
        if snapshot {
            // a pending Virtual A/B merge blocks flashing
            step("snapshot-update cancel", true);
        }
        for p in logical {
            step(&format!("flash {} {}.img", p, p), false);
        }
    }
    step("reboot", false);
    sh.push_str("echo \"Done.\"\n");
    bat.push_str(
        "echo Done.\r\n\
         exit /b 0\r\n\
         :error\r\n\
         echo Flashing failed.\r\n\
         exit /b 1\r\n",
    );
    (sh, bat)
}

/// Writes `data` as a stored entry, checking the offset it was planned at.
pub fn write_stored<W: Write + io::Seek>(
    zip: &mut ZipWriter<W>,
    name: &str,
    options: SimpleFileOptions,
    data: &[u8],
) -> io::Result<()> {
    zip.start_file(name, options).map_err(io::Error::from)?;
    zip.write_all(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::CompressionMethod;

    #[test]
    fn property_files_follow_offsets() {
        let locate = |n: &str| match n {
            "payload.bin" => Some((100, 2000)),
            "metadata" => Some((10, 50)),
            _ => None,
        };
        let text = "ota-property-files=payload.bin:3687:3859419014,care_map.pb:2951:689,metadata:69:695   \n\
                    ota-type=AB\n";
        assert_eq!(
            update_metadata(text, &locate),
            "ota-property-files=payload.bin:100:2000,metadata:10:50\nota-type=AB\n"
        );
        let mut entry = Writer::new();
        entry
            .string(1, "ota-property-files")
            .string(2, "metadata:1:2");
        let mut other = Writer::new();
        other.string(1, "x").string(2, "y");
        let mut pb = Writer::new();
        pb.varint(1, 1).message(4, &entry).message(4, &other);
        let new = update_metadata_pb(&pb.buf, &locate).unwrap();
        let f = proto::fields(&new).unwrap();
        assert_eq!(f.len(), 3);
        let e = proto::fields(f[1].1.as_bytes().unwrap()).unwrap();
        assert_eq!(e[1].1.as_string().unwrap(), "metadata:10:50");
        assert_eq!(f[2].1.as_bytes().unwrap(), &other.buf[..]);
    }

    #[test]
    fn planned_offsets_match_the_zip() {
        let stored = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .unix_permissions(0o644);
        let entries = vec![
            ("META-INF/com/android/metadata".to_string(), 5, stored),
            ("payload.bin".to_string(), 3, stored.large_file(true)),
            ("x".to_string(), 0, stored),
        ];
        let offsets = data_offsets(&entries).unwrap();
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, size, options) in &entries {
            write_stored(&mut zip, name, *options, &vec![7u8; *size as usize]).unwrap();
        }
        let buf = zip.finish().unwrap().into_inner();
        let mut archive = ZipArchive::new(Cursor::new(buf)).unwrap();
        for (i, off) in offsets.iter().enumerate() {
            assert_eq!(archive.by_index(i).unwrap().data_start(), Some(*off));
        }
    }

    #[test]
    fn scripts() {
        let (sh, bat) = flash_scripts(&["boot".into()], &["system".into()], true);
        assert!(sh.contains("\"$fastboot\" flash boot boot.img\n\"$fastboot\" reboot fastboot\n"));
        assert!(
            sh.contains("snapshot-update cancel || true\n\"$fastboot\" flash system system.img\n")
        );
        assert!(bat.contains("fastboot flash system system.img || goto :error\r\n"));
    }
}
