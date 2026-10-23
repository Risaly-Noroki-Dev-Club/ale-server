#!/usr/bin/env bash
#
# install.sh - install, upgrade or uninstall ale-server on Debian-family Linux.
#
# Run from an unpacked release tarball (the directory that contains bin/,
# config/, systemd/, docs/ and this script):
#
#   sudo ./install.sh                 # system install (default), user "ale"
#   sudo ./install.sh --prefix /usr/local
#   ./install.sh --user               # per-user install, no root required
#   sudo ./install.sh --uninstall     # keep config and state
#   sudo ./install.sh --uninstall --purge
#   ./install.sh --help
#
# The installer is idempotent: binaries and units are refreshed, an existing
# configuration is never overwritten unless --force is passed.
#
# Options:
#   --system        install as a system service (default; requires root)
#   --user          install for the current user (systemd --user, no root)
#   --prefix DIR    install prefix for binaries (default /usr, or ~/.local)
#   --force         overwrite an existing configuration file
#   --uninstall     remove binaries and units (keeps config/state unless --purge)
#   --purge         with --uninstall: also remove configuration, state and the
#                   system user (system mode only)
#   --dry-run       print the actions without changing anything
#   -h, --help      show this help
#   --version       print the payload version and exit
#
# After a system install:
#   sudo systemctl enable --now ale-server
#   sudo journalctl -u ale-server -f      # pairing code is logged at INFO
#
# See docs/DEPLOY.md for backends (grim/wtype/ydotool/xdotool), firewalls,
# pairing and troubleshooting.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

MODE="system"
PREFIX=""
DO_UNINSTALL=0
PURGE=0
FORCE=0
DRY_RUN=0
SHOW_VERSION=0

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

note() {
    printf '==> %s\n' "$*"
}

run() {
    if [[ "$DRY_RUN" -eq 1 ]]; then
        printf '  [dry-run] %s\n' "$*"
    else
        "$@"
    fi
}

run_quiet() {
    if [[ "$DRY_RUN" -eq 1 ]]; then
        printf '  [dry-run] %s\n' "$*"
    else
        "$@" >/dev/null 2>&1 || true
    fi
}

usage() {
    sed -n '3,35p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

payload_version() {
    if [[ -f "$SCRIPT_DIR/VERSION" ]]; then
        tr -d '[:space:]' <"$SCRIPT_DIR/VERSION"
        return 0
    fi
    if [[ -f "$SCRIPT_DIR/../Cargo.toml" ]]; then
        sed -n '/^\[workspace\.package\]/,/^\[/p' "$SCRIPT_DIR/../Cargo.toml" \
            | sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1
        return 0
    fi
    printf 'unknown\n'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --system)
            MODE="system"
            shift
            ;;
        --user)
            MODE="user"
            shift
            ;;
        --prefix)
            [[ $# -ge 2 ]] || die "--prefix requires a directory"
            PREFIX=$2
            shift 2
            ;;
        --force)
            FORCE=1
            shift
            ;;
        --uninstall)
            DO_UNINSTALL=1
            shift
            ;;
        --purge)
            PURGE=1
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --version)
            SHOW_VERSION=1
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

if [[ "$SHOW_VERSION" -eq 1 ]]; then
    printf 'ale-server installer (payload %s)\n' "$(payload_version)"
    exit 0
fi

[[ "$PURGE" -eq 0 || "$DO_UNINSTALL" -eq 1 ]] || die "--purge only makes sense together with --uninstall"

if [[ "$(uname -s)" != "Linux" && "$DRY_RUN" -eq 0 ]]; then
    die "this installer targets Debian-family Linux; use --dry-run to preview on other systems"
fi

if [[ "$MODE" == "system" ]]; then
    [[ -n "$PREFIX" ]] || PREFIX="/usr"
    BIN_DIR="$PREFIX/lib/ale"
    LINK_DIR="$PREFIX/bin"
    CONFIG_DIR="/etc/ale"
    CONFIG_FILE="$CONFIG_DIR/ale-server.toml"
    ENV_FILE="$CONFIG_DIR/ale-server.env"
    UNIT_FILE="/etc/systemd/system/ale-server.service"
    USER_UNIT_FILE="/usr/lib/systemd/user/ale-server.service"
    TMPFILES_FILE="/usr/lib/tmpfiles.d/ale-server.conf"
    STATE_DIR="/var/lib/ale"
else
    [[ -n "$PREFIX" ]] || PREFIX="${XDG_DATA_HOME:-$HOME/.local}"
    BIN_DIR="$PREFIX/bin"
    LINK_DIR="$PREFIX/bin"
    CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/ale"
    CONFIG_FILE="$CONFIG_DIR/ale-server.toml"
    ENV_FILE="$CONFIG_DIR/ale-server.env"
    UNIT_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ale-server.service"
    USER_UNIT_FILE="$UNIT_FILE"
    TMPFILES_FILE=""
    STATE_DIR="${XDG_DATA_HOME:-$HOME/.local}/share/ale"
