#!/bin/sh
# Jancox tool installer for Linux (Ubuntu, Debian, ...) and Termux.
#
#   curl -fsSL https://raw.githubusercontent.com/wahyu6070/jancox/master/install.sh | sh
#
# Options (pass them with `| sh -s -- <options>`):
#   --version vX.Y.Z   install that release (default: the newest one)
#   --dir DIR          install into DIR (default: $PREFIX/bin on Termux,
#                      /usr/local/bin on Linux, ~/.local/bin without sudo)
#   --uninstall        remove jancox
#
# Environment: JANCOX_VERSION, JANCOX_INSTALL_DIR, JANCOX_TARGET (e.g.
# linux-x86_64, android-arm64), JANCOX_RELEASE_URL (folder with the release
# files, for testing).

set -eu

REPO=wahyu6070/jancox
VERSION=${JANCOX_VERSION:-}
DIR=${JANCOX_INSTALL_DIR:-}
TARGET=${JANCOX_TARGET:-}
BASE=${JANCOX_RELEASE_URL:-}
ACTION=install

say() { printf '%s\n' "$*"; }
die() { printf 'jancox install: %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<EOF
Install jancox on Linux or Termux.

  curl -fsSL https://raw.githubusercontent.com/$REPO/master/install.sh | sh
  curl -fsSL .../install.sh | sh -s -- [options]

  --version vX.Y.Z   install that release (default: the newest one)
  --dir DIR          install into DIR (default: \$PREFIX/bin on Termux,
                     /usr/local/bin on Linux, ~/.local/bin without sudo)
  --uninstall        remove jancox
EOF
}

while [ $# -gt 0 ]; do
    case $1 in
        --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION=$2; shift ;;
        --dir) [ $# -ge 2 ] || die "--dir needs a value"; DIR=$2; shift ;;
        --uninstall) ACTION=uninstall ;;
        -h | --help) usage; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
    shift
done

is_termux() {
    [ -n "${TERMUX_VERSION:-}" ] && return 0
    case ${PREFIX:-} in *com.termux*) return 0 ;; esac
    return 1
}

# os-arch of the binary to install
if [ -z "$TARGET" ]; then
    if is_termux; then
        os=android
    else
        case $(uname -s) in
            Linux) os=linux ;;
            *) die "unsupported system $(uname -s); on Windows download the zip from https://github.com/$REPO/releases" ;;
        esac
    fi
    case $(uname -m) in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=arm64 ;;
        armv7* | armv8l | armhf | arm) arch=arm ;; # armv8l: 32-bit userland
        i?86) arch=x86 ;;
        *) die "unsupported CPU $(uname -m)" ;;
    esac
    TARGET=$os-$arch
fi

SUDO=
if [ -z "$DIR" ]; then
    if is_termux; then
        DIR=$PREFIX/bin
    elif [ "$(id -u)" = 0 ]; then
        DIR=/usr/local/bin
    elif command -v sudo >/dev/null 2>&1; then
        DIR=/usr/local/bin
        SUDO=sudo
    else
        DIR=$HOME/.local/bin
    fi
elif [ "$(id -u)" != 0 ] && [ -d "$DIR" ] && [ ! -w "$DIR" ] && command -v sudo >/dev/null 2>&1; then
    SUDO=sudo
fi

if [ "$ACTION" = uninstall ]; then
    if [ -e "$DIR/jancox" ]; then
        $SUDO rm -f "$DIR/jancox"
        say "- Removed $DIR/jancox"
    else
        say "- jancox is not installed in $DIR"
    fi
    exit 0
fi

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        die "curl or wget is needed (Termux: pkg install curl; Ubuntu: sudo apt install curl)"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        die "sha256sum is needed to check the download"
    fi
}

TMP=$(mktemp -d 2>/dev/null || mktemp -d -t jancox)
trap 'rm -rf "$TMP"' EXIT INT TERM

ASSET=jancox-$TARGET
if [ -z "$BASE" ]; then
    if [ -n "$VERSION" ]; then
        BASE=https://github.com/$REPO/releases/download/$VERSION
    else
        # newest release (betas included) that has this binary
        fetch "https://api.github.com/repos/$REPO/releases?per_page=20" "$TMP/releases.json" ||
            die "can't reach GitHub; try again or pass --version vX.Y.Z"
        url=$(tr ',' '\n' <"$TMP/releases.json" |
            grep -o "\"browser_download_url\": *\"[^\"]*/$ASSET\"" | head -n1 |
            sed 's/.*"\(https[^"]*\)"$/\1/')
        [ -n "$url" ] || die "no release has $ASSET yet"
        BASE=${url%/*}
    fi
fi
say "- Downloading $ASSET from ${BASE}"
fetch "$BASE/$ASSET" "$TMP/jancox" || die "download failed: $BASE/$ASSET"
fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" || die "download failed: $BASE/SHA256SUMS"

want=$(grep " \*\{0,1\}$ASSET\$" "$TMP/SHA256SUMS" | head -n1 | cut -d' ' -f1)
got=$(sha256 "$TMP/jancox")
[ -n "$want" ] || die "$ASSET is not in SHA256SUMS"
[ "$want" = "$got" ] || die "checksum mismatch for $ASSET (download corrupted?)"

$SUDO mkdir -p "$DIR"
if command -v install >/dev/null 2>&1; then
    $SUDO install -m 755 "$TMP/jancox" "$DIR/jancox"
else
    $SUDO cp "$TMP/jancox" "$DIR/jancox"
    $SUDO chmod 755 "$DIR/jancox"
fi
if ! installed=$("$DIR/jancox" --version 2>/dev/null); then
    $SUDO rm -f "$DIR/jancox"
    die "the downloaded binary does not run here ($TARGET)"
fi
say "- Installed $installed to $DIR/jancox"

case ":$PATH:" in
    *":$DIR:"*) ;;
    *)
        say "- $DIR is not in your PATH. Add it with:"
        say "    echo 'export PATH=\"$DIR:\$PATH\"' >> ~/.bashrc && . ~/.bashrc"
        ;;
esac
say " "
say " Usage: go to any folder and run"
say "   jancox init      (makes input/, output/, jancox.prop)"
say "   jancox unpack    (ROM zip from ./input/)"
say "   jancox repack    (new ROM in ./output/)"
say " More: jancox --help"
