# Jancox-tool

Unpack/repack Android ROMs. Being rewritten from shell + Python into a single Rust binary (`jancox`) for Android, Linux and Windows.

## Layout

- `crates/jancox-core/`: library with the ROM logic.
  - `br.rs`: brotli compress/decompress (`brotli` crate). `Encoder::finish()` must be used: `brotli::CompressorWriter` swallows errors from its final flush.
  - `sdat.rs`: `*.new.dat[.br]` -> image (sdat2img). `dat.rs`: image -> `*.new.dat[.br]` (img2sdat), streaming through brotli.
  - `fs/`: read-only filesystem readers behind the `Filesystem` trait. `fs/ext4.rs` is our own ext2/3/4 reader (extents, block maps, htree dirs read linearly, inline data, xattrs in-inode/block/EA inode). `fs/erofs.rs` reads uncompressed EROFS (compact/extended inodes, flat plain/inline and chunk-based files, shared/inline xattrs, long prefixes); compressed files are refused (see `TODO.md`). `fs::Window` reads a byte range of a file, e.g. an image stored in a zip.
  - `extract.rs`: extracts an ext4 or EROFS image (from a path, or any `Read + Seek` via `extract_reader`) to `<out>/<part>/` and writes `<out>/config/<part>_{fs_config,file_contexts,symlinks,info}`. Paths are device paths built from the mount point, which comes from the volume name (`/` = system-as-root); `last_mounted` is unreliable (old Jancox overwrote it). EROFS images have no volume name: a root with `system/build.prop` means system-as-root (`/`). Paths with whitespace can't be expressed in fs_config and only get a warning.
  - `fs/mkerofs.rs`: our uncompressed EROFS writer (superblock + shared xattrs + inodes from block 0, compact inodes unless ids/sizes need extended ones, FLAT_INLINE tails, sorted dirs incl. `.`/`..`, sb checksum); `build.rs` picks it when `<part>_info` says `fs_type=erofs`, and sizes EROFS images to fit their content.
  - `fs/mkext4.rs`: our ext4 image writer (no journal, no flex_bg, linear dirs, 256-byte inodes, extents with multi-level trees, all-zero file blocks left as holes, xattrs in-inode or in one xattr block). `build.rs`: folder + metadata -> `mkext4::Node` tree; the folder decides which files exist, new entries get Android-like defaults and inherit their directory's label, known entries without a label stay unlabeled.
  - `factory.rs`: fastboot ROMs (Pixel factory zip with a stored `image-*.zip`, or an image zip on its own), stored zip entry ranges, `super_empty.img` groups (liblp), the vbmeta disable-verity flags.
  - `rom.rs`: work folder = where jancox runs (`-w`): `input/`, `output/`, `jancox.prop` (made by `init`; `brotli.level`, `zip.level`, read by `repack`, CLI flags win), `rom/`, `partition/<part>/` + `partition/config/`. `unpack` (zip -> brotli -> sdat2img -> extract, streaming, state in `<work>/jancox_rom`; warns once when the storage can't hold symlinks, they stay in `_symlinks`), `repack` (build -> img2sdat -> brotli straight into the zip; grows full partitions and updates `dynamic_partitions_op_list` on dynamic ROMs), `cleanup` (never touches `input/`). Fastboot ROMs (`format=fastboot` in `jancox_rom`): unpack reads the logical partitions from `super_empty.img` straight from the inner zip, and the rest of the image zip goes to `rom/<image zip without .zip>/`; repack rebuilds the images, checks the super groups, patches the vbmeta flags, then writes the image zip (original entry order) and the outer zip.
  - Verify builder/repack changes with a round trip: extract -> build -> `e2fsck -fn` must exit 0 -> extract again -> folder, fs_config, file_contexts and symlinks identical. The crDroid surya ROM (4 ext4 partitions, `.new.dat.br`, dynamic) is in `input/` (gitignored) for end-to-end tests with `-w work`.
  - EROFS: verify with erofs-utils 1.9.4 (`fsck.erofs`, `--extract` as the reference; 1.7.1 truncates chunk-based files) and, where sudo works, `mount -t erofs -o ro,loop`. The Pixel factory zip (cubs, 15 GB, EROFS, fastboot) is in `work/pixel/input/` (gitignored); an unpack takes ~1 min, a repack ~4 min.
  - Verify extractor changes against `debugfs` (`stat` + `ea_get` for long labels, `rdump` for contents). `debugfs rdump` pads inline-data files to 60 bytes, so compare those with the source tree instead.
- Only sdat2img and img2sdat live in their own repos ([sdat2img-rust](https://github.com/wahyu6070/sdat2img-rust), [img2sdat-rust](https://github.com/wahyu6070/img2sdat-rust)), used as git dependencies pinned by tag: fix them there, tag a new version, then bump the tag in `Cargo.toml`. Everything else (brotli, the image extractor, ...) is written in this repo.
- `crates/jancox-cli/`: the `jancox` binary; one binary with subcommands (`init`, `unpack`, `repack`, `cleanup`, `extract`, `build`, `sdat2img`, `img2sdat`, `brotli`).
- `android/`: the Magisk/recovery flashable module; it is only an installer. `customize.sh` picks `bin/<arch>/jancox` for the device and installs it to `/system/bin/jancox`. The old shell + Python tool was removed (last shipped in v2.5, still in git history).
- `build.sh`: cross-compiles all targets (zig + cargo-zigbuild for Linux/Windows, Android NDK for Android). `./build.sh module` builds the Android targets and packs `android/` + `bin/<arch>/jancox` into `dist/Jancox-tool-android-<version>.zip`.

## Rules

- `update.json` and `changelog.md` must stay at the repo root on `master`: installed modules poll them through the raw GitHub URL in `android/module.prop` (`updateJson`).
- Before committing, run `cargo fmt`, `cargo clippy --all-targets -- -D warnings` (zero warnings) and `cargo test`.
- Commit `Cargo.lock`; `build.sh` builds with `--locked`.
- On a release, bump `version` in `Cargo.toml`, `version`/`versionCode` in `android/module.prop`, and `update.json` together.

## Git workflow

- This repo only uses one branch: `master`. Work and commit directly on `master`; don't create feature branches.
- Don't rename `master` to `main`: the `updateJson` URL points at `refs/heads/master`.
- The repo was renamed from `Jancox-tool-android` to `jancox` (2026-09-25). GitHub redirects the old URLs, and installed v2.5 modules still poll the old `updateJson` URL through that redirect: never create a new repo named `Jancox-tool-android`. Use `wahyu6070/jancox` in new URLs.
