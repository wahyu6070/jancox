# TODO

## Installing

- `install.sh` covers Linux and Termux (plain binaries in the release + SHA256SUMS). Still open: a PowerShell one-liner for Windows, and maybe a Termux package / `.deb`.

## EROFS

EROFS is read (`fs/erofs.rs`, compressed files in `fs/erofs/z.rs`) and written uncompressed (`fs/mkerofs.rs`). Checked against a Pixel factory image (cubs, CD1A.260905.001.B1, uncompressed) and a Xiaomi Redmi Note 13 4G OTA (sapphire, OS2.0.205.0.VNGMIXM, lz4 + 0padding): all 8 partitions extract identical to `fsck.erofs --extract` 1.9.4, and vendor matches a kernel loop mount (contents, owners, modes, labels). A test matrix of mkfs.erofs 1.9.4 images (lz4, lz4hc, lzma, deflate, zstd x big pclusters, ztailpacking, fragments, all-fragments, dedupe, legacy full indexes, small extents, long xattr prefixes in the packed inode) extracts identical to the source tree; mixed algorithms (HEAD2 via `--compress-hints`) were checked with mkfs 1.7.1 because 1.9.4 segfaults on compress hints. erofs-utils 1.7.1 (Ubuntu 24.04) truncates chunk-based files, so don't use it as a reference. 1.9.4 builds with `./autogen.sh && ./configure --enable-lzma --with-libzstd --disable-fuse` (packages: autoconf automake libtool liblz4-dev liblzma-dev libzstd-dev uuid-dev zlib1g-dev).

- Reader: the new extent-record format (`Z_EROFS_ADVISE_EXTENTS`) and `48bit` / `metabox` images are untested (no mkfs here makes the first by default; the others are refused).
- Writer: no chunk-based dedupe or holes. Pixel images share identical blocks and leave zero blocks as holes; ours are up to about 6% bigger (vendor 1.21 GB vs 1.14 GB). Pixel partitions fit the super group easily, but on a recovery ROM with EROFS and fixed-size partitions (no `dynamic_partitions_op_list`), even an unchanged rebuild can exceed the partition and fail with "remove some files". Holes and chunk dedupe fix that.
- Writer: lz4 only (every compressed original is rebuilt with lz4), one-block pclusters, full indexes, no ztailpacking/fragments/dedupe. Enough for the Redmi Note 13 4G (repacked images within 1% of Xiaomi's lz4hc ones); big pclusters or lz4hc-style optimal parsing would make them smaller.
- Writer: no xattr bloom filter (`xattr_filter`) and no `mtime` feature bit. Every inode gets the build time.

## payload.bin ROMs

A/B OTA zips (`payload.bin` + `payload_properties.txt`). Unpack and repack work (`payload.rs`, `sign.rs`, `ota.rs`, `rom.rs`), checked with the ASUS ROG Phone 5 full OTA: all 29 SHA-256 match on unpack; the repacked payload passes AOSP `paycheck.py --check` with the test key, the zip signature verifies (`openssl cms`), and unpacking it again gives the same trees and images. The re-encoded xz blobs also decode with liblzma (Python `lzma`) to the right SHA-256, so they don't only work with lzma-rust2. Not flashed on a device yet.

Most payload ROMs from Xiaomi, OnePlus and others ship lz4 EROFS: they unpack and repack (Redmi Note 13 4G checked).

### Open points

- Incremental OTAs (SOURCE_COPY, SOURCE_BSDIFF, BROTLI_BSDIFF, PUFFDIFF, ZUCCHINI, LZ4DIFF_*) are refused; they need the old images.
- A bare `payload.bin` unpacks, but its repacked OTA zip has no `META-INF/com/android/metadata` (the original zip's is needed).
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

Done: `lp.rs`, `sparse.rs`, `superrom.rs`, `jancox super`. Checked with the POCO F4 (munch) Xiaomi fastboot ROM: unpack matches the same version's payload OTA, and the repacked super.img passes `simg2img`, an independent liblp parse, `e2fsck` and a round trip; the CRC lists match Xiaomi's own script.

- Not flashed on a device yet.
- Split super images (`super.img.0`, `super_1.img`, ... in some OEM ROMs) and `super.img.zst` (newer Xiaomi) aren't read.
- Only the first block device (`super`) is supported; retrofit devices with super spread over system/vendor aren't.
- Older Xiaomi fastboot ROMs without super.img (raw `system.img`, ...) are refused.
- A repacked super.img always gets new extents for every partition; unchanged partitions could keep their place.
- The archive is written with gzip level `zip.level` on one thread; `.tgz` output takes most of the repack time.

## Repack

- The sdat (surya) and fastboot (Pixel) repack paths were not run end to end again after `shared_blocks` dedup and the inode-count change in the ext4 builder (the ROMs are no longer on disk); only unit tests cover them.

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
