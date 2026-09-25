# Jancox tool

Unpack and repack Android ROM zips on Android, Linux and Windows.

Version 3 is a rewrite in Rust: one `jancox` binary with no dependencies. It needs no Python, busybox or Termux packages, and no root. **3.0.0 is a beta.**

- Unpack a flashable ROM zip (`*.new.dat.br` / `*.new.dat` + `*.transfer.list`) into folders you can edit.
- Repack the folders into a new flashable ROM zip.
- Keeps owners, permissions, SELinux labels and capabilities in metadata files, so it works on `/sdcard` and on Windows too.
- Grows full dynamic partitions and updates `dynamic_partitions_op_list`.

## Download

Get the zips from [Releases](https://github.com/wahyu6070/Jancox-tool-android/releases).

| File | What it is |
|------|------------|
| `Jancox-tool-android-v<version>.zip` | Magisk / recovery module. Installs `jancox` for the device's CPU (arm, arm64, x86, x86_64) into `/system/bin`, and into Termux when it is installed. |
| `Jancox-tool-linux-<arch>-v<version>.zip` | `jancox` + empty `input/` and `output/`. Static binary, `arch` = `x86_64`, `arm64`, `x86`, `arm`. |
| `Jancox-tool-windows-<arch>-v<version>.zip` | `jancox.exe` + empty `input/` and `output/`. `arch` = `x86_64`, `x86`, `arm64`. |

## Usage

`jancox` works in the folder you run it from (or the folder given with `-w`).

1. Put the ROM zip in `input/`.
2. `jancox unpack`
3. Edit the files in the partition folders (`system/`, `vendor/`, `product/`, ...).
4. `jancox repack`. The new ROM is written to `output/NewROM-<date>.zip`.
5. `jancox cleanup` removes the unpacked files. It keeps `input/` and `output/` (`--all` also removes `output/`).

After `unpack` the folder looks like this:

```
input/                     your ROM zip
rom/                       the rest of the ROM (META-INF, boot.img, firmware, ...)
system/ vendor/ ...        partition files, edit these
config/<part>_fs_config    owner, group, mode, capabilities
config/<part>_file_contexts SELinux labels
config/<part>_symlinks     symlinks
config/<part>_info         filesystem parameters (size, UUID, ...)
output/                    new ROMs
```

- Added files get default owners and modes (`0 0 0644`, or `0 2000 0755` in `bin/`) and the SELinux label of their folder. To change them, edit `config/<part>_fs_config` and `config/<part>_file_contexts`.
- Deleted files are left out of the new image.
- A symlink listed in `config/<part>_symlinks` stays even where the folder can't hold symlinks (Windows, `/sdcard`). Remove its line to delete it.

### Commands

```
jancox unpack   [rom.zip] [-w workdir]
jancox repack   [-w workdir] [-o out.zip] [-b brotli_quality] [-z zip_level]
jancox cleanup  [-w workdir] [--all]

jancox extract  <image> [-o outdir] [-p name]           ext4 image -> folder + metadata
jancox build    <workdir> <part> [-o image] [-s size|auto]  folder + metadata -> ext4 image
jancox sdat2img <transfer_list> <new_dat[.br]> [image]
jancox img2sdat <image> [-o outdir] [-v version] [-p prefix] [-b quality]
jancox brotli   [-d] [-q quality] [-w window] [-o output] <file>
```

`repack` uses brotli quality 1 by default, which is fast but gives a bigger zip than most ROMs ship with. Use `-b 6` or higher for a smaller zip.

## Limitations (beta)

- Only ext4 partitions. EROFS ROMs are not supported yet.
- `payload.bin` ROMs (A/B OTA zips) are not supported yet.
- A repacked partition no longer matches its dm-verity hashtree / AVB data, so the ROM only boots with verification disabled (as with older Jancox versions).
- Paths with spaces can't be stored in `fs_config` and get a warning.
- Hard links become separate files.

## Build

You need [Rust](https://rustup.rs) 1.88 or newer.

```sh
cargo build --release          # target/release/jancox
./build.sh                     # every release zip into dist/
```

`build.sh` needs [zig](https://ziglang.org/download/) + [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) for Linux and Windows, the [Android NDK](https://developer.android.com/ndk/downloads) for Android (set `ANDROID_NDK_HOME` or extract it into `~/Android`), and `zip`.

## Credits

- [sdat2img-rust](https://github.com/wahyu6070/sdat2img-rust) and [img2sdat-rust](https://github.com/wahyu6070/img2sdat-rust), Rust ports of [xpirt/sdat2img](https://github.com/xpirt/sdat2img) and [xpirt/img2sdat](https://github.com/xpirt/img2sdat) (with AOSP `blockimgdiff.py`)
- [rust-brotli](https://github.com/dropbox/rust-brotli) (Dropbox)
- [zip](https://github.com/zip-rs/zip2)
- Older versions: [Jamflux SUR](https://github.com/jamflux/SUR), busybox, payload-dumper-go

## Links

- [YouTube](https://www.youtube.com/c/wahyu6070)
- [Telegram](https://t.me/wahyu6070group)
