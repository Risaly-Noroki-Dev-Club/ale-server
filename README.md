# ale-server

[![build](https://github.com/Risaly-Noroki-Dev-Club/ale-server/actions/workflows/build.yml/badge.svg)](https://github.com/Risaly-Noroki-Dev-Club/ale-server/actions/workflows/build.yml)

Headless backend bundle of **[Ale, My Eyes!](https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes)** —
the computer-side service for a pair of wearable eyes: an HTTP/JSON API, an
end-to-end encrypted device channel for screen capture and guarded desktop
automation, and an optional local model daemon.

This repository is what runs on the *computer being looked at and controlled*.
It contains no GUI code, links no window-server or audio libraries, and runs as a
hardened systemd service on Debian arm64 (Raspberry Pi 4/5, Armbian boards,
ordinary arm64 servers) or as a user service inside a desktop session.

## What is in the bundle

| Component | Kind | Purpose |
| --- | --- | --- |
| `ale-server` | binary | HTTP API, encrypted device channel, pairing, allow-listed cloud egress, admin CLI |
| `ale-modeld` | optional binary | local model scheduler daemon (SenseVoice/Whisper class ASR, local TTS) |
| `ale-device` | library | display discovery, command-based screenshot capture, guarded automation, Noise secure channel, session server, audit log |
| `ale-core` | library | vendored engine layer shared with the desktop app: cloud ASR/TTS/VLM, action plans, model management |

`ale-core` and `ale-modeld` are vendored from
[`ale-my-eyes-desktop`](https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-desktop)
at commit `8408dce`; see [docs/UPSTREAM.md](docs/UPSTREAM.md).

## How it fits together

```
   desktop GUI (admin)          phone / dedicated terminal (control side)
          │                              │  WebSocket + Noise (paired, encrypted)
          │                              │  screenshot ⇄ action plan ⇄ health
          ▼                              ▼
   ┌─────────────────────────── ale-server ───────────────────────────┐
   │  HTTP API  0.0.0.0:8080           device channel  0.0.0.0:37655  │
   │  /health /status /models          pairing + SessionService       │
   │  /asr /tts /vlm  (cloud engine)   CaptureService  (grim, …)      │
   │  /v1/device/status (loopback)     AutomationService (xdotool, …) │
   │  /v1/egress/*     (Bearer token)  AuditLog / rate limits         │
   └───────────────────────────────┬──────────────────────────────────┘
                                   │  allow-listed egress, rustls
                                   ▼
                            cloud model providers
```

* **Desktop GUI** — the rich admin surface: model downloads, pairing QR, local
  chat. It is a separate repository and is not needed on a headless machine.
* **Mobile client** — pairs with `ale-server`, sends voice/camera input and
  executes remote sessions.
* **Terminal (planned)** — dedicated hardware with a screen and continuous
  voice; it speaks the same encrypted device protocol.
* **This repository** — the always-on computer side: capture, automation, model
  access and network egress, independent of any GUI process.

## Ports and endpoints

| Port | Transport | Default bind | Purpose |
| --- | --- | --- | --- |
| 8080 | HTTP | `0.0.0.0:8080` | JSON API, health checks, egress proxy |
| 37655 | WebSocket (Noise-encrypted) | `0.0.0.0:37655` | pairing + terminal sessions |

REST routes: `GET /health`, `GET /status`, `GET /models`,
`POST /asr/transcribe`, `POST /tts/synthesize`, `POST /vlm/describe`,
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
  protocol version 1. Every message after pairing is encrypted and
  authenticated; large payloads are chunked with the `AME2` framing.
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
Cargo.toml          workspace manifest (members, shared deps, release profile)
ale-core/           vendored engine/config/protocol layer (see docs/UPSTREAM.md)
ale-device/         headless device library: capture, automation, crypto/session
ale-server/         service binary: config, HTTP routes, device wiring, egress
ale-modeld/         optional local model scheduler daemon
deploy/             systemd units, config template, tmpfiles rules
scripts/            Debian arm64 build, packaging and install helpers
docs/               architecture, contracts, protocol, API, deployment
.github/workflows/  CI: tests on x86_64, cross-built arm64 release artifact
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
