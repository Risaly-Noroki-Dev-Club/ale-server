#!/usr/bin/env bash
#
# package-tarball.sh - assemble the Debian arm64 release tarball for ale-server.
#
# Output (in dist/ by default):
#   ale-server-<version>-linux-aarch64.tar.gz
#   ale-server-<version>-linux-aarch64.tar.gz.sha256
#
# The tarball contains:
#   bin/ale-server                stripped release binary (bin/ale-modeld optional)
#   config/ale-server.toml        commented configuration template
#   systemd/ale-server.service    hardened system unit
#   systemd/ale-server-user.service  user-session unit (capture/automation)
#   tmpfiles/ale-server.conf      systemd-tmpfiles rules
#   docs/DEPLOY.md                full deployment guide
#   docs/UPSTREAM.md              vendored ale-core/ale-modeld provenance
#   UPSTREAM_COMMIT               pinned upstream commit (40-char sha)
#   install.sh                    Debian installer/uninstaller
#   README.md, VERSION, LICENSE
#   SHA256SUMS                    checksums of every file above
#
# Usage:
#   ./scripts/package-tarball.sh [options]
#
# Options:
#   --version VERSION  release version (default: workspace version from Cargo.toml)
#   --dist DIR         output directory (default: <repo>/dist)
#   --skip-build       use existing binaries instead of running a native build
#   --bin-dir DIR      directory containing ale-server (and optionally
#                      ale-modeld) for --skip-build; default: auto-detect
#                      target/release and target/aarch64-unknown-linux-gnu/release
#   --with-modeld      pass --with-modeld to build-debian-arm.sh
#   --dry-run          stage the full layout without compiling and without
#                      creating the tarball (safe on any host, incl. macOS)
#   --keep-stage       keep the staged directory after a successful run
#   -h, --help         show this help
#
# Typical on-device release:
#   ./scripts/build-debian-arm.sh --with-modeld
#   ./scripts/package-tarball.sh --skip-build
#
# Cross builds (CI): build aarch64-unknown-linux-gnu binaries, then
#   ./scripts/package-tarball.sh --skip-build --bin-dir target/aarch64-unknown-linux-gnu/release

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

VERSION=""
DIST_DIR="$REPO_ROOT/dist"
SKIP_BUILD=0
DRY_RUN=0
KEEP_STAGE=0
WITH_MODELD=0
BIN_DIR=""

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
    sed -n '3,43p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

