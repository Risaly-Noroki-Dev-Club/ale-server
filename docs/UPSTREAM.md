# Vendored upstream: `ale-core` and `ale-modeld`

This repository is the headless backend bundle. It has no GUI code, so the two core
crates are **vendored verbatim** from the desktop repository and kept as close to
upstream as possible. This file records the exact provenance, every local deviation,
and how to refresh the snapshot.

> Ownership: `ale-core/**` and `ale-modeld/**` belong to the core/vendoring owner.
> Any local change must be recorded in the "Deliberate local changes" table below.

## Source

| Field | Value |
| --- | --- |
| Repository | `ale-my-eyes-desktop` (`git@github.com:Risaly-Noroki-Dev-Club/ale-my-eyes-desktop.git`, https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-desktop) |
| Branch | `main` |
| Commit | `8408dce205592115695ac8c34038b95f55a8d47d` (short: `8408dce`) |
| Extracted | 2026-10-23 |
| Extracted by | `git archive HEAD ale-core ale-modeld` (commit above checked out in the upstream repo) |
| Vendored into | this workspace root (`/…/server/ale-server`) |

The desktop repository is the protocol/behaviour reference for desktop parity. It
remains the client / admin surface; its `ale-gui`, `ale-cli`, Slint layout files and
the vendored Slint backend patch are **not** part of this workspace.

## What was vendored (verbatim)

`git archive` of the two directories, including `Cargo.toml`, `build.rs`, sources,
tests, test fixtures and assets:

| Path | Contents | Feature notes |
| --- | --- | --- |
| `ale-core/` | `src/` (engine, config, cloud, actions, model IPC/scheduler, remote protocol, memory, diagnostics, …), `build.rs` (build-id/target env), `tests/fixtures/remote-v2.json`, `tests/fixtures/remote-v3.json`, `assets/model-probe.wav`, `assets/README.md` | `default = ["cloud"]`; opt-in `tts`, `local-inference` (`ort` load-dynamic + `whisper-rs`), `cloud-inference`, `adaptive` |
| `ale-modeld/` | `src/main.rs`, `src/gpu.rs`, `src/llama.rs`, `src/scheduler.rs`, `src/sensevoice.rs`, `src/bin/ale-modeld-acceptance.rs`, `tests/ipc_process.rs` | `default = ["sensevoice"]` → `sherpa-rs` 0.6.8 with `download-binaries` |

Not vendored (intentionally): the desktop root workspace manifest and `Cargo.lock`,
`ale-cli/`, `ale-gui/`, `vendor/` (Slint backend patch), `.github/`, `scripts/`,
`README*`, and any other desktop-only files. This workspace's root `Cargo.toml` is
owned by this repository and lists the two crates as workspace members (it supplies
`prost = "0.13"`, which the vendored `ale-core/Cargo.toml` references through
`workspace = true`; the remaining dependencies are declared directly in the vendored
manifests).

Parity check performed after extraction (`diff -rq` against the upstream checkout at
`8408dce`):

* `ale-core/` and `ale-modeld/` are byte-identical to the pinned commit for every
  tracked file;
* the only differences are upstream **working-tree-only, uncommitted** files:
  `ale-core/src/harness/` and the corresponding `pub mod harness;` line in
  `ale-core/src/lib.rs`. Because they are not part of commit `8408dce`, they are not
  vendored. (`harness/` is a device-independent observe/decide/act runtime; if it is
  committed upstream later, decide whether the backend needs it before syncing —
  it has no GUI dependency today.)
* `.DS_Store` files exist only in the upstream working tree; `git archive` does not
  carry them.

## Extraction commands

Run from a clean upstream checkout of `ale-my-eyes-desktop` at the pinned commit, with
`<server-repo>` being the root of this repository:

```bash
git -C ale-my-eyes-desktop archive HEAD ale-core ale-modeld \
  | tar -x -C <server-repo>
```

