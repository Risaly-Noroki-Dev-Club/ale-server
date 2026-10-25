#!/usr/bin/env bash
# Build ale-server natively on Debian/Ubuntu x86_64 (Linux x64).
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
TARGET=x86_64-unknown-linux-gnu
WITH_MODELD=0
OUT_DIR="$REPO_ROOT/dist/linux-x86_64"

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

note() {
    printf '==> %s\n' "$*"
}

usage() {
    printf '%s\n' \
        'Build ale-server natively on Debian/Ubuntu x86_64 (Linux x64).' \
        'Usage: ./scripts/build-debian-x64.sh [--with-modeld] [--out DIR]' \
        '' \
        '  --with-modeld  Also build ale-modeld with --no-default-features.' \
        '  --out DIR      Output directory (default: <repo>/dist/linux-x86_64).' \
        '                 Relative paths are resolved from the current directory.' \
        '  -h, --help     Show this help.' \
        '' \
        'Requires a Linux x86_64 host, Rust stable, binutils and file.' \
        'See docs/DEPLOY.md for build dependencies.'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --with-modeld)
            WITH_MODELD=1
            shift
            ;;
        --out)
            [[ $# -ge 2 && -n "$2" && "$2" != -* ]] || die "--out requires a directory"
            OUT_DIR=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            die "unknown option: $1"
            ;;
    esac
done

[[ "$(uname -s)" == Linux ]] || die "run this script on Linux x86_64; cross-compilation is not supported"
case "$(uname -m)" in
    x86_64 | amd64) ;;
    *) die "run this script on an x86_64/amd64 host" ;;
esac

for tool in cargo rustc strip file sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || die "required tool is missing: $tool (see docs/DEPLOY.md)"
done

# Resolve output before changing directory; Cargo paths are relative to the repo.
case "$OUT_DIR" in
    /*) ;;
    *) OUT_DIR="$PWD/$OUT_DIR" ;;
esac
TARGET_DIR=${CARGO_TARGET_DIR:-target}
case "$TARGET_DIR" in
    /*) ;;
    *) TARGET_DIR="$REPO_ROOT/$TARGET_DIR" ;;
esac

cd "$REPO_ROOT"
note "Building for $TARGET ($(rustc --version))"
# An explicit target prevents CARGO_BUILD_TARGET or Cargo config from selecting ARM.
cargo build --release --locked --target "$TARGET" -p ale-server
BINARIES=(ale-server)
if [[ "$WITH_MODELD" -eq 1 ]]; then
    cargo build --release --locked --target "$TARGET" -p ale-modeld --no-default-features
    BINARIES+=(ale-modeld)
fi

# Validate every requested binary before copying any output.
for name in "${BINARIES[@]}"; do
    bin="$TARGET_DIR/$TARGET/release/$name"
    [[ -f "$bin" ]] || die "expected binary not found: $bin"
    description=$(LC_ALL=C file -b "$bin")
    case "$description" in
        *ELF\ 64-bit*x86-64*) ;;
        *) die "$name is not an x86_64 ELF executable: $description" ;;
    esac
done

mkdir -p "$OUT_DIR"
for name in "${BINARIES[@]}"; do
    bin="$TARGET_DIR/$TARGET/release/$name"
    strip "$bin"
    # Permit --out to point at Cargo's own release directory.
    if [[ ! "$bin" -ef "$OUT_DIR/$name" ]]; then
        install -m 0755 "$bin" "$OUT_DIR/$name"
    fi
    file "$OUT_DIR/$name"
    sha256sum "$OUT_DIR/$name"
done

note "Build complete: $OUT_DIR"
note "Run: $OUT_DIR/ale-server --config $REPO_ROOT/deploy/config/ale-server.toml"
note "package-tarball.sh currently produces arm64 packages only; keep these x64 binaries separate."
