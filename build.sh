#!/usr/bin/env bash
#
# Cross-compile jancox for Android, Linux and Windows and pack one zip per
# platform. The Android zip is the Magisk/recovery flashable module
# (android/ + bin/<arch>/jancox for every Android architecture).
#
# Usage: ./build.sh [all|android|linux|windows|<target-name>...]
#        ./build.sh --list
#
# Requirements:
#   - rustup / cargo              https://rustup.rs
#   - zig + cargo-zigbuild        Linux and Windows targets
#   - Android NDK (r23+)          Android targets; set ANDROID_NDK_HOME or put it in ~/Android
#   - zip
#
# Output: dist/Jancox-tool-android-v<version>.zip         flashable module
#         dist/Jancox-tool-linux-<arch>-v<version>.zip    jancox + input/ + output/
#         dist/Jancox-tool-windows-<arch>-v<version>.zip  jancox.exe + input/ + output/
#         dist/jancox-<os>-<arch>[.exe]                  plain binaries (for install.sh)
#         dist/SHA256SUMS

set -euo pipefail
cd "$(dirname "$0")"

BIN=jancox
DIST=dist
STAGE=target/package
MODULE_DIR=android
ANDROID_API=${ANDROID_API:-21}
VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -n1)

# name | rust target triple | builder (zig, or ndk:<clang prefix>)
TARGETS=(
    "linux-x86_64|x86_64-unknown-linux-musl|zig"
    "linux-x86|i686-unknown-linux-musl|zig"
    "linux-arm64|aarch64-unknown-linux-musl|zig"
    "linux-arm|armv7-unknown-linux-musleabihf|zig"
    "android-arm64|aarch64-linux-android|ndk:aarch64-linux-android"
    "android-arm|armv7-linux-androideabi|ndk:armv7a-linux-androideabi"
    "android-x86|i686-linux-android|ndk:i686-linux-android"
    "android-x86_64|x86_64-linux-android|ndk:x86_64-linux-android"
    "windows-x86_64|x86_64-pc-windows-gnu|zig"
    "windows-x86|i686-pc-windows-gnu|zig"
    "windows-arm64|aarch64-pc-windows-gnullvm|zig"
)

if [[ -t 1 ]]; then
    RED=$'\e[31m' GREEN=$'\e[32m' YELLOW=$'\e[33m' BOLD=$'\e[1m' RESET=$'\e[0m'
else
    RED='' GREEN='' YELLOW='' BOLD='' RESET=''
fi

info() { echo "${BOLD}==>${RESET} $*"; }
warn() { echo "${YELLOW}warning:${RESET} $*" >&2; }
die() { echo "${RED}error:${RESET} $*" >&2; exit 1; }

usage() {
    sed -n '3,20p' "$0" | sed 's/^# \{0,1\}//'
}

