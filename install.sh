#!/bin/sh
# Installs the procd binary for this machine's architecture from the GitHub
# releases. POSIX sh, works with curl or wget (e.g. busybox on Alpine).
#
#   curl -fsSL https://raw.githubusercontent.com/bli-blu-bla/process-deamon/main/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --version v0.2.0 --dir ~/.local/bin
#
# Environment (same as the options): PROCD_VERSION, PROCD_INSTALL_DIR,
# PROCD_REPO (owner/repo), PROCD_BASE_URL (mirror of the releases URL).

set -eu

# GitHub repository the releases are downloaded from.
REPO="${PROCD_REPO:-bli-blu-bla/process-deamon}"
VERSION="${PROCD_VERSION:-latest}"
INSTALL_DIR="${PROCD_INSTALL_DIR:-/usr/local/bin}"

usage() {
    cat <<EOF
Usage: install.sh [--version <tag>] [--dir <path>] [--repo <owner/repo>]

  -v, --version <tag>   Release to install, e.g. v0.2.0 (default: latest)
  -d, --dir <path>      Install directory (default: /usr/local/bin)
  -r, --repo <o/r>      GitHub repository (default: $REPO)
  -h, --help            Show this help
EOF
}

say() { printf 'procd-install: %s\n' "$*"; }
die() { printf 'procd-install: error: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        -v | --version) [ $# -ge 2 ] || die "$1 needs a value"; VERSION="$2"; shift 2 ;;
        -d | --dir) [ $# -ge 2 ] || die "$1 needs a value"; INSTALL_DIR="$2"; shift 2 ;;
        -r | --repo) [ $# -ge 2 ] || die "$1 needs a value"; REPO="$2"; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
done

BASE_URL="${PROCD_BASE_URL:-https://github.com/$REPO/releases}"

# procd needs /proc and process groups, so Linux only. The binaries are
# static musl builds and run on any distribution (glibc or musl).
os=$(uname -s)
[ "$os" = "Linux" ] || die "unsupported OS: $os (procd runs on Linux only)"

case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64 | armv8*) arch=aarch64 ;;
    *) die "unsupported architecture: $(uname -m) (available: x86_64, aarch64)" ;;
esac

asset="procd-$arch-linux-musl"
if [ "$VERSION" = "latest" ]; then
    url="$BASE_URL/latest/download/$asset"
else
    url="$BASE_URL/download/$VERSION/$asset"
fi

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    die "curl or wget is required"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading $asset ($VERSION)"
fetch "$url" "$tmp/$asset" || die "download failed: $url"
fetch "$url.sha256" "$tmp/$asset.sha256" || die "download failed: $url.sha256"

if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp" && sha256sum -c "$asset.sha256" >/dev/null) || die "checksum mismatch for $asset"
elif command -v shasum >/dev/null 2>&1; then
    (cd "$tmp" && shasum -a 256 -c "$asset.sha256" >/dev/null) || die "checksum mismatch for $asset"
else
    say "warning: neither sha256sum nor shasum found, skipping checksum verification"
fi
chmod 755 "$tmp/$asset"

# Use sudo only when the target directory isn't writable (e.g. as non-root
# into /usr/local/bin); in containers this usually runs as root anyway.
sudo=""
if ! mkdir -p "$INSTALL_DIR" 2>/dev/null || [ ! -w "$INSTALL_DIR" ]; then
    command -v sudo >/dev/null 2>&1 || die "$INSTALL_DIR is not writable and sudo is not available (try --dir)"
    sudo="sudo"
    $sudo mkdir -p "$INSTALL_DIR"
fi
# Copy next to the target and rename, so a running procd isn't overwritten
# in place ("text file busy").
$sudo cp "$tmp/$asset" "$INSTALL_DIR/.procd.new"
$sudo mv -f "$INSTALL_DIR/.procd.new" "$INSTALL_DIR/procd"

say "installed $("$INSTALL_DIR/procd" --version) to $INSTALL_DIR/procd"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) say "note: $INSTALL_DIR is not in PATH" ;;
esac
