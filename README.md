# Jancox tool

Unpack and repack Android ROM zips on Android, Linux and Windows.

Jancox is one `jancox` binary, written in Rust, with no dependencies. Unpacking and repacking need no root.

- Unpack a ROM zip into folders you can edit:
  - recovery ROMs (`*.new.dat.br` / `*.new.dat` + `*.transfer.list`)
  - fastboot ROMs such as Pixel factory images (`<device>-<build>/image-*.zip`, flashed with `flash-all.sh`), or an `image-*.zip` on its own
  - A/B OTA zips with `payload.bin` (full OTAs, or a bare `payload.bin`), repacked as a new signed OTA zip and/or a fastboot ROM
  - ROMs with a `super.img` (raw or sparse): Xiaomi fastboot ROMs (`<device>_images_<version>.tgz`) and zips
- ext4 and EROFS partitions.
- Repack the folders into a new ROM zip of the same kind.
- Keeps owners, permissions, SELinux labels and capabilities in metadata files, so it works on `/sdcard` and on Windows too.
- Grows full dynamic partitions and updates `dynamic_partitions_op_list`.

Devices and ROMs it has been tested with: [device_tested.md](device_tested.md).

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
2. Put the ROM in `input/`: a zip, `.tgz` / `.tar.gz`, `.tar` or a bare `payload.bin`, with any name. Without `input/`, `input.zip`, `input.tgz`, `input.tar.gz`, `input.tar` or `payload.bin` in the work folder are used.
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

### Fastboot ROMs (Pixel factory images)

