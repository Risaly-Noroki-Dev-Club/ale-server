# Deploying `ale-server` on Debian arm64

Complete guide for running the headless Ale, My Eyes! backend on Debian 12
(Bookworm) arm64, Raspberry Pi OS Bookworm 64-bit and Armbian arm64 boards.
Every command is copy-pasteable; paths assume a system install with the release
tarball produced by this repository.

Tested target: **Debian 12 arm64**. The same instructions work on Ubuntu 24.04
arm64 and on x86_64 for development, with the architecture name adjusted.

---

## Contents

1. [Supported systems and hardware](#supported-systems-and-hardware)
2. [Build dependencies](#build-dependencies)
3. [Rust toolchain](#rust-toolchain)
4. [Building on arm64](#building-on-arm64)
5. [Cross-compiling from x86_64](#cross-compiling-from-x86_64)
6. [Packaging the release tarball](#packaging-the-release-tarball)
7. [Configuration](#configuration)
8. [Installation](#installation)
9. [System vs user service](#system-vs-user-service)
10. [Capture and automation backends](#capture-and-automation-backends)
11. [Network and firewall](#network-and-firewall)
12. [Pairing a terminal](#pairing-a-terminal)
13. [Upgrades](#upgrades)
14. [Uninstall](#uninstall)
15. [Troubleshooting](#troubleshooting)
16. [Hardening notes](#hardening-notes)
17. [Files, paths and environment variables](#files-paths-and-environment-variables)

---

## Supported systems and hardware

| System | Status | Notes |
| --- | --- | --- |
| Debian 12 (Bookworm) arm64 | primary target | systemd 252, glibc 2.36 |
| Raspberry Pi OS Bookworm 64-bit | primary target | Pi 4 / Pi 5, CM4 |
| Armbian (Bookworm/Noble based) arm64 | supported | Orange Pi, Rock Pi, Odroid, … |
| Ubuntu 24.04 arm64 | supported | same package names |
| Debian/Ubuntu x86_64 | development only | useful for the HTTP API; capture/automation are architecture-independent commands |

Hardware notes:

* Use a **64-bit OS** (`getconf LONG_BIT` must print `64`). 32-bit Raspberry Pi
  OS cannot run these binaries.
* A Raspberry Pi 4 with 4 GB RAM is the practical minimum; 8 GB is comfortable
  if local models are used through `ale-modeld`. The Pi 5 is noticeably faster.
* Use an SSD or a fast A2 SD card: Rust release binaries and local model files
  are large and capture workloads create temporary files.
* Screen capture and automation require a **live graphical session** on the
  machine being controlled (Wayland or X11) - a headless board without any
  display server still serves the HTTP API, pairing and egress, but capture
  requests return `CAPABILITY_UNAVAILABLE`.
* No GPU is required for cloud models. If you run local models, a board with a
  supported GPU/NPU is preferable.

---

## Build dependencies

Install the build toolchain and the only native library the vendored core needs
(DBus for the OS keyring):

```bash
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    cmake \
    clang \
    libdbus-1-dev \
    file \
    curl \
    ca-certificates
```

Notes:

* **No OpenSSL development package is required.** All HTTP clients use rustls
  (`reqwest` with `rustls-tls`), and the device channel uses
  `snow`/ChaChaPoly. Do not install `libssl-dev` for this service.
* `cmake` and `clang` are needed by transitive Rust build scripts (`ring`,
  `aws-lc-rs` and friends on some platforms) - install them proactively, they
  are small.
* `libdbus-1-dev` is required by the vendored `ale-core` secret store; the
  service itself does not open a session bus.
* `file` is used by the build/package scripts to verify the produced binaries.

Optional runtime packages, only for the features you enable:

```bash
# Wayland screen capture + display discovery + typing
sudo apt-get install -y grim wlr-randr wtype slurp
# Wayland pointer/keyboard injection (needs /dev/uinput)
sudo apt-get install -y ydotool
# X11 capture + automation
sudo apt-get install -y xdotool x11-utils scrot
# Alternative capture backends (any one is enough)
sudo apt-get install -y imagemagick        # provides `import`
sudo apt-get install -y gnome-screenshot   # GNOME only
sudo apt-get install -y spectacle          # KDE Plasma only
```

The service selects the first available backend at runtime; installing more
than one is fine. See [Capture and automation backends](#capture-and-automation-backends).

---

## Rust toolchain

Debian 12 ships Rust 1.63, which is too old for this workspace. Install the
current stable toolchain with rustup:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
rustc --version
cargo --version
```

A current stable Rust (1.80+) is expected. The workspace `Cargo.lock` is
committed and every build uses `--locked`, so toolchain upgrades are the only
moving part on the build machine.

If you build as `root` (not recommended), set `RUSTUP_HOME`/`CARGO_HOME`
explicitly or build as a normal user and install the resulting tarball with
`sudo`.

---

## Building on arm64

Build on the device itself (or on another arm64 machine with the same glibc
generation as the target - Debian 12 arm64 binaries run on Raspberry Pi OS
Bookworm):

```bash
sudo apt-get install -y git          # to fetch the source
git clone https://github.com/Risaly-Noroki-Dev-Club/ale-server.git
cd ale-server

# Native release build; --with-modeld also builds ale-modeld without the
# optional sherpa/SenseVoice download (recommended on arm64)
./scripts/build-debian-arm.sh --with-modeld
```

The script:

* checks the host architecture (`uname -m` should be `aarch64`/`arm64`) and
  warns if you are on another architecture;
* verifies `cargo`, `rustc` and `strip` are available;
* runs `cargo build --release --locked -p ale-server` (and
  `cargo build --release --locked -p ale-modeld --no-default-features` with
  `--with-modeld`);
* strips the binaries, prints `file` output and their SHA-256;
* copies the results to `dist/`.

A first build on a Pi 4 takes roughly 15-30 minutes. Build inside `tmux`/`screen`
over SSH so a dropped connection does not kill it.

To validate the build before installing:

```bash
file dist/ale-server
# ELF 64-bit LSB pie executable, ARM aarch64, ... stripped
dist/ale-server --version 2>/dev/null || true
```

---

## Cross-compiling from x86_64

The workspace has no X11/Wayland/audio link dependencies, so cross-compilation
for `aarch64-unknown-linux-gnu` is straightforward. Pick one method:

### Method A: cargo-zigbuild (fast, no Docker)

```bash
# zig provides the aarch64 linker and a recent glibc sysroot
sudo apt-get install -y pipx && pipx install ziglang
pipx ensurepath && exec "$SHELL"
cargo install --locked cargo-zigbuild
rustup target add aarch64-unknown-linux-gnu
cargo zigbuild --release --locked -p ale-server --target aarch64-unknown-linux-gnu
```

`ziglang` is also packaged by `pip install ziglang`; any zig >= 0.12 works.
Binaries land in `target/aarch64-unknown-linux-gnu/release/`.

### Method B: `cross` (Docker)

```bash
cargo install --locked cross
rustup target add aarch64-unknown-linux-gnu
cross build --release --locked -p ale-server --target aarch64-unknown-linux-gnu
```

Requires Docker on the host; the cross image provides the C toolchain and
`cmake`/`clang`.

### Method C: use CI artifacts (easiest)

Every green CI run of `.github/workflows/build.yml` cross-compiles
`ale-server` for `aarch64-unknown-linux-gnu` and uploads
`ale-server-<version>-linux-aarch64.tar.gz` plus its `.sha256` file. Download
the artifact, copy it to the device and continue with
[Installation](#installation).

Verify any cross-built binary before shipping it:

```bash
file target/aarch64-unknown-linux-gnu/release/ale-server
# must contain: ELF 64-bit LSB ... ARM aarch64
```

Note: `ale-modeld` is best built natively on the target because of the optional
`sherpa-rs` binary download; with `--with-modeld`, the build script uses
`--no-default-features` and skips that dependency.

---

## Packaging the release tarball

```bash
# On arm64 after a native build
./scripts/package-tarball.sh --skip-build

# Or let the script build first (native arm64 only)
./scripts/package-tarball.sh --with-modeld

# Cross-built binaries
./scripts/package-tarball.sh --skip-build \
    --bin-dir target/aarch64-unknown-linux-gnu/release

# Stage the layout without compiling or creating a tarball (any host)
./scripts/package-tarball.sh --dry-run
```

Output:

```
dist/ale-server-<version>-linux-aarch64.tar.gz
dist/ale-server-<version>-linux-aarch64.tar.gz.sha256
```

Tarball layout:

```
ale-server-<version>-linux-aarch64/
├── bin/ale-server              # stripped aarch64 binary
├── bin/ale-modeld              # optional
├── config/ale-server.toml      # commented configuration template
├── systemd/ale-server.service
├── systemd/ale-server-user.service
├── tmpfiles/ale-server.conf
├── docs/DEPLOY.md
├── install.sh
├── README.md
├── LICENSE
├── VERSION
└── SHA256SUMS
```

Always verify a copied artifact before installing:

```bash
sha256sum -c ale-server-0.2.0-linux-aarch64.tar.gz.sha256
```

---

## Configuration

| Scope | Path |
| --- | --- |
| configuration | `/etc/ale/ale-server.toml` (system) |
| secrets/environment | `/etc/ale/ale-server.env` (system, mode 0600 root) |
| state | `/var/lib/ale/` (system, owned by `ale`) |
| binaries | `/usr/lib/ale/` + `/usr/bin/ale-server` symlink (system) |
| engine config | `~/.config/ale-my-eyes/config.json` (resolved from `HOME`; for the system unit `HOME=/var/lib/ale`) |

Start from the template:

```bash
sudo install -d -m 0755 /etc/ale
sudo install -m 0644 deploy/config/ale-server.toml /etc/ale/ale-server.toml
sudoedit /etc/ale/ale-server.toml
```

A minimal configuration that exposes the device channel and leaves the cloud
engine optional:

```toml
[server]
bind = "0.0.0.0:8080"

[engine]
enabled = true
# config_path = "/var/lib/ale/.config/ale-my-eyes/config.json"

[device]
enabled = true
bind = "0.0.0.0:37655"

[device.pairing]
code_ttl_s = 300

[device.capture]
max_width = 1600

[device.automation]
dry_run = false
allow_high_risk = false
allow_process_actions = false

[egress]
enabled = false
allowed_hosts = ["api.openai.com"]

[logging]
level = "info"
format = "text"
```

Key sections (full comments are in the template):

* `[server]`: HTTP bind (default `0.0.0.0:8080`) and an optional CORS origin
  allow-list (`cors_origin`, a TOML list; empty disables CORS, `*` is rejected).
* `[engine]`: cloud ASR/TTS/VLM configuration, or `enabled = false` for a
  pure capture/egress node. Engine startup failures never crash the service;
  inference routes return HTTP 503 `ENGINE_UNAVAILABLE` while every other route
  keeps working. `secret_store` selects the credential backend: `auto`
  (keyring, falling back to a 0600 file), `system` (D-Bus Secret Service),
  `file` (JSON, default `/var/lib/ale/secrets.json` thanks to
  `StateDirectory=ale`) or `env` (read-only `ALE_SECRET__API_KEY`,
  `ALE_SECRET__BACKUP_API_KEY`, `ALE_SECRET__TRANSCRIPTION_API_KEY`).
  `secret_store_path` overrides the file location. On headless hosts prefer
  `secret_store = "file"`: the `auto` probe may spend up to 10 seconds trying
  the D-Bus Secret Service before falling back.
* `[device]`: device channel bind (default `0.0.0.0:37655`), pairing TTL and
  rate limits, capture (backend, max width, JPEG quality, timeout, cache),
  automation (backend, risk/process policy, dry-run, timeouts, stale-frame
  guard) and connection/request limits.
* `[egress]`: allow-listed cloud model proxy for paired terminals. Defaults:
  `upstream = "https://api.openai.com"` (bare base URL, no `/v1` path),
  `allowed_hosts = ["api.openai.com"]`, `api_key_env = "OPENAI_API_KEY"`.
  When `enabled = true` the upstream host must be listed in `allowed_hosts`,
  `allowed_hosts` must not be empty and the API key variable must be set in the
  service environment. The key never appears in TOML or logs.
* `[logging]`: `level` (`trace|debug|info|warn|error`) and `format`
  (`text|json`).

Configuration rules:

* `[device]` uses `deny_unknown_fields` - a typo is a startup error, not a
  silently ignored setting.
* Every key can be overridden by an environment variable using
  `ALE_SERVER__SECTION__KEY` (double underscore between levels):
  `ALE_SERVER__SERVER__BIND=127.0.0.1:8080`,
  `ALE_SERVER__DEVICE__PAIRING__CODE_TTL_S=600`,
  `ALE_SERVER__DEVICE__AUTOMATION__DRY_RUN=true`,
  `ALE_SERVER__LOGGING__LEVEL=debug`. List values are comma-separated:
  `ALE_SERVER__EGRESS__ALLOWED_HOSTS=api.openai.com,api.example.com`,
  `ALE_SERVER__SERVER__CORS_ORIGIN=https://a.example.lan`.
* `[server] bind` and `[device] bind` must differ (they are different ports by
  default); sharing one socket address is rejected at startup.
* Put secrets in the environment, not in TOML. The system unit reads
  `/etc/ale/ale-server.env` (root-owned, mode 0600):

  ```bash
  sudo install -m 0600 -o root -g root /dev/null /etc/ale/ale-server.env
  sudo sh -c 'printf "OPENAI_API_KEY=sk-...\n" >> /etc/ale/ale-server.env'
  ```

* Validate before starting anything:

  ```bash
  sudo /usr/lib/ale/ale-server --check-config --config /etc/ale/ale-server.toml
  ```

---

## Installation

From an unpacked release tarball (after `sha256sum -c`):

```bash
tar -xzf ale-server-0.2.0-linux-aarch64.tar.gz
cd ale-server-0.2.0-linux-aarch64

# System service (default): creates the `ale` user, installs
# /usr/lib/ale/ale-server, /etc/ale/ale-server.toml and the systemd unit.
sudo ./install.sh --system

# Custom binary prefix (default /usr -> /usr/lib/ale)
sudo ./install.sh --system --prefix /usr/local

# Per-user install (systemd --user, no root); use inside the desktop session
./install.sh --user

# Preview every action without changing the system
sudo ./install.sh --system --dry-run
```

The installer is idempotent:

* an existing `/etc/ale/ale-server.toml` is **kept** (use `--force` to replace
  it after reviewing the new template);
* `/etc/ale/ale-server.env` is created only when missing;
* binaries and units are refreshed, `systemctl daemon-reload` is run and
  tmpfiles rules are applied;
* the systemd unit's `ExecStart` is rewritten when `--prefix` is not `/usr`.

Then:

```bash
sudo systemctl enable --now ale-server
systemctl status ale-server
sudo journalctl -u ale-server -f
```

Expected journal lines include the bind addresses and the pairing code at
`INFO`. Smoke-test the API:

```bash
curl -s http://127.0.0.1:8080/health
curl -s http://127.0.0.1:8080/status
```

`/health` is intentionally reachable without authentication; keep port 8080 on
the trusted network (see [Network and firewall](#network-and-firewall)).

For the user-mode install:

```bash
systemctl --user enable --now ale-server
journalctl --user -u ale-server -f
# keep the user manager (and therefore the service) alive after logout:
loginctl enable-linger "$USER"
```

Check the sandbox score of the installed unit:

```bash
systemd-analyze security ale-server
```

---

## System vs user service

`deploy/systemd/` ships two units; they run the same binary with different
privileges and environment:

| | System unit (`ale-server.service`) | User unit (`ale-server-user.service`) |
| --- | --- | --- |
| Managed by | system manager (`sudo systemctl …`) | user manager (`systemctl --user …`) |
| Runs as | dedicated `ale` user | your desktop user |
| Session access | none (starts before/without login) | inherits the graphical session |
| Capture | needs a display it cannot reach by default | works (`grim`, X11 tools) |
| Automation | blocked by `PrivateDevices`/no session | works (`xdotool`, `ydotool`, `wtype`) |
| Hardening | maximum (`ProtectSystem=strict`, seccomp, no caps) | reduced subset (needs `/dev/uinput` and `$HOME`) |
| Best for | API/engine/egress-only nodes, servers | the computer whose screen is controlled |

Install **both** on a desktop-class machine if you want the API/egress service
to run independently of the login session, but note that only one process can
bind `8080` and `37655`. Usually you install one and keep the distinction in
mind:

* **Headless server** (no display): system unit.
* **Desktop being controlled** (Wayland/X11 session): user unit. Optionally
  install the system unit too, after moving the API/device ports of one of them
  (for example set the user unit's config `[server] bind = "127.0.0.1:8081"`
  and `[device] enabled = false` for one of the two - they must not share
  ports).

If you *must* run the system unit with a session on a kiosk-style machine, add
a drop-in with the session environment and disable `PrivateDevices` (needed for
`ydotool`):

```bash
sudo systemctl edit ale-server
```

```ini
[Service]
# UID of the auto-login desktop user, e.g. 1000
Environment=XDG_RUNTIME_DIR=/run/user/1000
Environment=XDG_SESSION_TYPE=wayland
Environment=WAYLAND_DISPLAY=wayland-0
# X11 alternative:
# Environment=DISPLAY=:0
# Environment=XAUTHORITY=/home/pi/.Xauthority
PrivateDevices=no
```

The `ale` user must be allowed into the session's runtime directory (on many
systems `/run/user/1000` is mode 0700 and belongs to the desktop user - an ACL
`setfacl -m u:ale:x /run/user/1000` is required, and Wayland sockets may still
be inaccessible). This is why the user unit is the recommended path for capture.

---

## Capture and automation backends

The service never links X11/Wayland libraries: capture and input are executed
through external commands, selected at runtime or forced in the configuration:

```toml
[device.capture]
# preferred_backend = "grim"     # or spectacle, gnome-screenshot, import, scrot

[device.automation]
# preferred_backend = "xdotool"  # or ydotool, dotool, wtype
```

### Session detection

The backends use the environment of the service process:

* Wayland: `XDG_SESSION_TYPE=wayland`, `WAYLAND_DISPLAY=wayland-0`,
  `XDG_RUNTIME_DIR=/run/user/<uid>` must be set.
* X11: `XDG_SESSION_TYPE=x11`, `DISPLAY=:0` (or `:1`), `XAUTHORITY` pointing at
  the session's Xauthority file.

The user unit inherits these from the systemd user manager. If they are missing
in `journalctl --user -u ale-server`, add them to
`~/.config/environment.d/ale.conf` (then re-login) or as `Environment=` lines in
a unit drop-in.

### Wayland

```bash
sudo apt-get install -y grim wlr-randr
# screenshot test - must produce a PNG
grim /tmp/ale-test.png && file /tmp/ale-test.png
```

* **Capture**: `grim` is the primary Wayland backend. On wlroots compositors,
  `wlr-randr` (or `swaymsg -t get_outputs` under sway) supplies display
  geometry. GNOME/KDE Wayland sessions work for capture via `grim` when the
  compositor exposes the screencopy protocol (GNOME usually routes this through
  the portal; if `grim` fails, install `gnome-screenshot`).
* **Typing**: `wtype` uses the virtual-keyboard protocol and needs no special
  permissions:
  `sudo apt-get install -y wtype`.
* **Pointer + keyboard**: `ydotool` (or `dotool`) injects events through
  `/dev/uinput`. Prepare the kernel module and permissions once:

  ```bash
  sudo modprobe uinput
  echo uinput | sudo tee /etc/modules-load.d/uinput.conf
  # /dev/uinput must be writable by the desktop user:
  sudo tee /etc/udev/rules.d/60-ale-uinput.rules >/dev/null <<'EOF'
  KERNEL=="uinput", GROUP="input", MODE="0660"
  EOF
  sudo udevadm control --reload-rules && sudo udevadm trigger
  sudo usermod -aG input "$USER"     # log out/in (or reboot) for group changes
  ```

  Verify:

  ```bash
  ls -l /dev/uinput                        # group input, mode 0660
  id | tr ' ' '\n' | grep -x 'input'       # current session has the group
  ydotool --help
  ```

  `ydotool` 1.x uses the `ydotoold` daemon and a socket; enable and start it as
  a user service (package: `ydotoold.service`), or use `dotool`, which talks to
  `/dev/uinput` directly. `ydotool` 0.1.x (Debian 12) accesses `/dev/uinput`
  itself and needs no daemon.

  The **system unit cannot use ydotool/dotool** because `PrivateDevices=yes`
  hides `/dev/uinput`; use the user unit (or the drop-in above with
  `PrivateDevices=no`).

### X11

```bash
sudo apt-get install -y xdotool x11-utils
# test inside the session that owns the display
DISPLAY=:0 xdotool getactivewindow
```

* **Capture**: `scrot`, ImageMagick `import`, `gnome-screenshot`, `spectacle`
  depending on the desktop. `xrandr` (package `x11-xserver-utils`) supplies
  display geometry.
* **Automation**: `xdotool` moves/clicks/types. Ensure `DISPLAY` and,
  when the service user differs from the session user, `XAUTHORITY` are
  available. On X11 the window manager's security model may block synthetic
  events from other users' sessions - another reason to use the user unit.

### Verify the backend end-to-end

With `[device.automation] dry_run = true` first, pair a client and issue a
capture request. The journal reports the selected backend and any
`CAPABILITY_UNAVAILABLE`/`BACKEND_ERROR` reason. Common verification commands:

```bash
journalctl -u ale-server -n 100 --no-pager
journalctl --user -u ale-server -n 100 --no-pager
```

---

## Network and firewall

| Port | Protocol | Default | Exposure |
| --- | --- | --- | --- |
| 8080 | HTTP | `0.0.0.0:8080` | health/status/models/asr/tts/vlm + egress proxy. API routes have no authentication - keep on the LAN/VPN |
| 37655 | WebSocket | `0.0.0.0:37655` | Noise-encrypted, pairing-authenticated; safe on the LAN, still better firewalled |

Recommendations:

* Keep both ports on the trusted LAN or behind a VPN (WireGuard/Tailscale).
* **Never port-forward 8080 or 37655 to the internet.**
* If the HTTP API must cross an untrusted network, terminate TLS with a reverse
  proxy (Caddy, nginx) and expose only the proxy, or use an SSH tunnel.
* Route the egress proxy through [egress] `allowed_hosts`; the terminal never
  receives provider credentials.

nftables example (allow only the LAN, everything else dropped):

```bash
sudo tee /etc/nftables.d/ale.conf >/dev/null <<'EOF'
table inet ale {
  chain input {
    type filter hook input priority filter; policy drop;
    ct state established,related accept
    iif lo accept
    ip saddr 192.168.1.0/24 tcp dport { 8080, 37655 } accept
  }
}
EOF
sudo systemctl restart nftables
```

With ufw:

```bash
sudo ufw allow from 192.168.1.0/24 to any port 8080 proto tcp
sudo ufw allow from 192.168.1.0/24 to any port 37655 proto tcp
sudo ufw enable
```

Change the bind addresses to `127.0.0.1` when a reverse proxy or SSH tunnel is
the only intended path:

```toml
[server]
bind = "127.0.0.1:8080"
[device]
bind = "127.0.0.1:37655"
```

(Then the device channel is not reachable from the LAN; only tunnel it.)

---

## Pairing a terminal

1. Start the service and watch the journal:

   ```bash
   sudo journalctl -u ale-server -f
   ```

   On startup it logs the current pairing code (8 characters, Crockford base32,
   TTL 300 s by default) at `INFO`. Because a running service rotates its code
   on expiry, always read the **latest** journal entry rather than an old one.

2. On the computer itself you can inspect the live pairing state over the
   loopback-only admin endpoint:

   ```bash
   curl -s http://127.0.0.1:8080/v1/device/status
   ```

   It returns the current pairing code and its expiry, active sessions and the
   audit tail. It is refused for non-loopback clients.

3. In the mobile app / terminal, enter the computer's address and the code.
   The client performs the Noise handshake with the PSK derived from the code;
   after `SessionReady` it receives a bearer token for the `/v1/egress/*`
   routes (valid 24 h by default, `pairing.token_ttl_s`).

4. `--print-pairing` prints one code and exits - useful before the service is
   running (for example over a serial console or in a provisioning script).
   It does not affect an already running service:

   ```bash
   /usr/lib/ale/ale-server --config /etc/ale/ale-server.toml --print-pairing
   ```

5. After all terminals are paired, disable further pairing in the
   configuration:

   ```toml
   [device.pairing]
   enabled = false
   ```

   Existing sessions and tokens are unaffected. Rotating/restarting the service
   generates a new code.

Pairing fails with stable protocol error codes (`PAIRING_EXPIRED`,
`PAIRING_DISABLED`, `TOO_MANY_ATTEMPTS`, ...); see
[`docs/PROTOCOL.md`](PROTOCOL.md). Increase `pairing.code_ttl_s` only up to a
value you accept (maximum 3600 s).

---

## Upgrades

```bash
# 1. Build/package the new version (on the device or cross)
./scripts/build-debian-arm.sh --with-modeld
./scripts/package-tarball.sh --skip-build

# 2. Copy the tarball to the target and verify it
sha256sum -c ale-server-<new>-linux-aarch64.tar.gz.sha256

# 3. Unpack and install; the existing config is preserved
tar -xzf ale-server-<new>-linux-aarch64.tar.gz
cd ale-server-<new>-linux-aarch64
sudo ./install.sh --system

# 4. Review the new template against your config, apply new keys if needed
sudo diff -u /etc/ale/ale-server.toml config/ale-server.toml || true

# 5. Restart and verify
sudo systemctl restart ale-server
sudo journalctl -u ale-server -n 50 --no-pager
curl -s http://127.0.0.1:8080/health
```

The installed configuration and state are never deleted by an upgrade; use
`--force` only after acknowledging that the template is the new baseline.
Rollback: keep the previous tarball, reinstall it the same way and restart.
There is no database migration in this service.

For the user unit: `./install.sh --user`, `systemctl --user restart ale-server`.

---

## Uninstall

```bash
# Keep configuration and state (recommended)
sudo ./install.sh --uninstall

# Remove everything, including /etc/ale, /var/lib/ale and the ale user
sudo ./install.sh --uninstall --purge

# User install
./install.sh --user --uninstall
./install.sh --user --uninstall --purge
```

The uninstaller stops and disables the unit, removes the service/user units,
binaries, symlink and tmpfiles rule, and runs `daemon-reload`. Configuration and
state are kept unless `--purge` is passed.

Manual cleanup if you removed the tarball first:

```bash
sudo systemctl disable --now ale-server
sudo rm -f /etc/systemd/system/ale-server.service
sudo rm -f /etc/systemd/system/ale-server.service.d -r
sudo rm -f /usr/lib/systemd/user/ale-server.service
sudo rm -f /usr/lib/ale/ale-server /usr/lib/ale/ale-modeld /usr/bin/ale-server
sudo rm -f /usr/lib/tmpfiles.d/ale-server.conf
sudo systemctl daemon-reload
# optional: sudo rm -rf /etc/ale /var/lib/ale && sudo userdel ale
```

---

## Troubleshooting

### The service does not start / configuration errors

```bash
sudo systemctl status ale-server --no-pager
sudo journalctl -u ale-server -n 100 --no-pager
sudo /usr/lib/ale/ale-server --check-config --config /etc/ale/ale-server.toml
```

Validation errors exit non-zero and name the offending key. Common causes:
typo in a `[device]` key (unknown keys are rejected), invalid address in a
`bind` key, a `pairing.code_ttl_s` outside `1..=3600`, or a config file not
readable by the `ale` user.

### HTTP 503 `ENGINE_UNAVAILABLE` on `/asr`, `/tts`, `/vlm`

The device channel and health routes are fine; the cloud engine could not start.
Check, in order:

1. `[engine] enabled` is `true`;
2. `[engine] config_path` points at a readable `ale-core` config file
   (`~/.config/ale-my-eyes/config.json`; for the system unit `HOME=/var/lib/ale`);
3. the provider URL/model/key in that file are valid, and the key env variable
   (if any) is exported to the service - systemd:
   `sudo systemctl show ale-server -p Environment`; the optional
   `/etc/ale/ale-server.env` is read at startup;
4. restart after fixing: `sudo systemctl restart ale-server`.

Engine failures are logged with their cause at `WARN`/`ERROR` and never crash
the service. Capture/automation/pairing keep working without a cloud engine.

### Capture fails: "no display", `CAPABILITY_UNAVAILABLE`, `BACKEND_ERROR`

* Check which unit runs the service: the **system** unit has no session. Use
  the user unit (see [System vs user service](#system-vs-user-service)).
* Inside the session, verify the environment:

  ```bash
  systemctl --user show-environment | grep -E 'XDG_SESSION_TYPE|DISPLAY|WAYLAND_DISPLAY|XDG_RUNTIME_DIR'
  grim /tmp/test.png && file /tmp/test.png          # Wayland
  DISPLAY=:0 scrot /tmp/test.png && file /tmp/test.png   # X11
  ```

* Install the backend for the session type (`grim`/`gnome-screenshot` for
  Wayland, `scrot`/`import` for X11) and make sure it is on the `PATH` of the
  service (`systemctl --user show ale-server -p Environment`).
* If `preferred_backend` is set, confirm the binary exists; an explicit backend
  that is missing is an error rather than an auto-detection fallback.
* GNOME Wayland may require the desktop's screenshot portal; install
  `gnome-screenshot` (the service invokes it in non-interactive mode).
* Check `journalctl --user -u ale-server` for the selected backend and the
  backend's stderr.

### `Permission denied` or no effect from automation

* `/dev/uinput` access (`ydotool`, `dotool`):

  ```bash
  ls -l /dev/uinput
  sudo modprobe uinput
  sudo usermod -aG input "$USER"      # then log out/in - group changes need a new session
  id | tr ' ' '\n' | grep -x input
  ```

* Install the udev rule shown in [Capture and automation backends](#capture-and-automation-backends)
  and reload udev.
* The system unit's `PrivateDevices=yes` hides `/dev/uinput` - use the user
  unit.
* X11: `DISPLAY`/`XAUTHORITY` are wrong for the service process, or another
  user owns the X socket; use the user unit or correct the environment.
* `wtype` only types text/keys (no pointer); use `ydotool`/`dotool`/`xdotool`
  for pointer actions.
* Actions can also be denied **by policy** (not by the OS): check the journal
  for `DENIED` and review `allow_high_risk`, `allow_process_actions`,
  `dry_run`, `frame_max_age_ms`. The error code distinguishes the two.

### Pairing code expired / pairing rejected

* An 8-character code is valid for `pairing.code_ttl_s` seconds (default 300).
  Read the current one from the journal:

  ```bash
  sudo journalctl -u ale-server --since '10 min ago' | grep -i pair
  ```

  or over loopback: `curl -s http://127.0.0.1:8080/v1/device/status`.
* `PAIRING_DISABLED`: `[device.pairing] enabled = false` - enable it or use an
  existing paired client.
* `TOO_MANY_ATTEMPTS`: failed handshakes exceeded `pairing.max_attempts` from
  that address; wait for the backoff window or restart the service.
* `PAIRING_EXPIRED`: the client used an old code; current codes rotate.
* If the client sees a wrong-code error, check that the code has no ambiguous
  characters misread (`0/O`, `1/I/L`) - the alphabet is Crockford base32 and
  excludes them.

### Connection refused / timeout on 37655

* `systemctl status ale-server` / `journalctl` - is the device channel enabled
  (`[device] enabled = true`) and bound successfully?
* `ss -tlnp | grep -E '8080|37655'` - is the process listening, on which
  address?
* Firewall/NAT between client and server; test from the same LAN:
  `nc -vz <host> 37655`.
* Binding a different port in config means clients must use that port.
* Pairing only happens over the device channel, not over HTTP.

### `STALE_FRAME`

The action plan referenced a screenshot older than
`[device.automation] frame_max_age_ms` (default 5000 ms) or a screenshot that
was replaced. The client must request a fresh capture first. Do not raise the
limit to work around race conditions; fix the client flow.

### `RATE_LIMITED`

The session exceeded the per-session request rate (30 requests/minute). Slow the
client down; this is a safety limit, not a bug.

### Unit starts but hardening blocks a needed path

Symptoms: the unit restarts, or capture/engine writes fail with permission
errors while a manual run works. The system unit makes everything read-only
except `/var/lib/ale` (`ProtectSystem=strict`, `ReadWritePaths=/var/lib/ale`).
Add a drop-in for the extra path:

```ini
[Service]
ReadWritePaths=/var/lib/ale /srv/ale-extra
```

Do not disable the sandbox wholesale; prefer the narrowest path. Check the
effective settings with `systemd-analyze security ale-server`.

### Cross-compilation errors

* `error: linker 'aarch64-linux-gnu-gcc' not found`: use cargo-zigbuild (zig
  provides the linker) or `cross` (Docker), or install
  `gcc-aarch64-linux-gnu` and set
  `.cargo/config.toml` with `linker = "aarch64-linux-gnu-gcc"`.
* `pkg-config` errors for `dbus-1`: the target sysroot must contain
  `libdbus-1-dev`; use `cross` (it does) or `cargo-zigbuild` after installing
  the arm64 package in the zig sysroot via the documented multiarch steps.
* `ring`/`aws-lc-rs` build failures: install `cmake clang` (and `perl`).
* Wrong architecture in the tarball: `file dist/.../bin/ale-server` must report
  `ELF 64-bit ... ARM aarch64`.

### `ale-modeld` fails to start / missing sherpa libraries

`ale-modeld` is optional. The packaged build uses `--no-default-features`
(no `sherpa-rs`/SenseVoice download); local SenseVoice ASR requires building on
the target with the default `sensevoice` feature:

```bash
cargo build --release --locked -p ale-modeld
```

The HTTP API, device channel and capture do not use `ale-modeld`.

---

## Hardening notes

The shipped system unit is intentionally strict; review it before weakening it:

* **Dedicated account** - runs as `ale:ale`, no login shell, no home directory
  requirement; `HOME=/var/lib/ale`.
* **No capabilities** - `CapabilityBoundingSet=` and `AmbientCapabilities=` are
  empty; ports 8080/37655 are unprivileged.
* **Filesystem** - `ProtectSystem=strict`, `ProtectHome=read-only`,
  `ReadWritePaths=/var/lib/ale`; only `/var/lib/ale` is writable.
* **Kernel surface** - `ProtectKernelTunables`, `ProtectKernelModules`,
  `ProtectKernelLogs`, `ProtectControlGroups`, `ProtectClock`,
  `ProtectHostname`, `PrivateDevices`, `PrivateTmp`, `RestrictNamespaces`,
  `LockPersonality`, `MemoryDenyWriteExecute`.
* **Syscalls and network** - `SystemCallFilter=@system-service`,
  `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`.
* **Logging** - journald only, `SyslogIdentifier=ale-server`; no log files.

Operational hardening:

* Put secrets only in `/etc/ale/ale-server.env` (mode 0600, root-owned) or the
  service environment; never in the TOML, the repository or the release tarball.
* Keep the HTTP API on the trusted LAN or behind a TLS reverse proxy/VPN - the
  API itself is plain HTTP and (apart from `/v1/egress/*` bearer tokens and the
  loopback-only admin endpoint) unauthenticated.
* Firewall 8080/37655; never expose them to the internet.
* Enable unattended security upgrades; Rust release builds are self-contained
  (no OpenSSL), so OS TLS library updates do not affect the service.
* Set `[device.pairing] enabled = false` once every terminal is paired.
* Keep `allow_high_risk = false` and `allow_process_actions = false` unless the
  paired terminal is fully trusted; use `dry_run = true` for the first pairing
  and observe the requested actions in the journal.
* Back up `/etc/ale/ale-server.toml`; state in `/var/lib/ale` is limited to
  engine configuration/caches and can be recreated.
* Review `systemd-analyze security ale-server` after adding drop-ins.
* The bounded audit log and structured error codes are the intended debugging
  surface; do not add verbose logging of screenshots, typed text, pairing codes
  or tokens.

---

## Files, paths and environment variables

| Path / variable | System install | User install |
| --- | --- | --- |
| binary | `/usr/lib/ale/ale-server` (+ `ale-modeld`) | `~/.local/bin/ale-server` |
| symlink | `/usr/bin/ale-server` | `~/.local/bin` is already on `PATH` |
| config | `/etc/ale/ale-server.toml` | `~/.config/ale/ale-server.toml` |
| secrets | `/etc/ale/ale-server.env` (0600 root) | `~/.config/ale/ale-server.env` (0600) |
| state | `/var/lib/ale/` (`ale:ale`, 0750) | `~/.local/share/ale/` |
| unit | `/etc/systemd/system/ale-server.service` | `~/.config/systemd/user/ale-server.service` |
| user unit (system-wide) | `/usr/lib/systemd/user/ale-server.service` | - |
| tmpfiles rule | `/usr/lib/tmpfiles.d/ale-server.conf` | - |
| engine config | `$HOME/.config/ale-my-eyes/config.json` | `~/.config/ale-my-eyes/config.json` |
| logs | `journalctl -u ale-server` | `journalctl --user -u ale-server` |

Environment variables:

| Variable | Meaning |
| --- | --- |
| `RUST_LOG` | tracing filter, e.g. `info` (unit default) or `ale_server=debug` |
| `ALE_SERVER__SECTION__KEY` | config override with double-underscore nesting, e.g. `ALE_SERVER__SERVER__BIND` |
| `OPENAI_API_KEY` | provider key referenced by `[egress] api_key_env` (name is configurable) |
| `ALE_SECRET__API_KEY`, `ALE_SECRET__BACKUP_API_KEY`, `ALE_SECRET__TRANSCRIPTION_API_KEY` | engine credentials when `[engine] secret_store = "env"` (read-only) |
| `XDG_SESSION_TYPE`, `DISPLAY`, `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, `XAUTHORITY` | display session environment needed by capture/automation (user unit inherits it) |

`--config PATH`, `--check-config` and `--print-pairing` are the supported CLI
flags; `--help` lists the current set.
