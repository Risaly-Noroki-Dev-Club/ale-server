# Vendored upstream: `ale-core` and `ale-modeld`

This repository is the headless backend bundle. It has no GUI code, so the two core
crates are **vendored verbatim** from the desktop repository and kept as close to
upstream as possible. This file records the exact provenance, every local deviation,
and how to refresh the snapshot.

> Ownership: `ale-core/**` and `ale-modeld/**` belong to the core/vendoring owner.
> Any local change must be recorded in the "Deliberate local changes" table below.
> The pinned commit lives in the repository-root `UPSTREAM_COMMIT` file; the two
> helper scripts `scripts/sync-upstream.sh` / `scripts/verify-upstream-sync.sh`
> implement the procedure and CI enforces it (job `upstream-sync`). CI checks the
> upstream repository out at the pinned sha, so that commit must be pushed to
> `Risaly-Noroki-Dev-Club/ale-my-eyes-desktop` before the workflow can pass. The
> cross-repository checkout needs the upstream repository to be publicly readable or
> a `token:` for a PAT configured on that workflow step if it is private.

## Source

| Field | Value |
| --- | --- |
| Repository | `ale-my-eyes-desktop` (`git@github.com:Risaly-Noroki-Dev-Club/ale-my-eyes-desktop.git`, https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-desktop) |
| Branch | `refactor/misaka-network-v4` |
| Commit | `23ef51d6e7113066bd6ae910feb4954ee8c34ab5` (short: `23ef51d`) |
| Commit subject | `Refactor desktop: drop dead local inference, honest provider mapping, verified tasks, Misaka-Network v4` |
| Extracted | 2026-10-25 |
| Extracted by | `scripts/sync-upstream.sh <desktop-checkout>` → `git archive <sha> ale-core ale-modeld \| tar -x -C <this repo>` |
| Pinned in | `UPSTREAM_COMMIT` (full 40-character sha) |
| Vendored into | this workspace root (`/…/server/ale-server`) |

The desktop repository is the protocol/behaviour reference for desktop parity. It
remains the client / admin surface; its `ale-gui`, `ale-cli`, Slint layout files and
the vendored Slint backend patch are **not** part of this workspace.

## What was vendored (verbatim)

`git archive` of the two directories, including `Cargo.toml`, `build.rs`, sources,
tests, test fixtures and assets. This snapshot follows the desktop refactor that
removed the dead local-inference subsystem from `ale-core`:

| Path | Contents | Feature notes |
| --- | --- | --- |
| `ale-core/` | `src/`: `actions`, `child_process`, `cloud`, `config`, `context`, `desktop_download`, `diagnostics`, `error`, `harness/` (`cloud`, `mod`, `simulation`, `tests`), `lib`, `model_api` (+`model_api_tests`), `model_ipc`, `model_probe`, `model_scheduler`, `remote`, `secret_store`, `types`; `build.rs` (build-id/target env); `tests/fixtures/remote-v4.json`; `assets/model-probe.wav`, `assets/README.md` | **No `[features]` section at all.** Local inference (ASR/VLM/LLM/TTS/VAD/memory/`inference.rs`) was deleted upstream; `ale-core` only performs remote calls. The task harness (`src/harness/`) is committed upstream and vendored. |
| `ale-modeld/` | `src/main.rs`, `src/gpu.rs`, `src/llama.rs`, `src/scheduler.rs`, `src/sensevoice.rs`, `src/bin/ale-modeld-acceptance.rs`, `tests/ipc_process.rs` | `default = ["sensevoice"]` → `sherpa-rs` 0.6.8 with `download-binaries`; use `--no-default-features` for the Debian packaging path |

Upstream facts now baked into the vendored copy:

* the remote protocol is **Misaka-Network v4** (`ale-core/src/remote.rs`,
  `REMOTE_PROTOCOL_VERSION = 4`, QR scheme `misaka-network://`, mDNS
  `_misaka-network._tcp`, PSK label `misaka-network-remote-v4`, chunk magic `MSKN`);
  the only protocol fixture is `tests/fixtures/remote-v4.json`;
