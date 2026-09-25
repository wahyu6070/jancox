# TODO

## Installing

- Install from a terminal without the Magisk module: Termux (e.g. a Termux package or an install script that puts `jancox` in `$PREFIX/bin`) and Ubuntu/Linux (e.g. a `.deb` or an install script into `/usr/local/bin`).

## EROFS extractor (postponed)

On hold until there is a real EROFS ROM to test with (payload-based ROMs, Android 12+). Focus on ext4 first.

- Add `crates/jancox-core/src/fs/erofs.rs` implementing the `Filesystem` trait, so `extract.rs` works unchanged. `extract.rs` currently stops with "EROFS images are not supported yet" (EROFS magic `0xE0F5E1E2` at offset 1024).
- To support:
  - superblock, compact/extended inodes, directories
  - inline and shared xattrs (`security.selinux`, `security.capability`), including long xattr name prefixes
  - data layouts: flat plain, flat inline, chunk based, compressed full and compact indexes
  - compression: lz4 / lz4hc first (the Android default), then lzma (MicroLZMA) and deflate
  - big pclusters, ztailpacking, fragments (packed inode), dedupe
- Testing: `erofs-utils` works without sudo: `apt-get download erofs-utils && dpkg-deb -x erofs-utils_*.deb root` gives `mkfs.erofs`, `fsck.erofs` (`--extract` as the reference) and `dump.erofs` (1.7.1 has lz4, lz4hc, lzma, deflate). Check against a real ROM before calling it done.

## Repack

- AVB / dm-verity: a rebuilt partition no longer matches its hashtree and AVB footer (the tail of the partition past the filesystem, and `vbmeta*.img`). Like the old Jancox, the result only boots with verification disabled. Add an option to patch `vbmeta.img` / `vbmeta_system.img` flags (disable verity + verification), or regenerate the hashtree.
- payload.bin ROMs (A/B): unpack is refused for now.
- `NewROM-<date>.zip` uses UTC; local time needs a timezone source.
- A config file for defaults (the old `jancox.prop`: brotli level, zip level).
- The auto size (`build -s auto`, growing a full dynamic partition) counts file blocks before holes are removed, so it overestimates a little.

## ext4 builder: open points

- Directories are linear (no `dir_index` htree); fine for Android sizes, slower lookups in huge directories.
- No `shared_blocks` dedup, no hard links (see below).

## ext4 extractor: open points

- Android sparse images (`simg`) are rejected; only raw images are read. Could read them through the sparse reader in img2sdat.
- Hard links are extracted as separate copies and not recorded.
- Paths with whitespace can't be written to fs_config / file_contexts; they only get a warning.
- `shared_blocks` (Android 10+ deduplicated ext4) is untested: no tool here creates such images.
