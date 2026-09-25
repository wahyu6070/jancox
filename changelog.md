# changelog
## Unreleased
- EROFS: own extractor (uncompressed images: flat, inline tail and chunk-based files, shared/inline xattrs) and image builder; partitions are rebuilt with the filesystem they came with
- Fastboot ROMs: Pixel factory images (`<device>-<build>/image-*.zip`) and plain `image-*.zip` unpack and repack. The logical partitions from `super_empty.img` are read straight out of the zip; repack checks the super group size and disables dm-verity/verification in `vbmeta.img`
- Recovery ROMs with uncompressed EROFS inside `*.new.dat.br` work too. Compressed EROFS (lz4/lzma, used by most non-Pixel ROMs) is not supported yet

## 3.0.0 25-09-2026
- Rewritten in Rust: one `jancox` binary for Android, Linux and Windows; no Python, busybox or Termux packages, no root needed
- New commands: unpack, repack, cleanup, extract, build, sdat2img, img2sdat, brotli
- Own ext4 extractor and image builder; owners, modes, SELinux labels and capabilities are kept in config/ metadata files
- Works in the folder it is run from (input/ -> output/)
- Repack grows full dynamic partitions and updates dynamic_partitions_op_list
- The module only installs the jancox binary for the device's CPU
- Not supported yet: EROFS, payload.bin ROMs

## 2.5
- Fix zip failed in 64bit arch
- Add support 32/64bit arch
- Move using payload dumper go
- Fix img extractor issue payload.bin
-_Update Binary : busybox,brotli,zip,unzip
- Payload is not support repack
- Change rename zip rom

## 2.3 25-03-2021
- Using kopi installer (support magisk non magisk)
- move zip compression 7z to zip

## 2.2 12-08-2020
- Fix unzip magic
- Supported bin payload (Asus Ota)
- Fix rom info error
- Supported x86/x86-64 arch
- Remove Python (install manual in termux)
- Added log
- Added Jancox Menu ( Run "jancoxmenu" in terminal)
- Added debloater
- and other improvements

## 2.1 05-05-2020
- Fix Repack error
- Fix exe not copying in termux
- And other improvements
