# Tested devices and ROMs

ROMs that Jancox 3.x has been tested with. Add a row when you test another ROM: open an issue or a pull request with the device, the ROM zip name, what worked and what didn't.

"Unpack" and "Repack" mean the check listed in the notes passed on a PC (Linux). "Boot" means the repacked ROM was flashed and booted on the device.

| Device | Codename | ROM | Android | ROM type | Filesystem | Unpack | Repack | Boot | Jancox |
|--------|----------|-----|---------|----------|------------|--------|--------|------|--------|
| Xiaomi POCO X3 NFC | surya | crDroid | 15 | Recovery (`*.new.dat.br`, dynamic partitions) | ext4 | ✅ | ✅ | not tested | 3.0.0 |
| Google Pixel | cubs | Factory image `cubs-cd1a.260905.001.b1` | 17 | Fastboot (`image-*.zip`, `super_empty.img`) | EROFS (uncompressed) | ✅ | ✅ | not tested | unreleased (after 3.0.0) |
| ASUS ROG Phone 5 (ZS673KS) | ASUS_I005_1 (I005D) | Stock WW `33.0210.0210.200` (`UL-ASUS_I005_1-ASUS-33.0210.0210.200-1.1.300-2304-user.zip`) | 13 | A/B OTA (`payload.bin`, full OTA, dynamic partitions) | ext4 (`shared_blocks`) | ✅ | ✅ (OTA zip + fastboot) | not tested | unreleased (after 3.0.0) |

## Notes

### Xiaomi POCO X3 NFC (surya), crDroid, Android 15

- 4 ext4 partitions in `.new.dat.br`, with `dynamic_partitions_op_list`.
- Round trip: unpack -> repack. The partitions of the new zip pass `e2fsck -fn`, and extracting them again gives the same files, fs_config, file_contexts and symlinks as the work folders.

### Google Pixel (cubs), factory image CD1A.260905.001.B1, Android 17

- Fastboot ROM: a stored `image-cubs-*.zip` inside the factory zip, and 6 EROFS logical partitions (system, system_dlkm, system_ext, product, vendor, vendor_dlkm).
- Round trip: unpack -> repack -> unpack. The trees and metadata are identical. The rebuilt images pass `fsck.erofs` 1.9.4 and loop-mount with the same owners, modes and SELinux labels.
- Repack sets the disable-verity/verification flags in `vbmeta.img`, so flashing needs an unlocked bootloader.

### ASUS ROG Phone 5 (ZS673KS), stock 33.0210.0210.200, Android 13

- A/B full OTA: `payload.bin` (3.6 GB) with 29 partitions, using REPLACE, REPLACE_XZ and REPLACE_BZ operations. Dynamic group `qti_dynamic_partitions` holds odm, product, system, system_ext and vendor, all ext4.
- The manifest is ASUS-modified: fields 16 and up hold ASUS strings.
- Unpack: all 29 images match the SHA-256 in the payload. The 5 logical partitions pass `e2fsck -fn` and are extracted to `partition/`. The other 24 images (boot, vendor_boot, vbmeta, modem, xrom, ...) are in `rom/payload/`. It takes about 1 minute; `jancox payload` alone dumps all images in about 40 s.
- The images use `shared_blocks` (block sharing). Jancox rebuilds them with it: an unchanged product image comes out with exactly the same number of used blocks (481884) as ASUS's.
- Repack (`-t both`, about 3 minutes): a signed OTA zip (3.9 GB) and a fastboot zip. The new payload passes AOSP `paycheck.py --check` with the test key. The payload/metadata signatures, `payload_properties.txt`, the `ota-property-files` offsets and the whole-zip signature (`openssl cms -verify`) all check out. The rebuilt partitions pass `e2fsck -fn`. Unpacking the new zip gives the same folders, metadata and images (vbmeta differs only in the disable-verity flags).
