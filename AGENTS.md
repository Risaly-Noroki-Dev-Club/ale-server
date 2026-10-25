# AGENTS.md

Guidance for coding agents working in this repository.

## What this repository is

Headless backend bundle of Ale, My Eyes! and a **downstream** of
[`ale-my-eyes-desktop`](https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-desktop).
It builds the computer-side service (`ale-server`), the headless device library
(`ale-device`), the vendored core (`ale-core`) and the optional local model daemon
(`ale-modeld`). No GUI code lives here; the Slint desktop app is upstream.

The shared crates are vendored from a pinned upstream commit recorded in
`UPSTREAM_COMMIT`; `ale-core` no longer contains any local inference (ASR/VLM/LLM/
TTS/VAD) and has no Cargo features at all. Local models run in `ale-modeld` only,
and TTS for `/tts/synthesize` is implemented downstream in `ale-server`.

Read `docs/UPSTREAM.md` (provenance and sync rules), `docs/CONTRACTS.md`
(ownership, frozen APIs, the two protocols) and `docs/ARCHITECTURE.md` before
changing code. Upstream `ale-my-eyes-desktop` remains the parity reference for
`ale-core`/`ale-modeld`; never vendor working-tree state.

## Layout

```
UPSTREAM_COMMIT pinned ale-my-eyes-desktop sha (40 chars)
ale-core/       vendored from upstream: config, cloud model calls, protocol
                types (Misaka-Network v4), task harness, model IPC
ale-device/     capture, automation, crypto/session, terminal protocol v1
ale-server/     service binary: config, HTTP routes (incl. downstream TTS),
                egress, CLI
ale-modeld/     optional local model scheduler daemon
deploy/         systemd units + config template + tmpfiles rules
scripts/        build/package/install helpers + upstream sync scripts
docs/           architecture, contracts, protocol, API, deployment, upstream
```

## Commands

```bash
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets -- -D warnings   # optional, fix when cheap
cargo run -p ale-server -- --check-config
cargo run -p ale-server -- --print-pairing

# Vendored snapshot provenance (run from this repo root):
bash scripts/verify-upstream-sync.sh /path/to/ale-my-eyes-desktop
./scripts/sync-upstream.sh /path/to/ale-my-eyes-desktop [sha]   # only when syncing

# Native builds / packaging (Linux hosts; arm64 is the release target):
./scripts/build-debian-arm.sh --with-modeld
./scripts/build-debian-x64.sh --with-modeld
./scripts/package-tarball.sh --skip-build
```

CI (`.github/workflows/build.yml`) runs `test` (x86_64), `upstream-sync`
(checks out the pinned upstream sha and byte-compares the vendored trees) and
`build-aarch64`. Do not run deployment scripts that modify system paths unless the
task is explicitly about installation.

## Rules

* No GUI or native capture/input crates (no slint/cpal/rodio/xcap/enigo/rfd).
  Capture and automation must stay command-based so the workspace cross-compiles
  for `aarch64-unknown-linux-gnu` without X11/Wayland development headers.
* Never edit `ale-core/` or `ale-modeld/` locally. Fix the call site in
  `ale-device`/`ale-server`, or change upstream and bump `UPSTREAM_COMMIT` with
  `scripts/sync-upstream.sh`. Any unavoidable local deviation must be recorded in
  `docs/UPSTREAM.md` and verified by `scripts/verify-upstream-sync.sh`.
* Two protocols, never mixed: the vendored `ale_core::remote` is
  **Misaka-Network v4** (`misaka-network://`, `_misaka-network._tcp`, PSK
  `misaka-network-remote-v4`, chunk magic `MSKN`); this repository's
  `ale-device` terminal protocol is **v1** with an 8-character Crockford pairing
  code, PSK label `ale-device-pairing-v1` and `AME2` chunk framing. The terminal
  protocol deliberately does not follow the upstream v4 rename (Lead decision).
* TTS is a downstream responsibility: `ale-server` implements `/tts/synthesize`
  and `/v1/egress/tts` against a configured cloud endpoint; `ale-core` has no TTS.
  Local inference stays in `ale-modeld`.
* Never panic on untrusted input. Protocol handlers return `DeviceError` and map
  to stable error codes (`ale-device/src/error.rs`, `protocol::error_code`).
* Do not log screenshot bytes, typed text, pairing codes, tokens or API keys.
* Every network-facing limit (message size, frame size, request timeout, rate,
  concurrency, body size) comes from config; do not hard-code new unbounded paths.
* Action execution goes through `AutomationService::validate_plan` first. Never
  execute raw model output.
* Preserve unrelated changes; keep generated output (`target/`, `dist/`) out of
  commits. Do not run `git commit`/`reset`/`checkout`/`stash` unless explicitly asked.

## Tests

Rust tests live next to the code (`#[cfg(test)]`) or in `crates/tests/`. Prefer
behavioural names (`rejects_stale_frame`, `pairing_code_rotates_after_ttl`). Fake
backends are scripts placed on `PATH` or passed via `preferred_backend` absolute
paths; tests must not require a real display server, cloud credentials or network.