The archive contains no `.git` metadata, so provenance lives only in this file. After
extracting, confirm the workspace root `Cargo.toml` still lists `ale-core` and
`ale-modeld` under `[workspace] members` and supplies `prost = "0.13"` under
`[workspace.dependencies]`.

## Deliberate local changes

**As of the `8408dce` snapshot: none.** No source file, `Cargo.toml`, `build.rs`,
test, fixture or asset of either crate was modified. The headless workspace builds
and tests both crates unmodified:

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
`cargo 1.96.0`. Run from the workspace root unless noted.

| Command | Result |
| --- | --- |
| `cargo check -p ale-core` | **PASS** |
| `cargo test -p ale-core` | **PASS** — 118 passed, 0 failed, 0 ignored (+0 doc-tests); includes protocol golden fixtures and the vendored `assets/model-probe.wav` probe tests |
| `cargo check -p ale-modeld --no-default-features` | **PASS** (the configuration used for Debian arm64 packaging) |
| `cargo test -p ale-modeld --no-default-features` | **PASS** — 22 unit + 3 integration tests, including a real `ale-modeld` process authenticating over a unix socket |
| `cargo check -p ale-modeld` (default `sensevoice`) | **PASS on macOS only** — `sherpa-rs` 0.6.8 `download-binaries` downloaded `sherpa-onnx` v1.12.9 (`osx-universal2-shared`) into `~/Library/Caches/sherpa-rs`. Requires build-time network + `libclang` (bindgen) + a C compiler (`bzip2-sys` static) and produces a binary dynamically linked against `libsherpa-onnx-c-api`/`libonnxruntime`. Not the Debian packaging path; see caveats. |
| `cargo fmt -p ale-core -p ale-modeld -- --check` | **PASS** |
| `cargo check -p ale-core --locked` / `cargo check -p ale-modeld --no-default-features --locked` | **PASS** (lockfile consistent) |
| `cargo tree -p ale-modeld --no-default-features ... \| grep -Ei 'slint\|cpal\|rodio\|xcap\|enigo\|rfd\|winit\|gtk\|wayland\|x11'` | **no matches → no GUI/native capture/audio deps** |

Cross-compilation to `aarch64-unknown-linux-gnu` was not build-tested on this host
(the target/toolchain is not installed; Debian packaging builds natively on arm64 per
`scripts/build-debian-arm.sh`). Dependency resolution for that target was checked
with `cargo tree --target aarch64-unknown-linux-gnu`: 313 packages for
`ale-modeld --no-default-features` (`normal,build` edges), and no C-library `-sys`
crate, no `pkg-config`, no `cmake`, no `bindgen` — the only `-sys` crates are pure
Rust (`libbz2-rs-sys`, `dirs-sys`, `linux-raw-sys`). `ring` (via rustls) needs a C
compiler, which Debian `build-essential` provides.

## Known headless caveats of the vendored code (not extraction bugs)

These are upstream behaviours to account for when deploying on Debian arm64. They are
recorded here because they are easy to trip over during packaging/operations:

1. **Credentials (`keyring` 4.2.0, default feature `v1`).** On Linux this pulls the
   pure-Rust `zbus` Secret Service stack (59 extra crates relative to macOS, no native
   headers, no `libdbus-dev`). At runtime it needs a session D-Bus Secret Service
   (gnome-keyring/KWallet). `ale_core::secret_store::SystemSecretStore` errors are
   propagated with `?` by `ConfigManager::load`/`update_config`, so a headless system
   service without a keyring daemon can fail to load/save config. The public escape
   hatch is the `SecretStore` trait plus `ConfigManager::with_secret_store` /
   `AleEngine::new_with_secret_store`; a headless service should inject a
   file/environment-backed store instead of relying on the OS keychain.
2. **`ale-modeld` default feature `sensevoice`.** Enabling it (the default) makes the
   build download prebuilt `sherpa-onnx` binaries (network at build time), run
   `bindgen` (needs `libclang`), compile a static `bzip2` via `cc`, and link the
   resulting binary dynamically against `libsherpa-onnx-c-api`/`libonnxruntime`
   (those shared libraries must be packaged and found via RPATH). Debian arm64
   packaging therefore uses `--no-default-features`; ship SenseVoice support only if
   the sherpa runtime is packaged deliberately.