Put the factory zip (e.g. `cubs-cd1a.260905.001.b1-factory-3bd92fff.zip` from [developers.google.com/android/images](https://developers.google.com/android/images)) in `input/` and run `jancox unpack` as usual.

- The logical partitions listed in `super_empty.img` (system, system_ext, product, vendor, system_dlkm, vendor_dlkm) go to `partition/`. They are read straight from the zip, so no temporary images are written.
- Everything else goes to `rom/<device>-<build>/`, and the files of `image-*.zip` go to `rom/<device>-<build>/image-<device>-<build>/`. Firmware images such as modem, radio and bootloader, plus `system_other.img`, are kept as they are.
- `jancox repack` rebuilds each partition with its original filesystem (EROFS on Pixels). It checks that they fit the super partition group, sets the disable-verity flags in `vbmeta.img`, and writes `output/NewROM-<date>.zip` with the same layout. Flash it with `flash-all.sh` / `flash-all.bat` on an unlocked device.

### payload.bin ROMs (A/B OTA zips)

Full OTA zips with a `payload.bin` (e.g. ASUS, Xiaomi, OnePlus stock ROMs) unpack with `jancox unpack` as usual. Incremental OTAs are refused.

- The logical partitions (system, system_ext, product, vendor, odm, ...) go to `partition/`.
- The other images (boot, vendor_boot, dtbo, vbmeta, modem, bootloader, ...) go to `rom/payload/<name>.img`.
- Every image is checked against the SHA-256 in the payload.

With `output.format=auto`, `jancox repack` makes what `payload.output` in `jancox.prop` says:

| `payload.output` | Result |
|---|---|
| `payload` (default) | `output/NewROM-<date>.zip`: a new OTA zip with `payload.bin`. Flash it in a custom recovery (TWRP, OrangeFox, ...) or with `adb sideload`. |
| `fastboot` | `output/NewROM-<date>.zip` with all images and `flash-all.sh` / `flash-all.bat`: firmware in the bootloader, the logical partitions in fastbootd. |
| `both` | both zips (`NewROM-<date>.zip` and `NewROM-<date>-fastboot.zip`). |

- The logical partitions are rebuilt. An image in `rom/payload/` that you replaced (e.g. a patched `boot.img`) is encoded again. Unchanged images are copied from the old payload as they are.
- `vbmeta.img` gets the disable-verity/verification flags, so the device must be unlocked.
- `payload.bin` and the zip are signed with the AOSP test key. TWRP, OrangeFox and other test-keys recoveries accept it; stock recoveries only take the vendor's key. Set `sign.key` / `sign.cert` in `jancox.prop` to sign with your own key.
- `payload.xz_level` (0-9, default 1) sets the xz level of the rebuilt partitions: 6 makes a ~7% smaller zip but is about 4x slower.
- Images made with block sharing (`shared_blocks`, most Android 10+ ext4 images) are rebuilt with it, so they keep their size.

`jancox payload ota.zip -o images/` only dumps the images (like payload-dumper); `-p boot,vendor_boot` picks some, `-l` lists them.

### Output format

`output.format` in `jancox.prop` (or `repack -t`) picks what repack makes; a list like `sdat,fastboot` makes one zip each. Missing or `auto` means the same kind of ROM as the input.

| Input \ `output.format` | `auto` | `fastboot` | `sdat` | `payload` | `super` |
|---|---|---|---|---|---|
| Recovery ROM (`*.new.dat[.br]`) | recovery ROM | images + `flash-all.sh/.bat` | ✅ | ❌ | ❌ |
| payload.bin ROM | `payload.output` | images + `flash-all.sh/.bat` | ❌ | ✅ | ❌ |
| Pixel factory ROM | factory ROM | (it is one) | ❌ | ❌ | ❌ |
| Xiaomi fastboot / super.img ROM | same archive | (it is one) | ❌ | ❌ | ✅ |

A generated fastboot ROM flashes the firmware and boot images in the bootloader and the logical (dynamic) partitions in fastbootd. The ❌ cases are refused with the reason: a recovery ROM needs the original updater-script, a payload the manifest of an A/B OTA, a super.img the super metadata of the device.

### Xiaomi fastboot ROMs (super.img)

Put the fastboot ROM (e.g. `munch_global_images_V14.0.6.0.TLMMIXM_..._13.0_global_418d21cc7e.tgz`) in `input/` and run `jancox unpack`.

- The archive is unpacked to `rom/`, except `images/super.img`: its ext4/EROFS partitions (slot a) go to `partition/`, its metadata and any other logical partition to `super/`. The sparse super.img is read in place, without a raw copy.
- `jancox repack` rebuilds the partitions, writes a new `super.img` with the same groups and size (sparse again), sets the disable-verity flags in `vbmeta.img`, and writes `output/NewROM-<date>.tgz` with the original files in their original order.
- Xiaomi's `flash_all.sh` flashes `images/crclist.txt` and `images/sparsecrclist.txt` first, and the bootloader refuses images that don't match them. Repack computes new lines for `super` and `vbmeta_ab` the way the ROM's own `flash_gen_crc_list.py` does.
- Flash with `flash_all.sh` / `flash_all.bat` on an unlocked device (they wipe data; `flash_all_except_storage` keeps it).

`jancox super super.img -o images/` only dumps the partitions of a super image (raw or sparse, like lpunpack); `-l` lists them.

### Commands

```
jancox init     [-w workdir]
jancox unpack   [rom] [-w workdir]                      zip / tgz / tar / payload.bin
jancox repack   [-w workdir] [-o out.zip] [-b brotli_quality] [-z zip_level] [-t auto|fastboot|sdat|payload|super,...]
jancox cleanup  [-w workdir] [--all]

jancox extract  <image> [-o outdir] [-p name]           ext4/EROFS image -> folder + metadata
jancox payload  <payload.bin|ota.zip> [-o outdir] [-p name,...] [-l]  payload -> images
jancox super    <super.img> [-o outdir] [-p name,...] [-l] [-s slot]   super.img -> images
jancox build    <workdir> <part> [-o image] [-s size|auto]  folder + metadata -> ext4/EROFS image
jancox sdat2img <transfer_list> <new_dat[.br]> [image]
jancox img2sdat <image> [-o outdir] [-v version] [-p prefix] [-b quality]
jancox brotli   [-d] [-q quality] [-w window] [-o output] <file>
```

`repack` uses brotli quality 1 by default, which is fast but gives a bigger zip than most ROMs ship with. Set `brotli.level=6` or higher in `jancox.prop` (or pass `-b 6`) for a smaller zip.

## Limitations

- EROFS images are read with any compression (lz4, lzma, deflate, zstd). A compressed partition is rebuilt with lz4 (one-block clusters, like mkfs.erofs' default, readable by Android kernels since 5.4), whatever algorithm it had.
- Incremental OTAs (payload.bin patches) are refused.
- Older Xiaomi fastboot ROMs without `super.img` (separate `system.img`, ...) are not supported.
- A repacked partition no longer matches its dm-verity hashtree / AVB data, so the ROM only boots with verification disabled (as with older Jancox versions). For fastboot ROMs, repack sets the "disable verity + verification" flags in `vbmeta.img`; this needs an unlocked bootloader, and the first flash with these flags needs a data wipe (`flash-all.sh` wipes by default).
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
