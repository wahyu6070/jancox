# TODO

## Installing

- `install.sh` covers Linux and Termux (plain binaries in the release + SHA256SUMS). Still open: a PowerShell one-liner for Windows, and maybe a Termux package / `.deb`.

## EROFS

Uncompressed EROFS is read (`fs/erofs.rs`) and written (`fs/mkerofs.rs`), and was checked against a Pixel factory image (cubs, CD1A.260905.001.B1). The images were compared with `fsck.erofs --extract` 1.9.4 and loop-mounted. erofs-utils 1.7.1 (Ubuntu 24.04) truncates chunk-based files, so don't use it as a reference. 1.9.4 builds without autotools: compile `lib/*.c` + `{fsck,mkfs,dump}/main.c` with a hand-written `config.h`.

- Compressed files (datalayout 1 and 3) are refused. Most non-Pixel EROFS ROMs (Xiaomi, OnePlus, ...) are lz4/lz4hc compressed. To add:
  - lz4 / lz4hc first (the Android default), then lzma (MicroLZMA), deflate and zstd
  - full and compact indexes, big pclusters, ztailpacking, fragments (packed inode), dedupe
  - long xattr prefixes stored in the packed inode
- Writer: no chunk-based dedupe or holes. Pixel images share identical blocks and leave zero blocks as holes; ours are up to about 6% bigger (vendor 1.21 GB vs 1.14 GB). Pixel partitions fit the super group easily, but on a recovery ROM with EROFS and fixed-size partitions (no `dynamic_partitions_op_list`), even an unchanged rebuild can exceed the partition and fail with "remove some files". Holes and chunk dedupe fix that.
- Writer: no compression (option `erofs.compress=lz4hc` in `jancox.prop`?).
- Writer: no xattr bloom filter (`xattr_filter`) and no `mtime` feature bit. Every inode gets the build time.

## payload.bin ROMs

A/B OTA zips (`payload.bin` + `payload_properties.txt`). Unpack and repack work (`payload.rs`, `sign.rs`, `ota.rs`, `rom.rs`), checked with the ASUS ROG Phone 5 full OTA: all 29 SHA-256 match on unpack; the repacked payload passes AOSP `paycheck.py --check` with the test key, the zip signature verifies (`openssl cms`), and unpacking it again gives the same trees and images. Not flashed on a device yet.

Most payload ROMs from Xiaomi, OnePlus and others ship lz4 EROFS. Until compressed EROFS works, those unpack only as far as the images: extraction fails.

### Open points

- Incremental OTAs (SOURCE_COPY, SOURCE_BSDIFF, BROTLI_BSDIFF, PUFFDIFF, ZUCCHINI, LZ4DIFF_*) are refused; they need the old images.
- A bare `payload.bin` works with `jancox payload`, but `unpack` only takes zips.
- The logical partitions are dumped to `tmp/` before extraction, which needs their size in free space (system is 3.4 GB on the ROG Phone 5). Reading them in place would need a reader over the operations.
- Only an ASUS payload was tested (REPLACE, REPLACE_XZ, REPLACE_BZ). ZSTD is covered by the decoder but untested on a real ROM.
- Repack rebuilds every logical partition even when unchanged (like fastboot ROMs); an untouched partition could keep its old operations and hashtree.
- Repack writes the blobs to `tmp/payload.data` first (the manifest, which comes first, needs their offsets and hashes): free space for the payload twice.
- Repack encodes with xz only. ZSTD is faster but only newer `update_engine` versions read it. AOSP also uses the ARM Thumb BCJ filter (a bit smaller).
- Virtual A/B COW estimates of rebuilt partitions are set on the safe side (`estimate_cow_size` = image size + headers), not computed.
- `care_map.pb` is left out of a repacked OTA (it lists the hashtree ranges of the old images).
- Fastboot output: the scripts flash the current slot only; a super group that is too full for fastbootd isn't handled.

### References

AOSP (Apache-2.0), easiest to browse on cs.android.com:

