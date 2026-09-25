# Jancox tool

Unpack and repack Android ROM zips on Android, Linux and Windows.

Jancox is one `jancox` binary, written in Rust, with no dependencies. Unpacking and repacking need no root.

- Unpack a flashable ROM zip (`*.new.dat.br` / `*.new.dat` + `*.transfer.list`) into folders you can edit.
- Repack the folders into a new flashable ROM zip.
- Keeps owners, permissions, SELinux labels and capabilities in metadata files, so it works on `/sdcard` and on Windows too.
- Grows full dynamic partitions and updates `dynamic_partitions_op_list`.

## Install

### Android (root: Magisk, KernelSU, APatch)

1. Download `Jancox-tool-android-v<version>.zip` from [Releases](https://github.com/wahyu6070/jancox/releases).
2. Install it as a module in the Magisk / KernelSU / APatch app (or flash it in recovery), then reboot.
3. Open a terminal app (e.g. Termux) and run `jancox --help`.

The module installs `jancox` for your CPU into `/system/bin`, and also into Termux when Termux is installed.

### Termux (no root)

```sh
pkg install curl
curl -fsSL https://raw.githubusercontent.com/wahyu6070/jancox/master/install.sh | sh
```

`jancox` goes into `$PREFIX/bin`. To work on files in your internal storage, run `termux-setup-storage` once. The Termux home (`~`) is faster than `/sdcard` and keeps symlinks.

### Ubuntu / Debian

```sh
sudo apt install curl
curl -fsSL https://raw.githubusercontent.com/wahyu6070/jancox/master/install.sh | sh
```

`jancox` goes into `/usr/local/bin` (the script asks for your sudo password). It is a static binary, so it has no other dependencies.

### Other Linux

The same one line works on any Linux with `curl` (or `wget`) and `sh`. Without sudo it installs into `~/.local/bin`. Options go after `sh -s --`:

```sh
curl -fsSL https://raw.githubusercontent.com/wahyu6070/jancox/master/install.sh | sh -s -- --version v3.0.0
curl -fsSL https://raw.githubusercontent.com/wahyu6070/jancox/master/install.sh | sh -s -- --dir ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/wahyu6070/jancox/master/install.sh | sh -s -- --uninstall
```

You can also download `Jancox-tool-linux-<arch>-v<version>.zip`, extract it and run `./jancox` in that folder.

### Windows

1. Download `Jancox-tool-windows-x86_64-v<version>.zip` (`arm64` for ARM PCs) from [Releases](https://github.com/wahyu6070/jancox/releases).
2. Extract it to a folder you can write to, e.g. `D:\jancox` (not `C:\Program Files`).
3. Open cmd or PowerShell in that folder and run `jancox --help`. It doesn't need administrator rights.

If SmartScreen says "Windows protected your PC", click **More info → Run anyway** (the binary isn't signed). You can also right-click the zip → Properties → **Unblock** before extracting it.

## Download

Get the zips from [Releases](https://github.com/wahyu6070/jancox/releases).

| File | What it is |
|------|------------|
| `Jancox-tool-android-v<version>.zip` | Magisk / recovery module. Installs `jancox` for the device's CPU (arm, arm64, x86, x86_64) into `/system/bin`, and into Termux when it is installed. |
| `Jancox-tool-linux-<arch>-v<version>.zip` | `jancox` + empty `input/` and `output/`. Static binary, `arch` = `x86_64`, `arm64`, `x86`, `arm`. |
| `Jancox-tool-windows-<arch>-v<version>.zip` | `jancox.exe` + empty `input/` and `output/`. `arch` = `x86_64`, `x86`, `arm64`. |
| `jancox-<os>-<arch>` | The plain binaries, used by `install.sh`. |

## Usage

`jancox` works in the folder you run it from (or the folder given with `-w`).

1. `jancox init` makes `input/`, `output/` and `jancox.prop` (optional; `unpack` does it too when no ROM is found).
2. Put the ROM zip in `input/`.
3. `jancox unpack`
4. Edit the files in `partition/system/`, `partition/vendor/`, `partition/product/`, ...
5. `jancox repack`. The new ROM is written to `output/NewROM-<date>.zip`.
6. `jancox cleanup` removes the unpacked files. It keeps `input/`, `output/` and `jancox.prop` (`--all` also removes `output/`).

After `unpack` the folder looks like this:

```
input/                               your ROM zip
output/                              new ROMs
jancox.prop                          settings (brotli.level, zip.level; default 1)
rom/                                 the rest of the ROM (META-INF, boot.img, firmware, ...)
partition/system/ vendor/ ...        partition files, edit these
partition/config/<part>_fs_config    owner, group, mode, capabilities
partition/config/<part>_file_contexts SELinux labels
partition/config/<part>_symlinks     symlinks
partition/config/<part>_info         filesystem parameters (size, UUID, ...)
```

- Added files get default owners and modes (`0 0 0644`, or `0 2000 0755` in `bin/`) and the SELinux label of their folder. To change them, edit `partition/config/<part>_fs_config` and `<part>_file_contexts`.
- Deleted files are left out of the new image.
- Where the storage can't hold symlinks (`/sdcard`, Windows), `unpack` says so, and symlinks live only in `partition/config/<part>_symlinks`; `repack` puts them back. Remove a line there to delete a symlink. Working in a folder with symlink support (e.g. the Termux home `~`) shows them as real symlinks.

### Commands

```
jancox init     [-w workdir]
jancox unpack   [rom.zip] [-w workdir]
jancox repack   [-w workdir] [-o out.zip] [-b brotli_quality] [-z zip_level]
jancox cleanup  [-w workdir] [--all]

jancox extract  <image> [-o outdir] [-p name]           ext4 image -> folder + metadata
jancox build    <workdir> <part> [-o image] [-s size|auto]  folder + metadata -> ext4 image
jancox sdat2img <transfer_list> <new_dat[.br]> [image]
jancox img2sdat <image> [-o outdir] [-v version] [-p prefix] [-b quality]
jancox brotli   [-d] [-q quality] [-w window] [-o output] <file>
```

`repack` uses brotli quality 1 by default, which is fast but gives a bigger zip than most ROMs ship with. Set `brotli.level=6` or higher in `jancox.prop` (or pass `-b 6`) for a smaller zip.

## Limitations

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