* cloud access has **one** `cloud::RemoteCloudApi` implementing four wire protocols;
  `config::CloudApiConfig.provider` is only a UI preset, `wire_api` is the
  transport truth, `effective_wire_api()` maps presets and config validation rejects
  preset/protocol conflicts. `retry_count` no longer exists;
* `AppConfig` gained `verified_tasks: { enabled: bool }` (default `true`, missing
  section falls back to the default);
* removed config sections/fields: `audio`, `asr`, `models.{auto_download,
  max_download_size, preferred_quality, offline_mode}`, `inference.{prefer_cloud,
  timeout, fallback_to_local}`; `models.models_dir` and `inference.mode` remain;
* `AleEngine::synthesize`, `available_models`, `ask_about_image`, `ask_about_image_with_memory`,
  the model download/install APIs, memory APIs, `load_local_*`, `init_tts`, `set_cloud_api`,
  `device_performance`, `network_status`, `context(_mut)` no longer exist.

Not vendored (intentionally): the desktop root workspace manifest and `Cargo.lock`,
`ale-cli/`, `ale-gui/`, `vendor/` (Slint backend patch), `.github/`, `scripts/`,
`README*`, and any other desktop-only files. This workspace's root `Cargo.toml` is
owned by this repository and lists the two crates as workspace members (it supplies
`prost = "0.13"`, which the vendored `ale-core/Cargo.toml` references through
`workspace = true`; the remaining dependencies are declared directly in the vendored
manifests).

Parity check performed after extraction (`scripts/verify-upstream-sync.sh <desktop-checkout>`):
`ale-core/` and `ale-modeld/` are **byte-identical** to commit `23ef51d` for every
file. `git archive` carries no `.git` metadata or `.DS_Store` files, so provenance
lives in `UPSTREAM_COMMIT` and this file.

## Sync procedure

Preferred (both scripts are safe to re-run; neither commits anything):

1. **Pick and pin a commit.** Choose a committed upstream `main` /
   `refactor/misaka-network-v4` commit; never vendor working-tree state.
2. **Confirm upstream health.** In the desktop checkout at that commit run its own
   `cargo check -p ale-core -p ale-modeld` / `cargo test -p ale-core`, and note
   feature/API changes.
3. **Sync.** From this repository root:
   ```bash
   ./scripts/sync-upstream.sh /path/to/ale-my-eyes-desktop <sha>   # or omit <sha> to reuse UPSTREAM_COMMIT
   ```
   The script refuses a dirty upstream working tree, removes the old trees first
   (so files upstream deleted disappear), extracts `git archive <sha> ale-core
   ale-modeld`, and prints the post-sync checklist.
4. **Pin the sha.** Update `UPSTREAM_COMMIT` to the new 40-character sha.
5. **Update this file**: source table, vendored content, caveats, and the
   verification table below with the results you actually ran.
6. **Regenerate the lockfile and verify the vendored crates** (workspace root):
   ```bash
   cargo check -p ale-core -p ale-modeld --no-default-features
   ./scripts/verify-upstream-sync.sh /path/to/ale-my-eyes-desktop
   cargo test -p ale-core
   cargo test -p ale-modeld --no-default-features
   cargo fmt -p ale-core -p ale-modeld -- --check
   cargo check -p ale-core --locked
   cargo check -p ale-modeld --no-default-features --locked
   cargo tree -p ale-modeld --no-default-features | grep -Ei 'slint|cpal|rodio|xcap|enigo|rfd|winit|gtk|wayland|x11'
   ```
   The `cargo tree` command must print nothing.
7. **Fix/verify the dependants.** `ale-device` and `ale-server` consume
   `ale_core::{actions, model_ipc, model_scheduler, config, …}`. Run
   `cargo check --workspace` and `cargo test --workspace`, then adapt their call
   sites (owners of those crates) instead of patching vendored code. `Cargo.lock`
   must be regenerated/committed so packaging can keep using `--locked`.
8. **Guard rails.** If the new upstream code introduces GUI/native capture/audio
   dependencies or desktop-only assumptions (window handles, tray, Slint types),
   stop before syncing and decide whether it belongs in this workspace at all;
   this repo must stay free of `slint`/`cpal`/`rodio`/`xcap`/`enigo`/`rfd` and stay
   cross-compilable for `aarch64-unknown-linux-gnu` without X11/Wayland headers.

