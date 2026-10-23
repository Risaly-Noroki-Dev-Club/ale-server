# Architecture

Ale, My Eyes! is split into four deployable pieces:

| Piece | Repo | Role |
| --- | --- | --- |
| Desktop GUI | `ale-my-eyes-desktop` | Slint app, local models, pairing QR, phone chat |
| Mobile client | `ale-my-eyes-mobile` | Android companion: voice, camera, remote control |
| **Backend service** | **this repo** | Headless computer-side service: HTTP API, encrypted device channel, optional model daemon |
| Terminal (planned) | — | Dedicated hardware: screen, continuous voice, on-device Whisper/ShowUI |

This repository is the *headless backend bundle*. It contains no GUI code and can run
as a systemd service on Debian arm64.

## Topology

```
             phone / dedicated terminal (control side)
                 │  WebSocket + Noise (paired, encrypted)
                 │  screenshot ⇄ action plan ⇄ health
                 ▼
┌──────────────────────── ale-server ────────────────────────┐
│  HTTP API (0.0.0.0:8080)        device channel (0.0.0.0:37655) │
│  /health /status /models        SessionService                   │
│  /asr /tts /vlm (cloud engine)   ├─ CaptureService  (grim/xdotool-free, command backends) │
│  /v1/device/status (loopback)    ├─ AutomationService (xdotool/ydotool, guarded)          │
│  /v1/egress/* (Bearer token)     ├─ AuditLog        (bounded, redacted)                   │
│                                  └─ pairing + Noise + rate limits                         │
└──────────────────────────────────────────────────────────────┘
                                 │
        egress proxy (allow-listed hosts, API key from env)
                                 ▼
                         cloud model providers
```

The optional `ale-modeld` binary remains available for on-computer local models
(Whisper via sherpa/SenseVoice, local TTS), speaking the same `ale_core::model_ipc`
contract as the desktop app.

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

* `ale-core` — vendored engine/protocol/config crate shared with the desktop app
  (cloud ASR/TTS/VLM, `Action`/`ActionPlan`, model management, memory).
* `ale-device` — the new headless device library: display discovery, command-based
  screenshot capture, guarded automation executor, Noise secure channel, session
  server, audit log, wire protocol.
* `ale-server` — the binary: configuration, HTTP API, device channel wiring, egress
  proxy, CLI (`--check-config`, `--print-pairing`), graceful shutdown.
* `ale-modeld` — optional local model scheduler daemon (feature `sensevoice`).

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
* optional ONNX/Whisper runtime via `ale-modeld` on ARM boards.