3. **Opt-in `ale-core` features `tts` / `local-inference`.** They add native
   toolchain/run-time requirements (`bindgen`, `cmake`, `whisper-rs-sys`,
   `ort-sys`, `native-tls`, system TTS) and are not enabled by the headless builds
   above. Keep them off for Debian packages unless explicitly required.
4. **Heavy default dependencies.** `ale-core` depends on `image` with default
   features (pulls `rav1e`/`ravif`/`exr`, a notable arm64 compile-time cost) and on
   `jsonschema` 0.33 and `reqwest` 0.12 with `rustls-tls` (no OpenSSL/native-tls
   system dependency). Trimming `image` features would speed up packaging but is a
   deviation from upstream; evaluate separately before changing.

## Sync procedure for future upstream core changes

1. **Pick and pin a commit.** Choose the upstream `main` commit to adopt; record its
   full hash. Do not vendor uncommitted working-tree state.
2. **Confirm upstream health.** In the desktop checkout, run its own checks at that
   commit (`cargo check -p ale-core -p ale-modeld`, `cargo test -p ale-core`) and note
   any feature/API changes.
3. **Diff before extracting.** From the upstream checkout, extract the pinned commit
   to a scratch directory and diff it against the current vendored copies:
   `diff -rq <scratch>/ale-core <server-repo>/ale-core` (same for `ale-modeld`).
   Reconcile the result with the "Deliberate local changes" table — every local
   difference must be accounted for.
4. **Replace the vendored trees.**
   ```bash
   rm -rf <server-repo>/ale-core <server-repo>/ale-modeld
   git -C ale-my-eyes-desktop archive <commit> ale-core ale-modeld \
     | tar -x -C <server-repo>
   ```
   (Removing first matters: `tar` extraction will not delete files that upstream
   removed.)
5. **Re-apply local changes** listed in this file, if any. Update the table if new
   changes were unavoidable, with rationale.
6. **Update this file**: commit hash, extraction date, verification results below,
   and any new headless caveats.
7. **Verify the vendored crates** (from the workspace root):
   ```bash
   cargo check -p ale-core
   cargo test -p ale-core
   cargo check -p ale-modeld --no-default-features
   cargo test -p ale-modeld --no-default-features
   cargo fmt -p ale-core -p ale-modeld -- --check
   cargo check -p ale-core --locked
   cargo check -p ale-modeld --no-default-features --locked
   cargo tree -p ale-modeld --no-default-features | grep -Ei 'slint|cpal|rodio|xcap|enigo|rfd|winit|gtk|wayland|x11'
   ```
   The `cargo tree` command must print nothing. Optionally try
   `cargo check -p ale-modeld` on a network-enabled host (default `sensevoice`
   feature) and record the result; it is not required for the Debian arm64 build.
8. **Verify the dependants.** `ale-device` and `ale-server` consume
   `ale_core::{actions, model_ipc, model_scheduler, config, …}`. Run
   `cargo check --workspace` and `cargo test --workspace`, then fix any breakage in
   `ale-device`/`ale-server` (their owners) rather than patching vendored code.
   `Cargo.lock` must be regenerated/committed so packaging can keep using `--locked`.
9. **Guard rails.** If the new upstream code introduces GUI/native capture/audio
   dependencies or desktop-only assumptions (window handles, tray, Slint types),
   stop before syncing and decide whether it belongs in this workspace at all;
   this repo must stay free of `slint`/`cpal`/`rodio`/`xcap`/`enigo`/`rfd` and stay
   cross-compilable for `aarch64-unknown-linux-gnu` without X11/Wayland headers.
10. **Update the task board / notify the Lead** if a vendored change affects the
    frozen interfaces in `docs/CONTRACTS.md` (e.g. `ale_core::model_ipc` wire
    contract, `actions` plan schema, remote protocol fixtures).
