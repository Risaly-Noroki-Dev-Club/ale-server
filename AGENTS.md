# AGENTS.md

Guidance for coding agents working in this repository.

## What this repository is

Headless backend bundle of Ale, My Eyes!. It builds the computer-side service
(`ale-server`), the device capability library (`ale-device`), the vendored core
(`ale-core`) and the optional local model daemon (`ale-modeld`). No GUI code lives
here; the Slint desktop app is in `ale-my-eyes-desktop`.

Read `docs/ARCHITECTURE.md` and `docs/CONTRACTS.md` before changing code. The
contracts file records file ownership and frozen public APIs.

## Layout

```
ale-core/     vendored from ale-my-eyes-desktop@8408dce (see docs/UPSTREAM.md)
ale-device/   capture, automation, crypto/session, protocol (no GUI deps)
ale-server/   service binary: config, HTTP routes, egress, CLI
ale-modeld/   optional local model scheduler daemon
deploy/       systemd units + config template
scripts/      Debian arm64 build/package/install helpers
docs/         architecture, contracts, protocol, API, deployment
```

## Commands

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # optional, fix when cheap
cargo run -p ale-server -- --check-config
cargo run -p ale-server -- --print-pairing
```

On Debian arm64 the packaging helpers are `scripts/build-debian-arm.sh` and
`scripts/package-tarball.sh`; see `docs/DEPLOY.md`. Do not run deployment scripts
that modify system paths unless the task is explicitly about installation.

## Rules

* No GUI or native capture/input crates (no slint/cpal/rodio/xcap/enigo/rfd).
  Capture and automation must stay command-based so the workspace cross-compiles
  for `aarch64-unknown-linux-gnu` without X11/Wayland development headers.
* Never panic on untrusted input. Protocol handlers return `DeviceError` and map
  to stable error codes (`ale-device/src/error.rs`, `protocol::error_code`).
* Do not log screenshot bytes, typed text, pairing codes, tokens or API keys.
* Every network-facing limit (message size, frame size, request timeout, rate,
  concurrency, body size) comes from config; do not hard-code new unbounded paths.
* Action execution goes through `AutomationService::validate_plan` first. Never
  execute raw model output.
* Keep `ale-core` and `ale-modeld` close to upstream: put local changes in
  `docs/UPSTREAM.md` and be surgical.
* Preserve unrelated changes; keep generated output (`target/`, `dist/`) out of
  commits.

## Tests

Rust tests live next to the code (`#[cfg(test)]`) or in `crates/tests/`. Prefer
behavioural names (`rejects_stale_frame`, `pairing_code_rotates_after_ttl`). Fake
backends are scripts placed on `PATH` or passed via `preferred_backend` absolute
paths; tests must not require a real display server, cloud credentials or network.
