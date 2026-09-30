# changelog
## Unreleased
- EROFS: own extractor (uncompressed images: flat, inline tail and chunk-based files, shared/inline xattrs) and image builder; partitions are rebuilt with the filesystem they came with
- Fastboot ROMs: Pixel factory images (`<device>-<build>/image-*.zip`) and plain `image-*.zip` unpack and repack. The logical partitions from `super_empty.img` are read straight out of the zip; repack checks the super group size and disables dm-verity/verification in `vbmeta.img`
- payload.bin ROMs (A/B OTA zips, full OTAs): unpack dumps the images straight from the zip (REPLACE, REPLACE_XZ, REPLACE_BZ, ZSTD, ZERO; decoded in parallel, SHA-256 checked) and extracts the logical partitions; the other images go to `rom/payload/`
- New command: `jancox payload` dumps the images of a payload.bin or OTA zip
- super.img ROMs: Xiaomi fastboot `.tgz` ROMs and zips with a `super.img` (raw or sparse) unpack and repack. The logical partitions are read straight out of the (sparse) super image; repack writes a new super.img with the same metadata, patches vbmeta, and renews Xiaomi's `crclist.txt` / `sparsecrclist.txt` so `flash_all` still passes the bootloader's CRC check
- New command: `jancox super` lists and dumps the partitions of a super image
- `output.format` in `jancox.prop` (or `repack -t`, a list): `auto` (default, like the input), `fastboot` (images + flash-all.sh/.bat from any ROM, e.g. a recovery ROM), `sdat`, `payload`, `super`; formats a ROM can't become are refused with the reason
- Input: any zip, `.tgz`, `.tar.gz`, `.tar` or bare `payload.bin` in `input/` is found by its content, whatever its name; `input.zip`/`.tgz`/`.tar.gz`/`.tar` and `payload.bin` in the work folder too
- payload.bin ROMs repack: a new OTA zip (signed payload.bin with the AOSP test key or your own, payload_properties.txt, metadata with new property files, whole-zip signature) and/or a fastboot ROM with flash-all.sh/.bat. Pick with `payload.output=payload|fastboot|both` in `jancox.prop` (default payload) or `repack -t`. Unchanged images are copied from the old payload; vbmeta gets the disable-verity flags
- ext4: images with `shared_blocks` (Android 10+ block sharing) are rebuilt with it, so an unchanged partition keeps its size
- ext4: a rebuild at the original size keeps the original inode count
- EROFS: compressed images are read: lz4/lz4hc, lzma, deflate, zstd, with big pclusters, ztailpacking, fragments, dedupe and both index formats (checked against fsck.erofs and a kernel mount on a Xiaomi HyperOS ROM). They are rebuilt with lz4 (own compressor filling one-block clusters, like mkfs.erofs), about as small as the originals (Xiaomi system 759 MiB vs 758 MiB), compressed in parallel; checked with fsck.erofs and a kernel mount
- Recovery ROMs with uncompressed EROFS inside `*.new.dat.br` work too

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
