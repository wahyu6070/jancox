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

- Parse the payload header and the `DeltaArchiveManifest` protobuf (hand-written decoder, no protobuf crate).
- Full OTAs only: REPLACE, REPLACE_BZ, REPLACE_XZ, ZERO, DISCARD operations. Needs bzip2 and xz decoders (pure Rust crates or our own).
- Incremental OTAs (SOURCE_COPY, SOURCE_BSDIFF, PUFFDIFF, BROTLI_BSDIFF, ZUCCHINI) need the old images: out of scope.
- Extract every partition image; ext4/EROFS ones go through `extract.rs`, the rest (boot, vbmeta, ...) to `rom/`.
- Repack: write a new full payload (REPLACE_XZ or REPLACE, SHA-256 per operation and per partition) and sign it with test keys? Or repack into a fastboot ROM (images + `super_empty.img`) instead, which needs no signing.

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