## Deliberate local changes

**As of the `23ef51d` snapshot: none.** No source file, `Cargo.toml`, `build.rs`,
test, fixture or asset of either crate was modified; `scripts/verify-upstream-sync.sh`
reports both trees byte-identical to the pinned commit. The headless workspace
resolves unmodified:

* `prost = { workspace = true }` resolves to this workspace's `prost = "0.13"`, the
  same major/minor as the desktop workspace, so no manifest edit was needed;
* all other dependencies are declared with versions directly in the vendored
  manifests and resolve normally;
* the desktop `[patch.crates-io] i-slint-backend-winit` patch is **not** required:
  it only existed for `ale-gui`, and neither vendored crate references `slint`;
* no GUI/capture/audio crates (`slint`, `cpal`, `rodio`, `xcap`, `enigo`, `rfd`,
  `winit`, GTK/Wayland/X11) appear anywhere in either crate's dependency tree.

Future local changes (if any become unavoidable) must be added here with file,
change, and rationale, and kept as small as possible. Prefer feature-gating or a
host-provided trait implementation over editing vendored behaviour; never add GUI or
native capture/input dependencies. If a change is needed only for the new headless
crates, fix the call site there instead of forking the vendored code.

| File | Change | Rationale | Upstream conflict risk |
| --- | --- | --- | --- |
| — | — | — | — |

## Verification of this snapshot

Environment: macOS (`Darwin arm64`), `rustc 1.96.0 (ac68faa20 2026-05-25)`,
`cargo 1.96.0 (30a34c682 2026-05-25)`. Run from the workspace root on 2026-10-25.

| Command | Result |
| --- | --- |
| `scripts/verify-upstream-sync.sh <desktop-checkout>` | **PASS** — `ale-core` and `ale-modeld` byte-identical to `23ef51d` |
| `cargo check -p ale-core` | **PASS** |
| `cargo test -p ale-core` | **PASS** — 120 passed, 0 failed, 0 ignored (+0 doc-tests); includes harness tests, Misaka-Network v4 golden fixture and probe tests against the vendored `assets/model-probe.wav` |
| `cargo check -p ale-modeld --no-default-features` | **PASS** (the configuration used for Debian arm64 packaging) |
| `cargo test -p ale-modeld --no-default-features` | **PASS** — 22 unit + 3 integration tests, including a real `ale-modeld` process authenticating over a unix socket |
| `cargo fmt -p ale-core -p ale-modeld -- --check` | **PASS** |
| `cargo check -p ale-core --locked` / `cargo check -p ale-modeld --no-default-features --locked` | **PASS** (lockfile consistent) |
| `cargo tree -p ale-modeld --no-default-features \| grep -Ei 'slint\|cpal\|rodio\|xcap\|enigo\|rfd\|winit\|gtk\|wayland\|x11'` | **no matches → no GUI/native capture/audio deps** |
| `cargo check -p ale-device --all-targets` | **PASS** — the headless device crate needs no adaptation for this sync |
| `cargo check -p ale-server --all-targets` | **PASS** — after the downstream adaptation (WS-B) of `/models`, `/tts` and `/vlm/ask`; see below |
| `cargo check --workspace --locked` | **PASS** |
| `cargo test --workspace --locked` | **PASS** — 317 passed, 0 failed (ale-core 120, `ale-modeld` 22 unit + 3 integration, `ale-device` integration suites, `ale-server` routes/egress/engine-init/TTS tests) |
| `cargo fmt --all -- --check` | **PASS** |
| `cargo check -p ale-modeld` (default `sensevoice`) | **not run** — needs build-time network + `libclang` + C compiler; not the Debian packaging path |

Cross-compilation to `aarch64-unknown-linux-gnu` was not build-tested on this host
(the target/toolchain is not installed; Debian packaging builds natively on arm64 per
`scripts/build-debian-arm.sh`). The vendored dependency tree is unchanged in the
ways that matter for that target: no GUI/capture/audio/`-sys` C libraries and no
X11/Wayland headers are required by `ale-core`/`ale-modeld --no-default-features`.

### `ale-server` adaptation after this sync (resolved downstream)