abs_path() {
    case "$1" in
        /*) printf '%s\n' "$1" ;;
        *) printf '%s/%s\n' "$REPO_ROOT" "$1" ;;
    esac
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)
            [[ $# -ge 2 ]] || die "--version requires a value"
            VERSION=$2
            shift 2
            ;;
        --dist)
            [[ $# -ge 2 ]] || die "--dist requires a directory"
            DIST_DIR=$(abs_path "$2")
            shift 2
            ;;
        --skip-build)
            SKIP_BUILD=1
            shift
            ;;
        --bin-dir)
            [[ $# -ge 2 ]] || die "--bin-dir requires a directory"
            BIN_DIR=$(abs_path "$2")
            shift 2
            ;;
        --with-modeld)
            WITH_MODELD=1
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --keep-stage)
            KEEP_STAGE=1
            shift
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

if [[ -n "$BIN_DIR" && "$SKIP_BUILD" -eq 0 ]]; then
    die "--bin-dir requires --skip-build"
fi

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

workspace_version() {
    sed -n '/^\[workspace\.package\]/,/^\[/p' "$REPO_ROOT/Cargo.toml" \
        | sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1
}

if [[ -z "$VERSION" ]]; then
    VERSION=$(workspace_version)
    [[ -n "$VERSION" ]] || die "could not read the workspace version from Cargo.toml; pass --version"
fi
case "$VERSION" in
    *[!A-Za-z0-9._+-]*)
        die "invalid version '$VERSION' (allowed: A-Z a-z 0-9 . _ + -)"
        ;;
esac

STAGE_NAME="ale-server-${VERSION}-linux-aarch64"
STAGE_DIR="$DIST_DIR/$STAGE_NAME"
TARBALL="$DIST_DIR/$STAGE_NAME.tar.gz"
TARBALL_SUM="$TARBALL.sha256"

PAYLOAD_FILES=(
    "deploy/config/ale-server.toml:config/ale-server.toml"
    "deploy/systemd/ale-server.service:systemd/ale-server.service"
    "deploy/systemd/ale-server-user.service:systemd/ale-server-user.service"
    "deploy/tmpfiles/ale-server.conf:tmpfiles/ale-server.conf"
    "scripts/install.sh:install.sh"
    "README.md:README.md"
    "LICENSE:LICENSE"
    "docs/DEPLOY.md:docs/DEPLOY.md"
    "docs/BROWSER-TASKS.md:docs/BROWSER-TASKS.md"
    "docs/API.md:docs/API.md"
    "docs/UPSTREAM.md:docs/UPSTREAM.md"
    "UPSTREAM_COMMIT:UPSTREAM_COMMIT"
)

copy_payload() {
    local mapping src dst
    for mapping in "${PAYLOAD_FILES[@]}"; do
        src=${mapping%%:*}
        dst=${mapping#*:}
        [[ -f "$REPO_ROOT/$src" ]] || die "missing packaging input: $src"
        mkdir -p "$STAGE_DIR/$(dirname "$dst")"
        if [[ "$src" == *.sh ]]; then
            install -m 0755 "$REPO_ROOT/$src" "$STAGE_DIR/$dst"
        else
            install -m 0644 "$REPO_ROOT/$src" "$STAGE_DIR/$dst"
        fi
    done
    printf '%s\n' "$VERSION" >"$STAGE_DIR/VERSION"
    chmod 0644 "$STAGE_DIR/VERSION"
}

find_binary() {
    local name=$1
    if [[ -n "$BIN_DIR" ]]; then
        if [[ -f "$BIN_DIR/$name" ]]; then
            printf '%s\n' "$BIN_DIR/$name"
            return 0
        fi
        return 1
    fi
    local candidate
    for candidate in \
        "$REPO_ROOT/target/release/$name" \
        "$REPO_ROOT/target/aarch64-unknown-linux-gnu/release/$name"; do
        if [[ -f "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

file_report() {
    local bin=$1 out
    if command -v file >/dev/null 2>&1; then
        out=$(file -b "$bin" | tr '\n' ' ')
        printf '    file: %s\n' "$out"
        case "$out" in
            *ARM\ aarch64* | *aarch64* | *arm64*) ;;
            *)
                warn "$(basename "$bin") is not an aarch64 ELF; this tarball is not a Debian arm64 release"
                ;;
        esac
    fi
}

stage_binaries() {
    mkdir -p "$STAGE_DIR/bin"
    local server
    if [[ "$DRY_RUN" -eq 1 ]]; then
        printf '    (dry run: binaries are not staged)\n'
        return 0
    fi
    if [[ "$SKIP_BUILD" -eq 1 ]]; then
        server=$(find_binary ale-server) || die "ale-server binary not found; run scripts/build-debian-arm.sh first or pass --bin-dir"
    else
        note "Building release binaries natively (scripts/build-debian-arm.sh)"
        local build_args=("--out" "$STAGE_DIR/bin")
        [[ "$WITH_MODELD" -eq 1 ]] && build_args+=("--with-modeld")
        "$SCRIPT_DIR/build-debian-arm.sh" "${build_args[@]}"
        server="$STAGE_DIR/bin/ale-server"
    fi
    if [[ "$SKIP_BUILD" -eq 1 ]]; then
        install -m 0755 "$server" "$STAGE_DIR/bin/ale-server"
    fi
    [[ -f "$STAGE_DIR/bin/ale-server" ]] || die "staging failed: bin/ale-server is missing"
    printf '    bin/ale-server\n'
    file_report "$STAGE_DIR/bin/ale-server"

    local modeld
    if [[ -f "$STAGE_DIR/bin/ale-modeld" ]]; then
        printf '    bin/ale-modeld\n'
        file_report "$STAGE_DIR/bin/ale-modeld"
    elif modeld=$(find_binary ale-modeld); then
        install -m 0755 "$modeld" "$STAGE_DIR/bin/ale-modeld"
        printf '    bin/ale-modeld\n'
        file_report "$STAGE_DIR/bin/ale-modeld"
    elif [[ "$WITH_MODELD" -eq 1 ]]; then
        die "--with-modeld was requested but ale-modeld was not built"
    else
        printf '    (ale-modeld not included; pass --with-modeld to build it)\n'
    fi
}

generate_sha256sums() {
    local sums="$STAGE_DIR/SHA256SUMS" f
    : >"$sums"
    (
        cd "$STAGE_DIR"
        find . -type f ! -name SHA256SUMS | sort | while IFS= read -r f; do
            printf '%s  %s\n' "$(sha256_of "$f")" "${f#./}"
        done
    ) >"$sums"
    chmod 0644 "$sums"
}

list_layout() {
    printf 'Staged layout: %s\n' "$STAGE_DIR"
    (
        cd "$STAGE_DIR"
        find . -mindepth 1 | sort | sed 's|^\./||' | while IFS= read -r path; do
            if [[ -d "$path" ]]; then
                printf '  %s/\n' "$path"
            else
                printf '  %-48s %10s bytes\n' "$path" "$(wc -c <"$path" | tr -d ' ')"
            fi
        done
    )
}

[[ -d "$REPO_ROOT/deploy" ]] || die "run this script from the ale-server repository"
mkdir -p "$DIST_DIR"
rm -rf "$STAGE_DIR"

note "ale-server $VERSION ($STAGE_NAME)"
note "Staging release layout in $STAGE_DIR"
mkdir -p "$STAGE_DIR"
stage_binaries
copy_payload
note "Generating SHA256SUMS"
generate_sha256sums

if [[ "$DRY_RUN" -eq 1 ]]; then
    list_layout
    note "Dry run complete: layout staged, no tarball written"
    note "Re-run without --dry-run to build/package, or use --skip-build with --bin-dir"
    exit 0
fi

note "Creating $TARBALL"
rm -f "$TARBALL" "$TARBALL_SUM"
COPYFILE_DISABLE=1 tar -czf "$TARBALL" -C "$DIST_DIR" "$STAGE_NAME"

TARBALL_HASH=$(sha256_of "$TARBALL")
printf '%s  %s\n' "$TARBALL_HASH" "$(basename "$TARBALL")" >"$TARBALL_SUM"

if [[ "$KEEP_STAGE" -eq 1 ]]; then
    note "Keeping staged directory: $STAGE_DIR"
else
    rm -rf "$STAGE_DIR"
fi

note "Release artifact: $TARBALL"
note "SHA256: $TARBALL_HASH"
note "Checksum file: $TARBALL_SUM"
note "Install on the target: tar -xzf $(basename "$TARBALL") && cd $STAGE_NAME && sudo ./install.sh --system"