# Locate the Android NDK: explicit env vars first, then common install paths.
find_ndk() {
    local candidates=()
    local var
    for var in ANDROID_NDK_HOME ANDROID_NDK_ROOT ANDROID_NDK; do
        [[ -n "${!var:-}" ]] && candidates+=("${!var}")
    done
    local sdk
    for sdk in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" "$HOME/Android/Sdk"; do
        [[ -n "$sdk" && -d "$sdk/ndk" ]] && candidates+=($(ls -d "$sdk"/ndk/* 2>/dev/null | sort -rV))
    done
    candidates+=($(ls -d "$HOME"/Android/android-ndk-* /opt/android-ndk* 2>/dev/null | sort -rV))

    local dir
    for dir in "${candidates[@]}"; do
        if [[ -d "$dir/toolchains/llvm/prebuilt" ]]; then
            echo "$dir"
            return 0
        fi
    done
    return 1
}

ndk_bin_dir() {
    local host
    case "$(uname -s)" in
        Linux) host=linux-x86_64 ;;
        Darwin) host=darwin-x86_64 ;;
        *) return 1 ;;
    esac
    echo "$NDK/toolchains/llvm/prebuilt/$host/bin"
}

# Select targets from the command line arguments.
select_targets() {
    local arg entry name matched
    SELECTED=()
    [[ $# -eq 0 ]] && set -- all
    for arg in "$@"; do
        matched=0
        for entry in "${TARGETS[@]}"; do
            name=${entry%%|*}
            if [[ "$arg" == all || "$arg" == "$name" || "$name" == "$arg"-* ]]; then
                [[ " ${SELECTED[*]:-} " == *" $entry "* ]] || SELECTED+=("$entry")
                matched=1
            fi
        done
        [[ $matched -eq 1 ]] || die "unknown target '$arg' (see ./build.sh --list)"
    done
}

check_prereqs() {
    local need_zig=0 need_ndk=0 entry triple builder missing=()
    command -v cargo >/dev/null || die "cargo not found. Install Rust: https://rustup.rs"
    command -v rustup >/dev/null || die "rustup not found. Install Rust: https://rustup.rs"

    for entry in "${SELECTED[@]}"; do
        IFS='|' read -r _ triple builder <<<"$entry"
        [[ $builder == zig ]] && need_zig=1
        [[ $builder == ndk:* ]] && need_ndk=1
        rustup target list --installed | grep -qx "$triple" || missing+=("$triple")
    done

    command -v zip >/dev/null || die "zip not found. Install it with: sudo apt install zip"

    if [[ ${#missing[@]} -gt 0 ]]; then
        info "Installing missing Rust targets: ${missing[*]}"
        rustup target add "${missing[@]}"
    fi

    if [[ $need_zig -eq 1 ]]; then
        command -v zig >/dev/null ||
            die "zig not found. Download it from https://ziglang.org/download/ and add it to PATH."
        cargo zigbuild --help >/dev/null 2>&1 ||
            die "cargo-zigbuild not found. Install it with: cargo install --locked cargo-zigbuild"
    fi

    if [[ $need_ndk -eq 1 ]]; then
        NDK=$(find_ndk) ||
            die "Android NDK not found. Download it from https://developer.android.com/ndk/downloads and set ANDROID_NDK_HOME."
        NDK_BIN=$(ndk_bin_dir) || die "unsupported host for the Android NDK: $(uname -s)"
        [[ -d "$NDK_BIN" ]] || die "NDK toolchain directory not found: $NDK_BIN"
        info "Using Android NDK: $NDK (API $ANDROID_API)"
    fi
}

build_one() {
    local name=$1 triple=$2 builder=$3
    local ext='' linker var

    [[ $triple == *windows* ]] && ext=.exe

    case $builder in
        zig)
            cargo zigbuild --release --locked --target "$triple"
            ;;
        ndk:*)
            linker="$NDK_BIN/${builder#ndk:}${ANDROID_API}-clang"
            [[ -x "$linker" ]] || { echo "linker not found: $linker" >&2; return 1; }
            var="CARGO_TARGET_$(echo "$triple" | tr 'a-z-' 'A-Z_')_LINKER"
            env "$var=$linker" cargo build --release --locked --target "$triple"
            ;;
        *)
            echo "unknown builder: $builder" >&2
            return 1
            ;;
    esac

    # linux-x86_64 -> target/package/linux/x86_64/jancox
    local dir="$STAGE/${name%%-*}/${name#*-}"
    mkdir -p "$dir"
    cp "target/$triple/release/$BIN$ext" "$dir/$BIN$ext"
    # plain binary for install.sh: dist/jancox-linux-x86_64
    cp "target/$triple/release/$BIN$ext" "$DIST/$BIN-$name$ext"
}

# One zip per Linux/Windows architecture with the binary at the top and
# empty input/ and output/ folders; run jancox from that folder. Android is
# packed as the module.
package() {
    local arch_dir os arch zipfile dir
    for arch_dir in "$STAGE"/*/*/; do
        [[ -d "$arch_dir" ]] || continue
        arch=$(basename "$arch_dir")
        os=$(basename "$(dirname "$arch_dir")")
        [[ $os == android ]] && continue
        mkdir -p "$arch_dir/input" "$arch_dir/output"
        zipfile="$PWD/$DIST/Jancox-tool-$os-$arch-v$VERSION.zip"
        rm -f "$zipfile"
        (cd "$arch_dir" && zip -q -r -9 -X "$zipfile" .)
        info "Packaged $DIST/$(basename "$zipfile")"
    done
}

# Zip android/ plus bin/<arch>/jancox for every Android target into
# dist/Jancox-tool-android-<version>.zip. customize.sh installs the one that
# matches the device into /system/bin.
package_module() {
    local version zipfile entry name triple stage=target/module
    version=$(sed -n 's/^version=//p' "$MODULE_DIR/module.prop")
    [[ -n $version ]] || { echo "version not found in $MODULE_DIR/module.prop" >&2; return 1; }
    [[ $version == "v$VERSION" ]] ||
        warn "$MODULE_DIR/module.prop has version $version but Cargo.toml has $VERSION"

    rm -rf "$stage"
    cp -a "$MODULE_DIR" "$stage"
    for entry in "${TARGETS[@]}"; do
        IFS='|' read -r name triple _ <<<"$entry"
        [[ $name == android-* ]] || continue
        # android-arm64 -> bin/arm64/jancox
        [[ -f "target/$triple/release/$BIN" ]] ||
            { echo "missing Android binary: target/$triple/release/$BIN" >&2; return 1; }
        mkdir -p "$stage/bin/${name#android-}"
        cp "target/$triple/release/$BIN" "$stage/bin/${name#android-}/$BIN"
    done

    zipfile="$PWD/$DIST/Jancox-tool-android-$version.zip"
    rm -f "$zipfile"
    (cd "$stage" && zip -q -r -9 -X "$zipfile" .)
    info "Packaged $DIST/Jancox-tool-android-$version.zip"
}

main() {
    case "${1:-}" in
        -h | --help) usage; exit 0 ;;
        -l | --list)
            local entry
            for entry in "${TARGETS[@]}"; do
                IFS='|' read -r name triple _ <<<"$entry"
                printf '  %-16s %s\n' "$name" "$triple"
            done
            exit 0
            ;;
    esac

    # "module" is an old name for "android"
    local arg targets=()
    for arg in "${@:-all}"; do
        [[ $arg == module ]] && arg=android
        targets+=("$arg")
    done

    local entry name triple builder ok=() failed=()
    mkdir -p "$DIST"
    # a full build starts from an empty dist/; a partial one only replaces
    # its own zips
    [[ " ${targets[*]} " == *" all "* ]] && rm -f "$DIST"/*.zip "$DIST/$BIN"-*
    rm -f "$DIST"/SHA256SUMS

    select_targets "${targets[@]}"
    check_prereqs

    rm -rf "$STAGE"
    mkdir -p "$STAGE"
    info "Building $BIN $VERSION for ${#SELECTED[@]} target(s)"

    for entry in "${SELECTED[@]}"; do
        IFS='|' read -r name triple builder <<<"$entry"
        info "Building ${BOLD}$name${RESET} ($triple)"
        if (set -e; build_one "$name" "$triple" "$builder"); then
            ok+=("$name")
        else
            failed+=("$name")
            warn "build failed: $name"
        fi
    done

    [[ ${#ok[@]} -gt 0 ]] && package

    # the module needs a binary for every Android architecture
    local android_ok=0 android_all=0
    for entry in "${TARGETS[@]}"; do
        name=${entry%%|*}
        [[ $name == android-* ]] || continue
        android_all=$((android_all + 1))
        [[ " ${ok[*]:-} " == *" $name "* ]] && android_ok=$((android_ok + 1))
    done
    if [[ $android_ok -eq $android_all ]]; then
        if package_module; then
            ok+=(android-module)
        else
            failed+=(android-module)
        fi
    elif [[ $android_ok -gt 0 ]]; then
        warn "Android module not packed: it needs all $android_all Android targets (./build.sh android)"
    fi

    if compgen -G "$DIST/*.zip" >/dev/null || compgen -G "$DIST/$BIN-*" >/dev/null; then
        (cd "$DIST" && shopt -s nullglob && sha256sum -- *.zip "$BIN"-* >SHA256SUMS)
    fi

    echo
    info "Summary"
    for name in "${ok[@]}"; do echo "  ${GREEN}ok${RESET}      $name"; done
    for name in "${failed[@]}"; do echo "  ${RED}FAILED${RESET}  $name"; done
    echo
    echo "Packages: $DIST/"

    [[ ${#failed[@]} -eq 0 ]]
}

main "$@"
