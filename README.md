# ale-server

[![build](https://github.com/Risaly-Noroki-Dev-Club/ale-server/actions/workflows/build.yml/badge.svg)](https://github.com/Risaly-Noroki-Dev-Club/ale-server/actions/workflows/build.yml)

Headless backend bundle of **[Ale, My Eyes!](https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes)** —
the computer-side service for a pair of wearable eyes: an HTTP/JSON API, an
end-to-end encrypted terminal channel for screen capture and guarded desktop
automation, and an optional local model daemon.

This repository is a **downstream** of
[`ale-my-eyes-desktop`](https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-desktop):
`ale-core` and `ale-modeld` are vendored from a pinned desktop commit
(`UPSTREAM_COMMIT`, see [docs/UPSTREAM.md](docs/UPSTREAM.md)) and refreshed with
`scripts/sync-upstream.sh`. The pinned snapshot includes the Misaka-Network **v4**
protocol types and task harness, and its `ale-core` contains **no local
inference** (that lives only in `ale-modeld`). TTS is implemented downstream by
`ale-server` itself.

This repository is what runs on the *computer being looked at and controlled*.
It contains no GUI code, links no window-server or audio libraries, and runs as a
hardened systemd service on Debian arm64 (Raspberry Pi 4/5, Armbian boards,
ordinary arm64 servers) or as a user service inside a desktop session. Linux
x86_64 builds are supported too (see below).

## What is in the bundle

| Component | Kind | Purpose |
| --- | --- | --- |
| `ale-server` | binary | HTTP API, encrypted terminal channel, pairing, allow-listed cloud egress, downstream TTS backend, admin CLI |
| `ale-modeld` | optional binary | local model scheduler daemon (SenseVoice ASR via sherpa-onnx; GPU-only llama.cpp worker for Qwen2.5-VL/ShowUI) |
| `ale-device` | library | display discovery, command-based screenshot capture, guarded automation, Noise secure channel, session server, audit log, terminal protocol v1 |
| `ale-core` | library | vendored from the desktop repo: configuration, remote cloud model calls, protocol types (Misaka-Network v4), model IPC/scheduler types, task harness |

`ale-core` and `ale-modeld` are vendored from
[`ale-my-eyes-desktop`](https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-desktop)
at commit `43511cd` (full sha in [`UPSTREAM_COMMIT`](UPSTREAM_COMMIT)); see
[docs/UPSTREAM.md](docs/UPSTREAM.md) for provenance and local deviations
(currently none). Run `bash scripts/verify-upstream-sync.sh <desktop-checkout>` to
confirm byte identity; CI job `upstream-sync` enforces the same check against the
pinned upstream sha.

## Upstream source and sync

This repository never vendors a working tree. The pinned commit lives in
`UPSTREAM_COMMIT` and is documented in [docs/UPSTREAM.md](docs/UPSTREAM.md)
(provenance, verification results, and deliberate local changes — currently none).

```bash
# Refresh ale-core/ and ale-modeld/ from a committed desktop checkout
./scripts/sync-upstream.sh /path/to/ale-my-eyes-desktop [sha]

# Confirm the vendored trees are byte-identical to the pinned commit
bash scripts/verify-upstream-sync.sh /path/to/ale-my-eyes-desktop
```

Any call-site adaptation needed after a sync belongs in `ale-device`/`ale-server`
(owners of those crates), not in the vendored files. `cargo check --workspace
--locked` and `cargo test --workspace --locked` must pass against the new snapshot
before the pin is bumped. See [docs/CONTRACTS.md](docs/CONTRACTS.md) for the
ownership rules.

The vendored snapshot contains two independent protocol families: the desktop
phone-facing **Misaka-Network v4** (`ale-core::remote`) and this repository's own
**terminal protocol v1** (`ale-device`, `AME2` framing). They are not
wire-compatible; see [docs/PROTOCOL.md](docs/PROTOCOL.md).

## How it fits together

```
   desktop GUI (admin)          phone / dedicated terminal (control side)
          │                              │  WebSocket + Noise (paired, encrypted)
          │                              │  screenshot ⇄ action plan ⇄ health
          ▼                              ▼
   ┌─────────────────────────── ale-server ───────────────────────────┐
   │  HTTP API  0.0.0.0:8080           device channel  0.0.0.0:37655  │
   │  /health /status /models          pairing + SessionService       │
   │  /asr /vlm (cloud engine)         CaptureService  (grim, …)      │
   │  /tts (downstream TTS)            AutomationService (xdotool, …) │
   │  /v1/device/status (loopback)     AuditLog / rate limits         │
   │  /v1/egress/*     (Bearer token)                                 │
   └───────────────────────────────┬──────────────────────────────────┘
                                   │  allow-listed egress, rustls
                                   ▼
                            cloud model providers
```

* **Desktop GUI** — the rich admin surface: model downloads, pairing QR, local
  chat. It is a separate repository and is not needed on a headless machine. It
  serves the phone-facing **Misaka-Network v4** protocol.
* **Mobile client** — pairs with the desktop GUI over Misaka-Network v4; this
  repository's encrypted device channel speaks its own **terminal protocol v1**
  (see `docs/PROTOCOL.md`) and is not wire-compatible with v4.
* **Terminal (planned)** — dedicated hardware with a screen and continuous
  voice; it speaks the terminal protocol v1.
* **This repository** — the always-on computer side: capture, automation, model
  access and network egress, independent of any GUI process.

## Ports and endpoints

| Port | Transport | Default bind | Purpose |
| --- | --- | --- | --- |
| 8080 | HTTP | `0.0.0.0:8080` | JSON API, health checks, egress proxy |
| 37655 | WebSocket (Noise-encrypted) | `0.0.0.0:37655` | pairing + terminal sessions |

REST routes: `GET /health`, `GET /status`, `GET /models` (configuration view:
cloud model identifiers plus local `ale-modeld` model names),
`POST /asr/transcribe`, `POST /tts/synthesize` (downstream cloud TTS implemented
by `ale-server`; the vendored core has no TTS), `POST /vlm/describe`,
`POST /vlm/ask`, plus `GET /v1/device/status` (loopback only) and the
token-protected `POST /v1/egress/{chat,vision,tts,asr}`. See
[docs/API.md](docs/API.md).

## Quickstart

Prerequisites: a Rust stable toolchain ([rustup](https://rustup.rs)) and, on
Linux, the system libraries listed in
[docs/DEPLOY.md](docs/DEPLOY.md#build-dependencies)
(`build-essential pkg-config cmake clang libdbus-1-dev`).

```bash
# Verify the workspace
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked

# Build the service binary (and optionally the local model daemon)
cargo build --release --locked -p ale-server
# --no-default-features skips ale-modeld's optional sherpa/SenseVoice runtime
cargo build --release --locked -p ale-modeld --no-default-features

# Validate the shipped configuration template
./target/release/ale-server \
    --config deploy/config/ale-server.toml --check-config

# Print a pairing code and its expiry (first-time setup / serial console)
./target/release/ale-server \
    --config deploy/config/ale-server.toml --print-pairing

# Run the service: HTTP on :8080, device channel on :37655
./target/release/ale-server \
    --config deploy/config/ale-server.toml
```

With no `--config`, the service reads `/etc/ale/ale-server.toml` and starts with
built-in defaults (plus a log notice) if the file is missing. On startup the
current pairing code is logged at `INFO`:

```bash
journalctl -u ale-server -f          # system install
systemctl --user status ale-server   # user install
```

The configuration reference is
[deploy/config/ale-server.toml](deploy/config/ale-server.toml); every key can be
overridden with `ALE_SERVER__SECTION__KEY` environment variables (double
underscore = nesting), which keeps secrets out of files.

## Linux x64 build

On a Debian/Ubuntu x86_64 host, install the same build dependencies listed in
[docs/DEPLOY.md](docs/DEPLOY.md#build-dependencies), then run:

```bash
./scripts/build-debian-x64.sh
# Optional model daemon (without the SenseVoice runtime):
./scripts/build-debian-x64.sh --with-modeld --out dist/linux-x86_64
./dist/linux-x86_64/ale-server --config deploy/config/ale-server.toml
```

The script builds `x86_64-unknown-linux-gnu` release binaries with `--locked`,
checks their ELF architecture, strips symbols and prints SHA-256 checksums.
It supports `CARGO_TARGET_DIR` and requires a Linux x64 host; it does not
cross-compile from macOS or ARM. The existing `package-tarball.sh` produces
arm64 packages only and should not be used for these x64 binaries.

## Debian arm64 deployment

```bash
# On the target machine (native arm64 build)
./scripts/build-debian-arm.sh --with-modeld
./scripts/package-tarball.sh --skip-build

# From an unpacked release tarball
sudo ./install.sh --system        # systemd system service, user "ale"
./install.sh --user               # systemd --user service (capture/automation)
```

`dist/ale-server-<version>-linux-aarch64.tar.gz` contains stripped binaries,
the configuration template, both systemd units, `install.sh`, docs and a
`SHA256SUMS` file. CI cross-compiles the same artifact for
`aarch64-unknown-linux-gnu` and uploads it from every green build.

The complete guide — apt packages, Rust installation, Wayland/X11 capture
backends (`grim`, `wtype`, `ydotool`, `xdotool`), uinput permissions, pairing,
firewall, upgrades and troubleshooting — is [docs/DEPLOY.md](docs/DEPLOY.md).

## Security model at a glance

* **Pairing** — an 8-character Crockford-base32 code (40 bits), valid for 300 s
  by default. The Noise PSK is derived with HKDF-SHA256 over the
  `ale-device-pairing-v1` label and the code. Failed handshakes are rate
  limited per peer address.
* **Device channel** — Noise `NNpsk0_25519_ChaChaPoly_BLAKE2s` over WebSocket,
  terminal protocol version 1 (this repository's own protocol; not the desktop
  Misaka-Network v4). Every message after pairing is encrypted and authenticated;
  large payloads are chunked with the `AME2` framing.
* **Guarded automation** — the service never executes raw model output:
  `ActionPlan`s are re-validated, risk is recomputed from the actions, high-risk
  and process actions are denied unless explicitly enabled, coordinates are
  bounded to the virtual desktop, and plans bound to a stale screenshot are
  rejected (`STALE_FRAME`).
* **Egress proxy** — paired terminals reach model providers only through an
  allow-list of upstream hosts, over rustls, with size and time caps. Provider
  API keys stay in the service environment and are never logged.
* **Hardening** — the system unit runs as a dedicated `ale` user with no
  capabilities, `ProtectSystem=strict`, `ProtectHome=read-only`,
  `PrivateDevices`, a `@system-service` syscall filter and journald logging.
  Capture/automation backends are external commands selected at runtime, so the
  service itself links no X11/Wayland code.
* **No secrets in logs** — pairing codes, tokens, screenshot bytes, typed text
  and API keys are excluded from logs and the bounded audit log.

## Repository layout

```
UPSTREAM_COMMIT     pinned ale-my-eyes-desktop sha for the vendored crates
Cargo.toml          workspace manifest (members, shared deps, release profile)
ale-core/           vendored engine/config/protocol layer (see docs/UPSTREAM.md)
ale-device/         headless device library: capture, automation, crypto/session, protocol v1
ale-server/         service binary: config, HTTP routes (incl. downstream TTS), egress
ale-modeld/         optional local model scheduler daemon
deploy/             systemd units, config template, tmpfiles rules
scripts/            Debian arm64/x64 build, packaging, install and upstream-sync helpers
docs/               architecture, contracts, protocol, API, deployment, upstream
.github/workflows/  CI: x86_64 tests, upstream-sync vendored check, cross-built arm64 artifact
```

## Documentation

| Document | Contents |
| --- | --- |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | components, topology, trust boundaries, non-goals |
| [docs/CONTRACTS.md](docs/CONTRACTS.md) | extraction contracts, frozen APIs, file ownership |
| [docs/PROTOCOL.md](docs/PROTOCOL.md) | device wire protocol, pairing flow, error codes |
| [docs/API.md](docs/API.md) | HTTP routes, authentication, limits, error envelope |
| [docs/DEPLOY.md](docs/DEPLOY.md) | Debian 12 / Raspberry Pi OS / Armbian arm64 guide |
| [docs/UPSTREAM.md](docs/UPSTREAM.md) | provenance of the vendored desktop code |

## License

MIT — see [LICENSE](LICENSE). Part of the Ale, My Eyes! project.
