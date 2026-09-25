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

A/B OTA zips (`payload.bin` + `payload_properties.txt`). Unpack is refused for now.

Do compressed EROFS (lz4/lz4hc) first: most payload ROMs (Xiaomi, OnePlus, ...) ship lz4 EROFS, so without it the dumped images can't be extracted.

### Unpack

- Read `payload.bin` straight from the zip (it is stored) with `factory::stored_range` + `fs::Window`.
- Parse the header (`CrAU`, version 2, manifest size, metadata signature size) and the `DeltaArchiveManifest` protobuf with a small hand-written decoder (no prost/protoc).
- Full OTAs only: REPLACE, REPLACE_XZ, REPLACE_BZ, ZSTD, ZERO, DISCARD. Check `data_sha256_hash` per operation and `new_partition_info.hash` per partition (SHA-256). Operations are independent: decode them in parallel with `std::thread::scope`.
- Incremental OTAs (SOURCE_COPY, SOURCE_BSDIFF, BROTLI_BSDIFF, PUFFDIFF, ZUCCHINI, LZ4DIFF_*) need the old images: refuse them.
- ext4/EROFS partitions go through `extract.rs`. The rest (boot, vendor_boot, vbmeta, firmware, ...) go to `rom/` as images.
- Pure-Rust decoders that cross-compile for Android: xz (`lzma-rs` or our own), bzip2 (`bzip2` with the Rust backend), zstd (`ruzstd`), and `sha2`.

### Repack, in this order

1. **Fastboot ROM** (no signing, most code exists): rebuilt images + untouched images + a `super_empty.img` made from the manifest's `dynamic_partition_metadata` (groups, max sizes, partitions) + `flash-all.sh`/`.bat`. Reuses `mkerofs`, `factory::check_groups` and the vbmeta patch.
2. **super.img** (see below): the same images in one lpmake-like `super.img`.
3. **New payload.bin** (flashable in recovery):
   - Manifest via a hand-written protobuf encoder: `block_size`, `minor_version = 0` (full), `dynamic_partition_metadata`, and per partition `new_partition_info` (size + SHA-256) and its operations.
   - Rebuilt partitions: 2 MiB chunks as REPLACE_XZ, falling back to REPLACE when xz doesn't shrink the chunk (as `full_update_generator.cc` does). ZSTD is faster but only newer `update_engine` versions read it. A pure-Rust xz encoder compresses worse; `liblzma` (C) is better but must cross-compile.
   - Untouched partitions: copy their operations and data blobs from the old payload as they are (no recompression).
   - `vbmeta.img` in the payload: set the disable-verity/verification flags (as for fastboot ROMs).
   - `payload_properties.txt` (`FILE_HASH`, `FILE_SIZE`, `METADATA_HASH`, `METADATA_SIZE`, base64 SHA-256) and `META-INF/com/android/metadata` + `metadata.pb`.
   - Signing: RSA-2048 PKCS#1 v1.5 + SHA-256 over the metadata and the whole payload, plus the zip signature, with the AOSP test keys (own code or the `rsa` crate). Stock recoveries reject it (OEM keys). Custom recoveries (TWRP, OrangeFox, LineageOS recovery) take test keys, skip the check, or ask "install anyway".
   - Test: AOSP `scripts/update_payload/checker.py` must accept our payload. Compare with `delta_generator` from `otatools.zip` (Linux x86_64 only, so as a reference, not a dependency).

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
- No `shared_blocks` dedup, no hard links (see below).

## ext4 extractor: open points

- Android sparse images (`simg`) are rejected; only raw images are read. Could read them through the sparse reader in img2sdat.
- Hard links are extracted as separate copies and not recorded.
- Paths with whitespace can't be written to fs_config / file_contexts; they only get a warning.
- `shared_blocks` (Android 10+ deduplicated ext4) is untested: no tool here creates such images.
