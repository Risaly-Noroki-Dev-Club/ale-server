# ale-server — downstream contracts

This repository is the **headless backend distribution** of Ale, My Eyes! and a
**downstream** of [`ale-my-eyes-desktop`](https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-desktop):
the two shared crates (`ale-core`, `ale-modeld`) are vendored from a pinned desktop
commit and refreshed with `scripts/sync-upstream.sh`. The workspace also builds the
computer-side service (`ale-server`) and the headless device library (`ale-device`).

The upstream desktop repository remains the **source of truth** for `ale-core`,
`ale-modeld`, the Misaka-Network v4 desktop protocol, configuration and model
semantics. This repository adds the headless server/device layers on top; it must
build and run on Debian (aarch64/arm64 primary, x86_64 supported) with no GUI stack.

The desktop GUI is the *client / admin surface* and is not part of this workspace.

## Workspace layout

```
UPSTREAM_COMMIT         full sha of the vendored ale-my-eyes-desktop commit
Cargo.toml              workspace manifest (members, shared deps, release profile)
ale-core/               vendored from ale-my-eyes-desktop@43511cd: config, cloud
                        model calls, remote protocol types, task harness, model IPC
ale-modeld/             vendored model scheduler daemon (feature `sensevoice` opt-in)
ale-device/             headless device library: capture, automation, crypto,
                        session, terminal protocol v1 (no GUI deps)
ale-server/             service binary: HTTP API, device wiring, egress, CLI
deploy/                 systemd units, config template, tmpfiles rules
scripts/                build/package/install helpers + upstream sync scripts
docs/                   architecture, contracts, protocol, API, deployment, upstream
```

The vendored snapshot follows the desktop refactor that removed the dead local
inference subsystem from `ale-core`: **no `local-inference`/`tts`/`cloud` Cargo
features at all**, no ASR/VLM/LLM/TTS/VAD/memory modules, and `ale-core` only
performs remote model calls. The task harness (`ale-core/src/harness/`) is included
verbatim. Provenance, byte-identity verification and the sync procedure are
documented in `docs/UPSTREAM.md`.

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
| `ale-core/**`, `ale-modeld/**`, `UPSTREAM_COMMIT`, `docs/UPSTREAM.md`, `scripts/sync-upstream.sh`, `scripts/verify-upstream-sync.sh` | upstream-sync owner (vendored; only the sync procedure may refresh them) |
| `ale-device/**` (except its root `Cargo.toml`/`lib.rs` contract files) | device owner |
| `ale-server/**`, `ale-server/src/app.rs`, `ale-server/tests/egress_*.rs` | server/API owner |
| `README.md`, `AGENTS.md`, `docs/{ARCHITECTURE,PROTOCOL,DEPLOY,API,CONTRACTS}.md`, `deploy/**`, `scripts/**` (except the two sync scripts), `.github/**` | downstream docs/packaging owner |
| root `Cargo.toml`, `ale-device/Cargo.toml`, `ale-device/src/{lib,config,error,protocol,audit}.rs`, release/version decisions | Lead |

If a contract needs to change, message the Lead instead of editing another
owner's file. Never edit a vendored file locally: fix the call site in
`ale-device`/`ale-server`, or change it upstream and re-sync.

## Two protocols — do not confuse them

This repository contains two unrelated wire protocols:

1. **Vendored `ale_core::remote` — Misaka-Network v4.** This is the desktop
   phone-facing protocol, vendored verbatim from upstream. Version `4`, pairing
   QR scheme `misaka-network://`, mDNS `_misaka-network._tcp`, Noise PSK label
   `misaka-network-remote-v4`, chunk magic `MSKN`, execution states including
   `outcome_unknown`/`needs_user`. `ale-server` does not serve this protocol; the
   desktop app does. Do not rename or reuse its labels here.
2. **`ale-device` terminal protocol — version 1.** This repository's own
   encrypted terminal channel (`ale-device/src/{crypto,session,protocol}.rs`,
   served on port 37655 by default). It uses an 8-character Crockford-base32
   pairing code, Noise PSK derivation with the label `ale-device-pairing-v1`, and
   `AME2` chunk framing (`magic(4) | message_id(8 BE) | index(4 BE) | total(4 BE)
   | payload`). **It is not wire-compatible with Misaka-Network v4** and does not
   follow the upstream rename: Lead decision is to keep the terminal protocol at
   v1 with `AME2`/`ale-device-pairing-v1` while the desktop protocol is v4/`MSKN`.
   The two protocols may coexist in the product (desktop↔phone vs terminal↔server)
   but never share a connection or a PSK label.

## `ale-device` public API (frozen)

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
  terminal protocol's own `AME2` chunking (see above; not the desktop `MSKN`).
