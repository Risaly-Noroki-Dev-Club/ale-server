# Architecture

Ale, My Eyes! is split into four deployable pieces:

| Piece | Repo | Role |
| --- | --- | --- |
| Desktop GUI | `ale-my-eyes-desktop` | Slint app, admin surface, phone protocol v4 (Misaka-Network), optional local model daemon on the desktop |
| Mobile client | `ale-my-eyes-mobile` | Android companion: voice, camera, remote control |
| **Backend service** | **this repo** | Headless computer-side service: HTTP API, encrypted terminal channel, optional model daemon. A **downstream** of the desktop repository |
| Terminal (planned) | — | Dedicated hardware: screen, continuous voice, terminal protocol v1 client |

This repository is the *headless backend bundle*. It contains no GUI code and runs
as a systemd service on Debian arm64 (x86_64 supported for local builds). Its
`ale-core/` and `ale-modeld/` are vendored from a pinned `ale-my-eyes-desktop`
commit (`UPSTREAM_COMMIT`, see `docs/UPSTREAM.md`); changes to shared core
behaviour are made upstream and re-synced, not patched here.

## Topology

```
             terminal / control client (terminal protocol v1)
                 │  WebSocket + Noise (paired, encrypted)
                 │  screenshot ⇄ action plan ⇄ health
                 ▼
┌──────────────────────── ale-server ────────────────────────┐
│  HTTP API (0.0.0.0:8080)        device channel (0.0.0.0:37655) │
│  /health /status /models        SessionService                   │
│  /asr /vlm (cloud engine)      ├─ CaptureService  (grim/xdotool-free, command backends) │
│  /tts (downstream TTS backend) ├─ AutomationService (xdotool/ydotool, guarded)          │
│  /v1/device/status (loopback)  ├─ AuditLog        (bounded, redacted)                   │
│  /v1/egress/* (Bearer token)   └─ pairing + Noise + rate limits                         │
└──────────────────────────────────────────────────────────────┘
                                 │
        egress proxy (allow-listed hosts, API key from env)
                                 ▼
                         cloud model providers
```

The optional `ale-modeld` binary remains available for on-computer local models
(SenseVoice ASR via sherpa-onnx, Qwen2.5-VL/ShowUI via a GPU-only llama.cpp
worker), speaking the same `ale_core::model_ipc` contract as the desktop app.
The vendored `ale-core` itself contains **no local inference and no TTS**: it only
performs remote cloud calls, holds configuration/protocol types and the task
harness. TTS for `/tts/synthesize` is implemented in this repository's server layer.

## Two protocols, deliberately separate

* **Desktop phone-facing protocol** — Misaka-Network **v4** (vendored
  `ale-core::remote`): `misaka-network://` QR scheme, `_misaka-network._tcp`
  mDNS, PSK label `misaka-network-remote-v4`, chunk magic `MSKN`. Served by the
  desktop app, not by `ale-server`.
* **Terminal protocol** — `ale-device`'s own **v1**: 8-character Crockford pairing
  code, PSK label `ale-device-pairing-v1`, `AME2` chunk framing. Served by
  `ale-server` on the device channel. It is **not wire-compatible** with
  Misaka-Network v4 and intentionally keeps its own names (Lead decision).
  See `docs/PROTOCOL.md`.

## Why the split

* The terminal must keep working when the computer is busy; the service keeps
  capture/action state independent of any GUI process.
* Models can move to the terminal later without rewriting the computer side:
  the terminal already receives screenshots and produces action plans.
* Network egress stays on the computer: credentials live there, the terminal never
  needs direct internet access.
* Headless deployment: no Slint, no audio stack, no window server required for the
  HTTP/inference side; capture/automation backends are selected at runtime.

## Trust boundaries

| Link | Protection |
| --- | --- |
| terminal ⇄ service | Noise_NNpsk0 (Curve25519 + ChaChaPoly + BLAKE2s), PSK derived from a short-lived pairing code; every request bounded by timeouts, rate limits and per-request cancellation |
| service ⇄ cloud | rustls, allow-listed upstream hosts, API key from environment, size caps |
| service ⇄ OS | command-driven capture/automation only; no privileged daemon; guarded action policy (risk levels, approval, coordinate bounds, stale-frame rejection) |

The service never trusts model output: `ActionPlan` is re-validated and its risk is
recomputed from the actions themselves. A plan bound to an old screenshot is
rejected with `STALE_FRAME`.

## Component summary

* `ale-core` (vendored, pinned `23ef51d`) — configuration, remote cloud model calls
  (`RemoteCloudApi` for the four wire protocols), `Action`/`ActionPlan`, model IPC
  and scheduler types, the Misaka-Network v4 protocol types, and the task harness.
  No local inference, no TTS, no model management/memory subsystem.
* `ale-device` — the headless device library: display discovery, command-based
  screenshot capture, guarded automation executor, Noise secure channel, session
  server, audit log, terminal protocol v1.
* `ale-server` — the binary: configuration, HTTP API (including the downstream TTS
  backend), device channel wiring, egress proxy, CLI (`--check-config`,
  `--print-pairing`), graceful shutdown.
* `ale-modeld` (vendored) — optional local model scheduler daemon (feature
  `sensevoice`; Debian packages build it with `--no-default-features`).

## Non-goals (for now)

* No GUI in this repository; the desktop app remains the admin surface.
* No arbitrary TCP/HTTP proxy: egress is limited to configured model endpoints.
* No model inference needed for capture/automation; the service is useful on a
  machine with no API key.
* No Windows-specific automation backend yet (the command-based design keeps the
  door open).

## Roadmap hooks

* terminal client reference implementation (Noise initiator + JPEG screenshot + plan);
* authenticated pairing confirmation on the terminal screen;
* multi-display aware action validation (already in `CaptureService` metadata);
* optional local ASR/VLM runtime via `ale-modeld` on ARM boards, packaged
  deliberately with the sherpa/llama.cpp runtimes.
