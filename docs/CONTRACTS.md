# ale-server — extraction contracts

This repository is the **headless backend distribution** of Ale, My Eyes!. It is a
Rust workspace that builds the computer-side service and the optional local model
daemon. It must build and run on **Debian (aarch64/arm64)** with no GUI stack.

The desktop GUI (`ale-my-eyes-desktop`) is the *client / admin surface* and is not
part of this workspace.

## Workspace layout

```
Cargo.toml              workspace manifest (members, shared deps, release profile)
ale-core/               vendored from ale-my-eyes-desktop@8408dce (engine, actions,
                        config, cloud, remote, model_ipc, ...)
ale-modeld/             vendored model scheduler daemon (feature `sensevoice` opt-in)
ale-device/             NEW headless device service library (no GUI deps)
ale-server/             NEW service binary: HTTP API + encrypted terminal channel
deploy/                 systemd units, config template, install script
scripts/                build/package helpers (Debian arm64)
docs/                   architecture, protocol, API and deployment docs
```

Hard rules for every crate:

* no `slint`, `cpal`, `rodio`, `xcap`, `enigo`, `rfd`, `winit` or other GUI deps;
* no OS packaging assumptions inside libraries (paths/ports come from config);
* libraries must be `Send + Sync` where shared and must never panic on untrusted
  protocol input;
* Linux support must not require linking X11/Wayland/XTest: capture and input are
  executed through external commands so the crate cross-compiles cleanly;
* `cargo fmt --all` before handing off; `cargo clippy` warnings should be fixed
  when cheap.

## Ownership (do not edit outside your scope)

| Path | Owner |
| --- | --- |
| `ale-core/**`, `ale-modeld/**` | core teammate |
| `ale-device/src/display.rs`, `ale-device/src/capture.rs`, `ale-device/tests/capture_*.rs` | capture teammate |
| `ale-device/src/automation.rs`, `ale-device/tests/automation_*.rs` | automation teammate |
| `ale-device/src/crypto.rs`, `ale-device/src/session.rs`, `ale-device/tests/session_*.rs` | session teammate |
| `ale-server/**`, `ale-device/tests/egress_*.rs` | server teammate |
| `deploy/**`, `scripts/**`, `.github/**`, `docs/DEPLOY.md`, `README.md` | packaging teammate |
| `Cargo.toml` (root), `ale-device/Cargo.toml`, `ale-device/src/lib.rs`, `ale-device/src/config.rs`, `ale-device/src/error.rs`, `ale-device/src/protocol.rs`, `ale-device/src/audit.rs`, `docs/ARCHITECTURE.md`, `docs/CONTRACTS.md` | Lead |

If a contract needs to change, message the Lead instead of editing another
owner's file.

## `ale-device` public API (frozen; fill in the `unimplemented!()` bodies)

* `config::{DeviceConfig, CaptureConfig, AutomationConfig, PairingConfig, LimitsConfig,
  DEFAULT_DEVICE_PORT}` — serde schema with `Default` + `validate()`.
* `display::{DisplayInfo, DisplaySet, SessionKind, list_displays(&CaptureConfig)}` —
  discovery via `xrandr`, `wlr-randr`, `swaymsg`, `system_profiler` (macOS dev only)
  with a single-display fallback.
* `capture::{CaptureService, CaptureRequest, CapturedFrame, CaptureCapabilities,
  ImageMime, now_unix_ms, decode_dimensions}` — `Arc`-shared, caches the last frame,
  `capture` honors `cache_ttl_ms`, `capture_now` always shells out.
  Backend order on Linux: `grim` (Wayland), `spectacle`, `gnome-screenshot`,
  `import` (ImageMagick), `scrot`; macOS dev: `screencapture`.
* `automation::{AutomationService, AutomationCapabilities, ExecutionControl,
  ActionOutcome, inferred_risk}` — executes `ale_core::actions::ActionPlan` through
  `xdotool` / `ydotool` / `dotool` / `wtype`; `dry_run` logs only.
  Guards listed in the module docs are mandatory.
* `crypto::{SecureChannel, pairing_psk, psk_fingerprint, random_pairing_code,
  random_token, constant_time_eq}` — Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s with the
  `AME2` chunking scheme from the desktop GUI.
* `protocol::{DeviceMessage, SessionInit, SessionReady, DeviceCapabilities,
  FrameMeta, ClientKind, ExecutionState, HealthStatus, DEVICE_PROTOCOL_VERSION}` —
  `#[serde(tag = "type", rename_all = "snake_case")]`.
* `session::{SessionService, SessionDescriptor}` — `serve()` accepts until shutdown;
  `pairing_code()` / `rotate_pairing_code()`; `validate_token()` for the egress API.

Noise framing details (must match `remote_crypto.rs` of the desktop repo):

* plaintext up to 48 KiB is one frame;
* larger payloads are split into `AME2` chunks: `magic(4) | message_id(8 BE) |
  index(4 BE) | total(4 BE) | payload`;
* receiver rejects out-of-order, oversized or interleaved messages;
* handshake must complete within a small timeout, and failures are rate limited.

