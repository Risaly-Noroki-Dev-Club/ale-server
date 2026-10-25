#!/usr/bin/env bash
#
# build-debian-arm.sh - build ale-server natively on a Debian-family arm64 host
# (Debian 12, Raspberry Pi OS Bookworm, Armbian).
#
# The script checks the host architecture and toolchain, builds with
# `cargo build --release --locked`, strips the binaries, prints `file` output
# and copies the results to the output directory (dist/ by default) where
# scripts/package-tarball.sh --skip-build can pick them up.
#
# Usage:
#   ./scripts/build-debian-arm.sh [--with-modeld] [--out DIR]
#
# Options:
#   --with-modeld   also build ale-modeld with --no-default-features (avoids
#                   downloading the optional sherpa/SenseVoice binaries)
#   --out DIR       output directory (default: <repo>/dist)
#   -h, --help      show this help
#
# Cross-compiling from x86_64? See docs/DEPLOY.md (cargo-zigbuild / cross) and
# let CI produce the release tarball instead.
#
# Native x86_64 Linux host? Use scripts/build-debian-x64.sh instead.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

WITH_MODELD=0
OUT_DIR="$REPO_ROOT/dist"

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

note() {
    printf '==> %s\n' "$*"
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

usage() {
    sed -n '3,21p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --with-modeld)
            WITH_MODELD=1
            shift
            ;;
        --out)
            [[ $# -ge 2 ]] || die "--out requires a directory"
            OUT_DIR=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'error: unknown option: %s\n\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

command -v cargo >/dev/null 2>&1 || die "cargo is not installed (install Rust: https://rustup.rs)"
command -v rustc >/dev/null 2>&1 || die "rustc is not installed (install Rust: https://rustup.rs)"
command -v strip >/dev/null 2>&1 || die "strip is not installed (apt install binutils)"

HOST_ARCH=$(uname -m)
case "$HOST_ARCH" in
    aarch64 | arm64)
        ;;
    *)
        warn "host architecture is '$HOST_ARCH', expected aarch64/arm64."
        warn "The binaries will be built for '$HOST_ARCH' and are not Debian arm64 releases."
        warn "Continue only for a local test build; use CI or a native arm64 host for releases."
        ;;
esac

WORKSPACE_VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$REPO_ROOT/Cargo.toml" \
    | sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
[[ -n "$WORKSPACE_VERSION" ]] || die "could not read the workspace version from Cargo.toml"

TARGET_DIR=${CARGO_TARGET_DIR:-target}
case "$TARGET_DIR" in
    /*) ;;
    *) TARGET_DIR="$REPO_ROOT/$TARGET_DIR" ;;
esac

cd "$REPO_ROOT"

note "ale-server $WORKSPACE_VERSION on $HOST_ARCH ($(rustc --version))"
note "Building ale-server (release, --locked)"
cargo build --release --locked -p ale-server

SERVER_BIN="$TARGET_DIR/release/ale-server"
[[ -f "$SERVER_BIN" ]] || die "expected binary not found: $SERVER_BIN"

MODELD_BIN=""
if [[ "$WITH_MODELD" -eq 1 ]]; then
    note "Building ale-modeld (release, --locked, --no-default-features)"
    cargo build --release --locked -p ale-modeld --no-default-features
    MODELD_BIN="$TARGET_DIR/release/ale-modeld"
    [[ -f "$MODELD_BIN" ]] || die "expected binary not found: $MODELD_BIN"
fi

mkdir -p "$OUT_DIR"

# Strip debug/symbol tables; the release profile already sets strip="symbols",
# this keeps the on-device path small even if the profile changes.
for bin in "$SERVER_BIN" ${MODELD_BIN:+"$MODELD_BIN"}; do
    if ! strip "$bin" 2>/dev/null; then
        warn "strip failed for $bin (leaving it unstripped)"
    fi
done

file_report() {
    local bin=$1
    if command -v file >/dev/null 2>&1; then
        local out
        out=$(file -b "$bin" | tr '\n' ' ')
        printf '    file: %s\n' "$out"
        case "$out" in
            *ARM\ aarch64* | *aarch64* | *arm64*)
                ;;
            *)
                warn "$(basename "$bin") is not an aarch64 executable on this host"
                ;;
        esac
    else
        warn "'file' is not installed; skipping binary format check (apt install file)"
    fi
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

copy_binary() {
    local src=$1
    local name
    name=$(basename "$src")
    install -m 0755 "$src" "$OUT_DIR/$name"
    printf '    %s\n' "$OUT_DIR/$name"
    file_report "$OUT_DIR/$name"
    printf '    sha256: %s\n' "$(sha256_of "$OUT_DIR/$name")"
}

note "Installing binaries into $OUT_DIR"
copy_binary "$SERVER_BIN"
if [[ -n "$MODELD_BIN" ]]; then
    copy_binary "$MODELD_BIN"
fi

note "Build complete (ale-server $WORKSPACE_VERSION)"
note "Next: ./scripts/package-tarball.sh --skip-build"