- [platform/system/update_engine](https://android.googlesource.com/platform/system/update_engine/):
  - `update_metadata.proto`: the manifest format.
  - `payload_generator/`: `delta_generator`. `generate_delta_main.cc` (options), `full_update_generator.cc` (full OTA chunks), `payload_file.cc` (writes `payload.bin`), `payload_signer.cc` (hashes, signatures, `METADATA_HASH`/`FILE_HASH`), `xz_android.cc`, `extent_utils.cc`.
  - `scripts/brillo_update_payload`: the steps `generate`, `hash`, `sign`, `properties`. The clearest map of a repack.
  - `scripts/update_payload/`: Python payload reader and `checker.py`, to validate ours.
- [platform/build tools/releasetools](https://android.googlesource.com/platform/build/+/refs/heads/main/tools/releasetools/): `ota_from_target_files.py` (the A/B OTA zip: payload, `payload_properties.txt`, `META-INF/com/android/metadata(.pb)`, `care_map.pb`, zip signing), `ota_utils.py`, `ota_metadata.proto`, `payload_signer.py`. Test keys: `build/make/target/product/security/testkey.{pk8,x509.pem}`.
- [platform/bootable/recovery](https://android.googlesource.com/platform/bootable/recovery/): `install/install.cpp` (zip signature against `otacerts.zip`, then `payload_properties.txt` -> `update_engine`). Shows which checks a custom recovery can skip.

[rhythmcache/payload-dumper-rust](https://github.com/rhythmcache/payload-dumper-rust) (Apache-2.0; checked at `ac244d8`, 2026-09-05): a dumper only (no repack). Borrow its knowledge, not its stack:

- `src/payload/payload_parser.rs`: header and where the data blob starts.
- `src/payload/payload_dumper.rs`: one operation -> `dst_extents` (REPLACE, REPLACE_XZ, REPLACE_BZ, ZSTD, ZERO) with the hash check.
- `src/zip/core_parser.rs` + `src/readers/local_zip_reader.rs`: `payload.bin` read inside the zip (we have `factory::stored_range` + `fs::Window`).
- `src/payload/diff.rs`: incremental ops, only if they ever matter.
- Don't take its dependencies (tokio/async, prost + `build.rs` codegen, reqwest, clap, indicatif). Keep the Apache-2.0 notice on anything copied almost verbatim.

## super.img

The super partition image with all logical partitions (in some fastboot ROMs, and dumped from devices).

- Read the liblp metadata (geometry, header, partitions, extents, groups; `factory::super_groups` already reads groups from `super_empty.img`). Sparse `super.img` needs the sparse reader first (see the ext4 extractor points below).
- Unpack: extract each partition of slot a (`system_a`, ...) by its extents, straight from `super.img` through `fs::Window` when it is one linear extent.
- Repack: write a new `super.img` (lpmake-like) from the rebuilt images, with the same groups, block devices and metadata size/slots, as raw or sparse.

## Repack

- AVB / dm-verity: a rebuilt partition no longer matches its hashtree and AVB footer (the tail of the partition past the filesystem, and `vbmeta*.img`). Like the old Jancox, the result only boots with verification disabled. Add an option to patch `vbmeta.img` / `vbmeta_system.img` flags (disable verity + verification), or regenerate the hashtree.
- Fastboot ROMs: all partitions are rebuilt even when nothing changed. An unchanged partition could keep its original image, with the AVB footer and a working hashtree.
- Fastboot ROMs: the image zip is written to `tmp/` first and then copied into the ROM zip, which needs its size in free space twice. Streaming it would need a zip writer that doesn't seek.
- `NewROM-<date>.zip` uses UTC; local time needs a timezone source.
- The auto size (`build -s auto`, growing a full dynamic partition) counts file blocks before holes are removed, so it overestimates a little.

## ext4 builder: open points

- Directories are linear (no `dir_index` htree); fine for Android sizes, slower lookups in huge directories.
- No hard links (see below). `shared_blocks` dedup is done when the original image had it.

## ext4 extractor: open points

- Android sparse images (`simg`) are rejected; only raw images are read. Could read them through the sparse reader in img2sdat.
- Hard links are extracted as separate copies and not recorded.
- Paths with whitespace can't be written to fs_config / file_contexts; they only get a warning.
- `shared_blocks` (Android 10+ deduplicated ext4) images read fine (ASUS ROG Phone 5 system/vendor/product).
