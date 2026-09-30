AOSP test keys, from `build/make/target/product/security/` in the Android
Open Source Project (Apache-2.0). They are public: every AOSP-based test-keys
build (TWRP, OrangeFox, ...) trusts them. Jancox signs repacked payload.bin
OTA zips with them unless `sign.key` / `sign.cert` in `jancox.prop` point to
other keys.