The upstream engine deleted `AleEngine::available_models()`, `AleEngine::synthesize()`
and `AleEngine::ask_about_image()`. They were adapted **outside** the vendored
crates (WS-B), with no local modification to `ale-core`/`ale-modeld`:

| Route | Adaptation |
| --- | --- |
| `GET /models` | configuration view assembled in `ale-server` from the cloud config plus `model_scheduler` names (local entries marked `source: local-modeld`); no core model registry |
| `POST /tts/synthesize` | downstream OpenAI-compatible cloud TTS (`/audio/speech`, pinned `tts-1`/`alloy`/`wav`); unsupported `wire_api` → `501 UNSUPPORTED_WIRE_API`; see `docs/API.md` |
| `POST /vlm/ask` | now calls `ask_about_image_with_tools(image, &question, Vec::new())` and keeps the `VisionResponse` JSON shape |

`cargo check/test --workspace --locked` pass against the pinned snapshot; details in
`docs/CONTRACTS.md` and `docs/API.md`.

## Known headless caveats of the vendored code (not extraction bugs)

1. **Credentials (`keyring` 4.1).** On Linux this pulls the pure-Rust `zbus` Secret
   Service stack (no native headers, no `libdbus-dev`). At runtime it needs a session
   D-Bus Secret Service (gnome-keyring/KWallet). `ale_core::secret_store::SystemSecretStore`
   errors are propagated with `?` by `ConfigManager::load`/`update_config`, so a
   headless system service without a keyring daemon can fail to load/save config.
   The public escape hatch is the `SecretStore` trait plus
   `ConfigManager::with_secret_store` / `AleEngine::new_with_secret_store`; a headless
   service should inject a file/environment-backed store instead of relying on the OS
   keychain.
2. **`ale-core` no longer provides TTS or local inference.** The desktop refactor
   deleted `synthesize`, the TTS/ASR/VLM/LLM/VAD modules and the
   `tts`/`local-inference` Cargo features. `ale-server`'s frozen routes
   `POST /tts/synthesize` and `POST /v1/egress/tts` therefore need a downstream
   implementation (or a contract change with the Lead); local inference continues to
   live in `ale-modeld` only. Do **not** expect `tts`/`ort`/`whisper-rs` in the
   dependency tree.
3. **Cloud provider semantics changed.** `config::CloudApiConfig.provider` is now a
   UI preset only (`openai`, `anthropic`, `google`, custom); the transport protocol
   comes from `effective_wire_api()`, and `ConfigValidator` rejects a known preset
   paired with a conflicting explicit `wire_api`. Headless deployments that write
   config files directly must set `wire_api` to match a known preset (or use
   `provider = "custom"` with an explicit protocol). `CloudConfig` no longer has
   `provider` or `retry_count`.
4. **`ale-modeld` default feature `sensevoice`.** Enabling it (the default) makes the
   build download prebuilt `sherpa-onnx` binaries (network at build time), run
   `bindgen` (needs `libclang`), compile a static `bzip2` via `cc`, and link the
   resulting binary dynamically against `libsherpa-onnx-c-api`/`libonnxruntime`
   (those shared libraries must be packaged and found via RPATH). Debian arm64
   packaging therefore uses `--no-default-features`; ship SenseVoice support only if
   the sherpa runtime is packaged deliberately.
5. **Heavy default dependencies.** `ale-core` depends on `image` with default
   features (pulls `rav1e`/`ravif`/`exr`, a notable arm64 compile-time cost) and on
   `jsonschema` 0.33 and `reqwest` 0.12 with `rustls-tls` (no OpenSSL/native-tls
   system dependency). Trimming `image` features would speed up packaging but is a
   deviation from upstream; evaluate separately before changing.
6. **Two protocols by design.** Upstream desktop is now **Misaka-Network v4**
   (chunk magic `MSKN`, PSK label `misaka-network-remote-v4`, QR scheme
   `misaka-network://`), while this repository's `ale-device` **terminal
   protocol stays at version 1** with `AME2` chunking and the
   `ale-device-pairing-v1` PSK label. Lead decision: the terminal protocol does
   not follow the upstream rename and is **not wire-compatible** with the desktop
   protocol; never reuse v4 labels/keys here. See `docs/CONTRACTS.md` and
   `docs/PROTOCOL.md`.