Pairing code: 8 Crockford-base32 characters (40 bits), TTL from
`pairing.code_ttl_s`, PSK = HKDF-SHA256 over `"ale-device-pairing-v1"` + code.

Session flow (WebSocket binary frames, port `37_655` by default):

1. client → handshake message (plain Noise message bytes, first WS binary frame);
2. server → handshake reply;
3. client → `SessionInit` (encrypted);
4. server → `SessionReady { session_id, token, capabilities, server_version }`;
5. request/response messages as defined in `protocol.rs`.

## `ale-server` binary contract

* binary name: `ale-server`; config path default `/etc/ale/ale-server.toml`,
  override with `--config`; env overrides use the `ALE_SERVER__` prefix
  (e.g. `ALE_SERVER__HTTP__BIND`).
* HTTP listener: default `0.0.0.0:8080`; routes kept from the original repo:
  `GET /health`, `GET /status`, `GET /models`, `POST /asr/transcribe`,
  `POST /tts/synthesize`, `POST /vlm/describe`, `POST /vlm/ask`.
* device channel: enabled by `[device]`, binds `device.bind`.
* egress proxy: `POST /v1/egress/chat`, `POST /v1/egress/vision`,
  `POST /v1/egress/tts`, `POST /v1/egress/asr` with
  `Authorization: Bearer <token from SessionReady>`; only allow-listed upstream
  hosts are reachable; requests/responses are size-capped and audited.
* `GET /v1/device/status` (localhost only) exposes pairing expiry, sessions and
  audit tail for the admin CLI.
* graceful shutdown on SIGINT/SIGTERM; logs to stderr (journald friendly);
  `--check-config` validates and exits.
* never panic on malformed HTTP; return JSON `{ "error": { "code", "message" } }`.

## Packaging contract

* `deploy/systemd/ale-server.service` (system) and
  `deploy/systemd/ale-server-user.service` (user session; needed for capture).
* config template `deploy/config/ale-server.toml`, docs `docs/DEPLOY.md`.
* `scripts/build-debian-arm.sh` builds on-device (native `cargo build --release`),
  `scripts/package-tarball.sh` creates `dist/ale-server-<version>-linux-aarch64.tar.gz`
  with binaries, config and units; `scripts/install.sh` installs on Debian.
* CI: `.github/workflows/build.yml` runs `cargo test --workspace` on x86_64 and
  cross-builds `aarch64-unknown-linux-gnu` (release binaries as artifacts).
* system paths: `/usr/lib/ale/`, `/etc/ale/`, `/var/lib/ale/`, log to journald.

## Verification

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # optional, fix when cheap
```

`ale-core`/`ale-modeld` are vendored from `ale-my-eyes-desktop` commit
`8408dce`, which is also the protocol/behaviour reference for desktop parity.

## Implementation decisions (final)

These were settled during implementation and are covered by tests:

* **Cancellation** — `AutomationService::execute` returns
  `Ok(ActionOutcome { cancelled: true, .. })` when `ExecutionControl` fires; the
  session layer maps that to `ActionResult { state: cancelled }`. Plan rejection
  (policy) is always `Err(DeviceError::Denied)`. Timeout is `Err(DeviceError::Timeout)`.
* **Capture target** — `CaptureRequest.display_id = None` captures the whole
  virtual desktop and reports `frame.display_id = None`; `Some(id)` crops to that
  display. Display discovery is cached for 5 s inside `CaptureService`.
* **Automation bounds** — `AutomationService` discovers the virtual desktop itself
  (xrandr / wlr-randr / swaymsg, cached) because it only receives
  `AutomationConfig`; `ALE_AUTOMATION_VIRTUAL_DESKTOP="x,y,w,h"` (or `off`) is the
  documented headless/test override.
* **Pairing code lifecycle** — the code is single-use: a successful pairing
  rotates it, and the code rotates automatically after `pairing.code_ttl_s`.
  Failed handshakes are counted per peer IP (60 s window) with a 60 s back-off
  after `pairing.max_attempts`.
* **Egress tokens** — the bearer token issued in `SessionReady` is valid for
  `pairing.token_ttl_s` (default 24 h), independent of the WebSocket session, so a
  terminal can keep calling the egress proxy after a reconnect.
* **Pairing code logging** — `ale-server` logs the active pairing code at INFO on
  startup and on rotation (headless deployments have no other way to read it).
  This is a deliberate exception to the "never log pairing codes" rule in
  `AGENTS.md`; never log it at DEBUG or in HTTP responses to non-loopback peers.
* **Secret store** — engine construction uses `AleEngine::new_with_secret_store`
  with `[engine].secret_store = auto|system|file|env`. `auto` probes the keyring
  (10 s cap) and falls back to a `0600` JSON file (`STATE_DIRECTORY/secrets.json`),
  so a Debian host without D-Bus Secret Service still starts.
* **Security fix (integration)** — the device WS handshake frame is capped at
  49 168 B and the handshake must finish within 10 s; unknown/expired pairing
  codes close the socket with a stable reason instead of falling into the request
  loop.
