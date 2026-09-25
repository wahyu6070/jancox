# TODO

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

## ext4 extractor: open points

- Android sparse images (`simg`) are rejected; only raw images are read. Could read them through the sparse reader in img2sdat.
- Hard links are extracted as separate copies and not recorded.
- Paths with whitespace can't be written to fs_config / file_contexts; they only get a warning.
- `shared_blocks` (Android 10+ deduplicated ext4) is untested: no tool here creates such images.