fi

case "$PREFIX" in
    /*) ;;
    *) die "--prefix must be an absolute path (got '$PREFIX')" ;;
esac

if [[ "$MODE" == "system" && "$DRY_RUN" -eq 0 && "${EUID:-$(id -u)}" -ne 0 ]]; then
    die "a system install modifies /etc and /usr and requires root: use sudo, or --user"
fi

PAYLOAD_DIR="$SCRIPT_DIR"
if [[ "$DO_UNINSTALL" -eq 0 && ! -f "$PAYLOAD_DIR/bin/ale-server" ]]; then
    die "no release payload found in $PAYLOAD_DIR; run this script from an unpacked release tarball"
fi

render_unit() {
    # render_unit SRC DST [sed-expression]
    local src=$1 dst=$2 expr=${3:-}
    [[ -f "$src" ]] || die "missing unit file in payload: $src"
    if [[ "$DRY_RUN" -eq 1 ]]; then
        printf '  [dry-run] install -m 0644 %s %s\n' "$src" "$dst"
        return 0
    fi
    local tmp
    tmp=$(mktemp)
    if [[ -n "$expr" ]]; then
        sed "$expr" "$src" >"$tmp"
    else
        cp "$src" "$tmp"
    fi
    install -m 0644 "$tmp" "$dst"
    rm -f "$tmp"
}

write_env_template() {
    [[ -n "$ENV_FILE" ]] || return 0
    if [[ -f "$ENV_FILE" ]]; then
        return 0
    fi
    if [[ "$DRY_RUN" -eq 1 ]]; then
        printf '  [dry-run] create %s (mode 0600)\n' "$ENV_FILE"
        return 0
    fi
    (
        umask 077
        cat >"$ENV_FILE" <<'ENVEOF'
# Environment for ale-server.service. Read by systemd (root for the system
# unit, the desktop user for the user unit); keep this file mode 0600.
#
# Egress provider key referenced by [egress] api_key_env
# (OPENAI_API_KEY is the default key name in the shipped template):
# OPENAI_API_KEY=sk-...
#
# Optional: use the read-only env credential store for the engine
# (requires [engine] secret_store = "env"):
# ALE_SECRET__API_KEY=sk-...
#
# Optional per-host configuration overrides (double underscore = nesting):
# ALE_SERVER__LOGGING__LEVEL=debug
# ALE_SERVER__DEVICE__AUTOMATION__DRY_RUN=true
ENVEOF
    )
    chmod 0600 "$ENV_FILE"
}

system_install() {
    note "Installing ale-server $VERSION (system mode, prefix $PREFIX)"

    if ! getent passwd ale >/dev/null 2>&1; then
        note "Creating system user and group 'ale'"
        run useradd --system --user-group --home-dir "$STATE_DIR" --no-create-home \
            --shell /usr/sbin/nologin ale
    fi

    note "Creating directories"
    run install -d -m 0755 "$BIN_DIR"
    run install -d -m 0755 "$LINK_DIR"
    run install -d -m 0755 "$CONFIG_DIR"
    run install -d -m 0750 -o ale -g ale "$STATE_DIR"

    note "Installing binaries into $BIN_DIR"
    for name in ale-server ale-modeld; do
        if [[ -f "$PAYLOAD_DIR/bin/$name" ]]; then
            run install -m 0755 "$PAYLOAD_DIR/bin/$name" "$BIN_DIR/$name"
        fi
    done
    run ln -sfn "$BIN_DIR/ale-server" "$LINK_DIR/ale-server"

    if [[ -f "$CONFIG_FILE" && "$FORCE" -eq 0 ]]; then
        warn "keeping existing configuration: $CONFIG_FILE (use --force to overwrite)"
    else
        note "Installing configuration template: $CONFIG_FILE"
        run install -m 0644 "$PAYLOAD_DIR/config/ale-server.toml" "$CONFIG_FILE"
    fi
    write_env_template

    note "Installing systemd units"
    run install -d -m 0755 "$(dirname "$UNIT_FILE")"
    render_unit "$PAYLOAD_DIR/systemd/ale-server.service" "$UNIT_FILE" \
        "s|/usr/lib/ale/ale-server|$BIN_DIR/ale-server|g"
    run install -d -m 0755 "$(dirname "$USER_UNIT_FILE")"
    render_unit "$PAYLOAD_DIR/systemd/ale-server-user.service" "$USER_UNIT_FILE" \
        "s|/usr/lib/ale/ale-server|$BIN_DIR/ale-server|g"
    if [[ -f "$PAYLOAD_DIR/tmpfiles/ale-server.conf" ]]; then
        run install -d -m 0755 "$(dirname "$TMPFILES_FILE")"
        run install -m 0644 "$PAYLOAD_DIR/tmpfiles/ale-server.conf" "$TMPFILES_FILE"
        run_quiet systemd-tmpfiles --create "$TMPFILES_FILE"
    fi

    run_quiet systemctl daemon-reload

    note "Installed. Enable and start the service:"
    printf '    sudo systemctl enable --now ale-server\n'
    printf '    sudo journalctl -u ale-server -f     # pairing code is logged at INFO\n'
    printf 'Validate the configuration with:\n'
    printf '    sudo %s/ale-server --check-config --config %s\n' "$BIN_DIR" "$CONFIG_FILE"
}

user_install() {
    note "Installing ale-server $VERSION (user mode)"
    note "Binary: $BIN_DIR/ale-server"
    note "Config: $CONFIG_FILE"
    note "Unit:   $UNIT_FILE"

    run install -d -m 0755 "$BIN_DIR"
    for name in ale-server ale-modeld; do
        if [[ -f "$PAYLOAD_DIR/bin/$name" ]]; then
            run install -m 0755 "$PAYLOAD_DIR/bin/$name" "$BIN_DIR/$name"
        fi
    done

    run install -d -m 0755 "$CONFIG_DIR"
    if [[ -f "$CONFIG_FILE" && "$FORCE" -eq 0 ]]; then
        warn "keeping existing configuration: $CONFIG_FILE (use --force to overwrite)"
    else
        run install -m 0644 "$PAYLOAD_DIR/config/ale-server.toml" "$CONFIG_FILE"
    fi
    write_env_template

    run install -d -m 0755 "$(dirname "$UNIT_FILE")"
    render_unit "$PAYLOAD_DIR/systemd/ale-server-user.service" "$UNIT_FILE" \
        "s|^ExecStart=.*|ExecStart=$BIN_DIR/ale-server --config $CONFIG_FILE|; s|^EnvironmentFile=.*|EnvironmentFile=-$ENV_FILE|"
    run_quiet systemctl --user daemon-reload

    note "Installed. Enable and start the user service (inside the graphical session):"
    printf '    systemctl --user enable --now ale-server\n'
    printf '    journalctl --user -u ale-server -f\n'
    printf 'For a headless box with a desktop session, keep the user manager alive:\n'
    printf '    loginctl enable-linger %s\n' "${USER:-$(id -un)}"
    printf 'Capture/automation need a live Wayland or X11 session; see docs/DEPLOY.md.\n'
}

system_uninstall() {
    note "Uninstalling ale-server (system mode)"
    run_quiet systemctl disable --now ale-server
    run_quiet systemctl stop ale-server
    run rm -f "$UNIT_FILE"
    run rm -f "$USER_UNIT_FILE"
    run rm -f "${TMPFILES_FILE:-/usr/lib/tmpfiles.d/ale-server.conf}"
    run rm -f "$LINK_DIR/ale-server"
    run rm -f "$BIN_DIR/ale-server" "$BIN_DIR/ale-modeld"
    run_quiet systemctl daemon-reload
    if [[ "$PURGE" -eq 1 ]]; then
        note "Purging configuration, state and system user (--purge)"
        run rm -rf "$CONFIG_DIR" "$STATE_DIR"
        if getent passwd ale >/dev/null 2>&1; then
            run userdel ale || true
        fi
    else
        printf 'Kept: %s and %s (re-run with --uninstall --purge to remove them)\n' "$CONFIG_FILE" "$STATE_DIR"
    fi
}

user_uninstall() {
    note "Uninstalling ale-server (user mode)"
    run_quiet systemctl --user disable --now ale-server
    run rm -f "$UNIT_FILE"
    run rm -f "$BIN_DIR/ale-server" "$BIN_DIR/ale-modeld"
    run_quiet systemctl --user daemon-reload
    if [[ "$PURGE" -eq 1 ]]; then
        note "Purging user configuration (--purge)"
        run rm -rf "$CONFIG_DIR"
    else
        printf 'Kept: %s (re-run with --uninstall --purge to remove it)\n' "$CONFIG_FILE"
    fi
}

VERSION=$(payload_version)
[[ -n "$VERSION" ]] || VERSION="unknown"

if [[ "$MODE" == "system" ]]; then
    if [[ "$DO_UNINSTALL" -eq 1 ]]; then
        system_uninstall
        note "Uninstall complete"
    else
        system_install
    fi
else
    if [[ "$EUID" -eq 0 ]]; then
        warn "running --user as root installs into root's home; run it as the desktop user instead"
    fi
    if [[ "$DO_UNINSTALL" -eq 1 ]]; then
        user_uninstall
        note "Uninstall complete"
    else
        user_install
    fi
fi