* `protocol::{DeviceMessage, SessionInit, SessionReady, DeviceCapabilities,
  FrameMeta, ClientKind, ExecutionState, HealthStatus, DEVICE_PROTOCOL_VERSION}` —
  `#[serde(tag = "type", rename_all = "snake_case")]`.
* `session::{SessionService, SessionDescriptor}` — `serve()` accepts until shutdown;
  `pairing_code()` / `rotate_pairing_code()`; `validate_token()` for the egress API.

Terminal protocol framing details (implemented in `ale-device/src/crypto.rs`):

* plaintext up to 48 KiB is one frame;
* larger payloads are split into `AME2` chunks (20-byte header + payload);
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
* HTTP listener: default `0.0.0.0:8080`; routes:
  `GET /health`, `GET /status`, `GET /models`, `POST /asr/transcribe`,
  `POST /tts/synthesize`, `POST /vlm/describe`, `POST /vlm/ask`.
* Inference ownership: `/asr`, `/vlm/describe`, `/vlm/ask` use the vendored cloud
  engine (`AleEngine` → `RemoteCloudApi`). **TTS is not in the vendored core**: the
  server implements `/tts/synthesize` (and `POST /v1/egress/tts`) downstream using
  the configured cloud provider; an unsupported provider/wire protocol returns a
  clear client error instead of a fake success.
* `GET /models` is assembled downstream from configuration: configured cloud
  model identifiers plus the local model names known to the model scheduler
  (local entries are marked as such). It never claims a model is downloaded or
  ready when the engine cannot confirm it.
* device channel: enabled by `[device]`, binds `device.bind` (terminal protocol v1).
* egress proxy: `POST /v1/egress/chat`, `POST /v1/egress/vision`,
  `POST /v1/egress/tts`, `POST /v1/egress/asr` with
  `Authorization: Bearer <token from SessionReady>`; only allow-listed upstream
  hosts are reachable; requests/responses are size-capped and audited.
* `GET /v1/device/status` (localhost only) exposes pairing expiry, sessions and
  audit tail for the admin CLI.
* graceful shutdown on SIGINT/SIGTERM; logs to stderr (journald friendly);
  `--check-config` validates and exits.
* never panic on malformed HTTP; return JSON `{ "error": { "code", "message" } }`.

## Upstream sync contract

* pinned sha lives in `UPSTREAM_COMMIT`; `docs/UPSTREAM.md` records provenance,
  deliberate local changes (currently none) and verification results.
* refresh: `./scripts/sync-upstream.sh <desktop-checkout> [sha]` (refuses a dirty
  upstream working tree, replaces `ale-core/`+`ale-modeld/`, prints a checklist).
* verify: `./scripts/verify-upstream-sync.sh <desktop-checkout>` compares both
  trees byte-for-byte with the pinned commit; CI job `upstream-sync` checks out the
  pinned upstream sha and runs the same script.
* never vendor working-tree state, never patch vendored files locally; adapt
  `ale-device`/`ale-server` call sites, or change upstream and bump the pin.

## Packaging contract

* `deploy/systemd/ale-server.service` (system) and
  `deploy/systemd/ale-server-user.service` (user session; needed for capture).
* config template `deploy/config/ale-server.toml`, docs `docs/DEPLOY.md`.
* `scripts/build-debian-arm.sh` builds natively on arm64 (primary release target);
  `scripts/build-debian-x64.sh` builds natively on x86_64 for Linux x64 hosts.
  Both build `ale-server` with `--locked` and the optional `ale-modeld` with
  `--no-default-features` (the sherpa/SenseVoice runtime is not downloaded).
* `scripts/package-tarball.sh` creates
  `dist/ale-server-<version>-linux-aarch64.tar.gz` with binaries, config, units,
  `install.sh`, docs/provenance files and `SHA256SUMS`; it is arm64-only by design.
* `scripts/install.sh` installs on Debian (system `--system` or `--user`).
* CI: `.github/workflows/build.yml` jobs `test` (x86_64), `upstream-sync`
  (pinned vendored check) and `build-aarch64` (cross-built release artifact).
* system paths: `/usr/lib/ale/`, `/etc/ale/`, `/var/lib/ale/`, log to journald.

## Verification

```bash
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets -- -D warnings   # optional, fix when cheap
bash scripts/verify-upstream-sync.sh /path/to/ale-my-eyes-desktop
```

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
* **TTS ownership** — the vendored `ale-core` no longer provides TTS or local
  inference; `/tts/synthesize` and `POST /v1/egress/tts` are implemented by this
  repository's server layer against a configured cloud endpoint. Local inference
  remains exclusively in `ale-modeld`.
* **Security fix (integration)** — the device WS handshake frame is capped at
  49 168 B and the handshake must finish within 10 s; unknown/expired pairing
  codes close the socket with a stable reason instead of falling into the request
  loop.
