# Tested devices and ROMs

ROMs that Jancox 3.x has been tested with. Add a row when you test another ROM: open an issue or a pull request with the device, the ROM zip name, what worked and what didn't.

"Unpack" and "Repack" mean the check listed in the notes passed on a PC (Linux). "Boot" means the repacked ROM was flashed and booted on the device.

| Device | Codename | ROM | Android | ROM type | Filesystem | Unpack | Repack | Boot | Jancox |
|--------|----------|-----|---------|----------|------------|--------|--------|------|--------|
| Xiaomi POCO X3 NFC | surya | crDroid | 15 | Recovery (`*.new.dat.br`, dynamic partitions) | ext4 | ✅ | ✅ | not tested | 3.0.0 |
| Google Pixel | cubs | Factory image `cubs-cd1a.260905.001.b1` | 17 | Fastboot (`image-*.zip`, `super_empty.img`) | EROFS (uncompressed) | ✅ | ✅ | not tested | 3.0.1 |
| ASUS ROG Phone 5 (ZS673KS) | ASUS_I005_1 (I005D) | Stock WW `33.0210.0210.200` (`UL-ASUS_I005_1-ASUS-33.0210.0210.200-1.1.300-2304-user.zip`) | 13 | A/B OTA (`payload.bin`, full OTA, dynamic partitions) | ext4 (`shared_blocks`) | ✅ | ✅ (OTA zip + fastboot) | not tested | 3.0.1 |
| Xiaomi Redmi Note 13 4G | sapphire | HyperOS 2 Global `OS2.0.205.0.VNGMIXM` (`sapphire_global-ota_full-OS2.0.205.0.VNGMIXM-user-15.0-8f094cff3e.zip`) | 15 | A/B OTA (`payload.bin`, full OTA, dynamic partitions) | EROFS (lz4) | ✅ | ✅ | not tested | 3.0.1 |
| Xiaomi POCO F4 | munch | MIUI 14 Global `V14.0.6.0.TLMMIXM` fastboot (`munch_global_images_V14.0.6.0.TLMMIXM_20240204.0000.00_13.0_global_418d21cc7e.tgz`) | 13 | Fastboot `.tgz` with sparse `super.img` | ext4 | ✅ | ✅ | not tested | 3.0.1 |
| Xiaomi POCO F4 | munch | MIUI 14 Global `V14.0.6.0.TLMMIXM` recovery (`miui_MUNCHGlobal_V14.0.6.0.TLMMIXM_60b6629f69_13.0.zip`) | 13 | A/B OTA (`payload.bin`) | ext4 | ✅ | not run | not tested | 3.0.1 |

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
- Repack (`-t both`, about 3 minutes): a signed OTA zip (3.9 GB) and a fastboot zip. The new payload passes AOSP `paycheck.py --check` with the test key. The payload/metadata signatures, `payload_properties.txt`, the `ota-property-files` offsets and the whole-zip signature (`openssl cms -verify`) all check out. The re-encoded xz blobs decode with liblzma (the reference xz library) to the right SHA-256. The rebuilt partitions pass `e2fsck -fn`. Unpacking the new zip gives the same folders, metadata and images (vbmeta differs only in the disable-verity flags).

### Xiaomi Redmi Note 13 4G (sapphire), HyperOS 2 OS2.0.205.0.VNGMIXM, Android 15

- A/B full OTA: `payload.bin` with 29 partitions; the dynamic group `qti_dynamic_partitions` holds odm, product, system, system_dlkm, system_ext, vendor, vendor_dlkm and mi_ext, all EROFS with lz4 (0padding, one-block pclusters).
- Unpack (about 1.7 minutes): all 8 partitions extract identical to `fsck.erofs --extract` 1.9.4; vendor also matches a kernel loop mount (contents, owners, modes, SELinux labels).
- Uncompressed the partitions would need 8.99 GB, more than the 7.16 GiB super group, so repack rebuilds them with lz4: product 2903 MiB (Xiaomi: 2910), system 759 (758), system_ext 602 (599), vendor 878 (874). All 8 rebuilt images in the final OTA pass `fsck.erofs` 1.9.4 (with `--extract`, full decoding) and match a kernel loop mount; on system and vendor the mount also shows the same owners, modes and SELinux labels (0 differences in 7840 entries).
- Repack takes about 3.8 minutes; the new OTA passes AOSP `paycheck.py --check`, and unpacking it gives the same folders and metadata for all 8 partitions.

### Xiaomi POCO F4 (munch), MIUI 14 V14.0.6.0.TLMMIXM, Android 13

- Fastboot ROM: a 6 GB `.tgz` with `images/super.img` (sparse, 8.5 GB; 3 metadata slots, groups `qti_dynamic_partitions_a/_b`), 6 ext4 logical partitions in slot a.
- Unpack (about 1.5 minutes, 10 MB of memory): the partitions are read straight out of the sparse super image. They are identical to the ones unpacked from the recovery OTA of the same version (payload.bin).
- Repack (about 2.7 minutes): a new `.tgz` whose file list (system `tar`) is the same as the original. The new super.img converts with `simg2img`; an independent parse of its liblp metadata finds valid checksums and no overlapping extents; each partition passes `e2fsck -fn` and extracts identical to the work folders. The new `crclist.txt` / `sparsecrclist.txt` are identical to what the ROM's own `flash_gen_crc_list.py` computes for the new images.
