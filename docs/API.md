# `ale-server` HTTP API

Headless service binary for **Ale, My Eyes!**. It exposes:

* the model/status API (`/health`, `/status`, `/models`, `/asr`, `/vlm`);
* a downstream TTS backend (`/tts/synthesize`) implemented by `ale-server`
  itself, because the vendored `ale-core` no longer provides TTS;
* a loopback-only admin endpoint for the encrypted device channel
  (`/v1/device/status`);
* an allow-listed, size-capped AI egress proxy (`/v1/egress/*`) authenticated
  with session tokens from the device channel.

The device channel itself is a separate WebSocket protocol documented in
`docs/PROTOCOL.md`; this file only covers HTTP.

* Default listener: `0.0.0.0:8080` (`[server].bind`).
* Default config: `/etc/ale/ale-server.toml` (`--config` overrides).
* Java/Kotlin, Rust, curl and any HTTP client work; requests and responses are
  UTF-8 JSON unless stated otherwise.

---

## 1. Conventions

### 1.1 Error envelope

Every non-2xx response produced by the service is JSON:

```json
{
  "error": {
    "code": "EGRESS_DENIED",
    "message": "upstream host `evil.example.com` is not allow-listed"
  }
}
```

Clients **must** branch on `error.code`; `message` is human-readable and may
change. Error codes are stable:

| Code | HTTP | Meaning |
| --- | --- | --- |
| `ENGINE_UNAVAILABLE` | 503 | `[engine].enabled = false`, or lazy initialization failed/timed out. Inference routes only. |
| `INFERENCE_FAILED` | 502 | Engine reached but the inference/cloud call failed. |
| `UNSUPPORTED_WIRE_API` | 501 | `/tts/synthesize` requires an OpenAI-compatible endpoint, but the configured cloud `wire_api` is not `openai_chat_completions`. |
| `UNAUTHORIZED` | 401 | Missing/invalid `Authorization: Bearer` token (egress). |
| `FORBIDDEN` | 403 | Non-loopback `/v1/device/status`. |
| `NOT_FOUND` | 404 | Unknown route. |
| `PAYLOAD_TOO_LARGE` | 413 | Request body/multipart field/decoded payload over a cap. |
| `INVALID_JSON` | 400 | Malformed JSON body. |
| `INVALID_MULTIPART` | 400/413 | Malformed multipart body. |
| `INVALID_REQUEST` | 400 | Syntactically valid but unusable request. |
| `DEVICE_DISABLED` | 503 | `[device].enabled = false` (device status / egress token validation). |
| `EGRESS_DISABLED` | 403 | `[egress].enabled = false`. |
| `EGRESS_DENIED` | 403 | Resolved upstream host is not in `[egress].allowed_hosts`. |
| `EGRESS_NOT_CONFIGURED` | 503 | `[egress].api_key_env` is unset/blank at request time. |
| `EGRESS_UPSTREAM_ERROR` | 502 | Upstream 4xx/5xx, timeout, connection or redirect failure. |
| `EGRESS_RESPONSE_TOO_LARGE` | 502 | Upstream response exceeded `[egress].max_response_bytes`. |
| `INTERNAL_ERROR` | 500 | Caught panic / unexpected internal failure. |

Method-not-allowed is axum's default `405` with an empty body and an `Allow`
header; `/health` and `/status` are public, the inference routes are protected by
network policy (bind address/firewall), not by application authentication.

### 1.2 Response headers

Responses are `application/json` unless the route forwards a binary upstream
response (egress TTS → audio). The server does not emit permissive CORS headers;
see §7.

### 1.3 No streaming

Server-sent events / chunked streaming are not supported. `POST /v1/egress/chat`
rejects `"stream": true` with `400 INVALID_REQUEST`, and upstream streaming
responses are buffered (still subject to the response size cap).

---

## 2. Health and status

### `GET /health`

Liveness probe. **Always `200` while the process is serving**, even when the
engine or device channel is unavailable — supervisors should not restart a
healthy process just because a model is missing. Readiness is expressed by the
boolean fields.

```bash
curl -s http://127.0.0.1:8080/health
```

```json
{
  "status": "ok",
  "version": "0.2.0",
  "engine_enabled": true,
  "engine_ready": false,
  "engine_error": null,
  "device_enabled": true,
  "device_ready": true,
  "uptime_s": 42
}
```

| Field | Meaning |
| --- | --- |
| `status` | `ok`, or `degraded` when the engine is enabled but the last lazy initialization attempt failed. |
| `version` | `ale-server` crate version. |
| `engine_enabled` | `[engine].enabled`. |
| `engine_ready` | Engine successfully initialized. |
| `engine_error` | Last init error, truncated to 300 chars (`null` when none). |
| `device_enabled` | `[device].enabled`. |
| `device_ready` | Device WebSocket runtime is running. |
| `uptime_s` | Seconds since the HTTP state was created. |

### `GET /status`

Redacted configuration + runtime summary. Never contains credential values
(`[egress].api_key_env` is the *name* of the environment variable, plus a
boolean `api_key_configured`).

```json
{
  "version": "0.2.0",
  "uptime_s": 42,
  "config_source": "file",
  "config_path": "/etc/ale/ale-server.toml",
  "config": {
    "server": { "bind": "0.0.0.0:8080", "cors_origin": [] },
    "engine": {
      "enabled": true,
      "config_path": null,
      "resolved_config_path": "/root/.config/ale-my-eyes/config.json",
      "secret_store": "auto",
      "secret_store_path": "/var/lib/ale/secrets.json"
    },
    "device": {
      "enabled": true,
      "bind": "0.0.0.0:37655",
      "pairing": { "enabled": true, "code_ttl_s": 300, "max_attempts": 5, "token_ttl_s": 86400 },
      "capture": { "preferred_backend": null, "max_width": 1600, "jpeg_quality": 78, "timeout_ms": 5000, "cache_ttl_ms": 0 },
      "automation": { "preferred_backend": null, "allow_high_risk": false, "allow_process_actions": false, "dry_run": false, "timeout_ms": 30000, "max_actions": 20, "frame_max_age_ms": 5000 },
      "limits": { "max_connections": 4, "max_concurrent_requests": 2, "max_message_bytes": 8388608, "idle_timeout_s": 120, "request_timeout_s": 90 }
    },
    "egress": {
      "enabled": false,
      "upstream": "https://api.openai.com",
      "allowed_hosts": ["api.openai.com"],
      "api_key_env": "OPENAI_API_KEY",
      "api_key_configured": true,
      "timeout_s": 60,
      "max_body_bytes": 1048576,
      "max_response_bytes": 16777216
    },
    "logging": { "level": "info", "format": "text" }
  },
  "runtime": {
    "engine_enabled": true,
    "engine_ready": false,
    "engine_error": null,
    "device_enabled": true,
    "device_ready": true,
    "egress_enabled": false,
    "egress_ready": false
  }
}
```

`config_source` is `"file"` or `"defaults"` (the default config path did not
exist and built-in defaults were used).

---

## 3. Inference API

The engine is created **lazily** on the first inference request, inside a
`tokio::sync::Mutex`. Initialization failures are logged, reported through
`/health` (`degraded`) and returned as `503 ENGINE_UNAVAILABLE`; they never crash
the process. Failed initializations are retried at most once every 30 s; a single
initialization attempt is bounded by a 120 s timeout.

The vendored `ale-core` engine is **remote-only**: it performs cloud chat,
vision and ASR calls, but it ships no local inference, TTS or model registry.
`/asr` uses the independent **transcription endpoint** from the ale-core
configuration, `/vlm/*` uses the primary `cloud_api` endpoint, and
`/tts/synthesize` is implemented by `ale-server` itself (see below). Local models
run in the optional `ale-modeld` daemon and are not reachable through these
routes.

If an inference call itself fails after the engine is up, the route returns
`502 INFERENCE_FAILED` (error text truncated).

### `GET /models`

Returns a **configuration view** of the models this host can use; the engine no
longer keeps a downloaded-model registry. `503 ENGINE_UNAVAILABLE` when the
engine is disabled or cannot be initialized.

```json
{
  "models": [
    {
      "id": "gpt-4o",
      "name": "gpt-4o",
      "description": "Cloud chat/vision model (provider `openai`, wire_api `openai_chat_completions`, base URL `https://api.openai.com/v1`)",
      "size_bytes": null,
      "purpose": "chat",
      "quantization": null,
      "recommended_for": null,
      "source": "cloud"
    },
    {
      "id": "Qwen2.5-VL-3B-Instruct",
      "name": "Qwen2.5-VL-3B-Instruct",
      "description": "Local vision-language planning model served by ale-modeld (GPU-only)",
      "size_bytes": null,
      "purpose": "planning",
      "quantization": null,
      "recommended_for": null,
      "source": "local-modeld"
    }
  ]
}
```

| Field | Meaning |
| --- | --- |
| `id`, `name` | Model identifier from the ale-core configuration. |
| `description` | Human-readable origin (cloud endpoint or `ale-modeld` role). |
| `size_bytes` | Always `null`; the server does not track downloaded file sizes. |
| `purpose` | `chat`, `asr`, `planning` or `grounding`. |
| `source` | `cloud` (primary `cloud_api` or transcription endpoint) or `local-modeld`. |
| `quantization`, `recommended_for` | Kept for response compatibility; always `null`. |

Cloud entries appear when the corresponding key is configured: the primary model
when `cloud_api.api_key` is set, and the transcription model when
`transcription.enabled` and its endpoint key is set. `ale-modeld` entries
(`model_scheduler.qwen_model`, `qwen_large_model`, `grounding_model`) appear only
while `model_scheduler.enabled = true`. Duplicate identifiers are de-duplicated
case-insensitively (cloud entries win). Model files may still be missing on disk
— this route reports configuration, not readiness.

Errors: `503 ENGINE_UNAVAILABLE`.

### `POST /asr/transcribe`

Transcribe audio. Multipart form data.

| Form field | Required | Notes |
| --- | --- | --- |
| `audio` (or `file`) | yes | raw audio bytes (WAV/FLAC/etc. per engine) |
| `language` | no | accepted for compatibility; the transcription endpoint decides the language today |
| `format` | no | accepted for compatibility |

Body limit: **32 MiB** (`413 PAYLOAD_TOO_LARGE` beyond that).

```bash
curl -s -X POST http://127.0.0.1:8080/asr/transcribe \
  -F audio=@speech.wav
```

```json
{ "text": "hello world" }
```

Transcription uses the independent endpoint from the ale-core configuration
(`transcription.enabled` plus its own base URL/key/model); without it the route
returns `502 INFERENCE_FAILED`.

Errors: `400 INVALID_REQUEST` (missing `audio`), `400 INVALID_MULTIPART`,
`413 PAYLOAD_TOO_LARGE`, `502 INFERENCE_FAILED`, `503 ENGINE_UNAVAILABLE`.

### `POST /tts/synthesize`

Synthesize speech from text through the configured cloud endpoint. JSON body,
limit **1 MiB**; `text` is capped at **4096 characters**.

```json
{ "text": "hello world", "voice": "alloy", "speed": 1.0 }
```

`voice`/`speed` are accepted for client compatibility but **ignored**: the
downstream request is pinned to OpenAI-compatible `tts-1`/`alloy`/`wav`. It is
sent to `{cloud_api.api_url}/audio/speech` with the Bearer key from the ale-core
credential store (`cloud_api.api_key`), so the host must have a configured cloud
key. The configured `cloud_api.timeout` bounds the upstream call.

```json
{
  "audio_base64": "UklGRiQAAABXQVZF...",
  "format": "wav",
  "bytes": 24576
}
```

`format` is sniffed from the audio bytes: `wav`, `mp3`, `ogg`, `flac` or `bin`.
An upstream audio response larger than 32 MiB is rejected to avoid unbounded
buffering.

Errors: `400 INVALID_REQUEST` (empty/too long text), `400 INVALID_JSON`,
`413 PAYLOAD_TOO_LARGE`, `501 UNSUPPORTED_WIRE_API` (configured `wire_api` is not
`openai_chat_completions`), `502 INFERENCE_FAILED` (missing cloud key, upstream
timeout/connection/HTTP error), `503 ENGINE_UNAVAILABLE` (engine disabled or
initialization failed).

### `POST /vlm/describe`

Describe an image. Multipart form data.

| Form field | Required | Notes |
| --- | --- | --- |
| `image` (or `file`) | yes | raw image bytes |
| `format` | no | accepted for compatibility |

Body limit: **16 MiB**.

```json
{ "description": "A laptop on a wooden desk with a terminal open." }
```

Errors: `400 INVALID_REQUEST`, `400 INVALID_MULTIPART`, `413 PAYLOAD_TOO_LARGE`,
`502 INFERENCE_FAILED`, `503 ENGINE_UNAVAILABLE`.

### `POST /vlm/ask`

Visual question answering. Multipart form data.

| Form field | Required | Notes |
| --- | --- | --- |
| `image` (or `file`) | yes | raw image bytes |
| `question` (or `prompt`) | yes | UTF-8 question text |

Body limit: **16 MiB**. The response mirrors `ale-core`'s vision response:

```json
{
  "content": "The window is closed.",
  "tool_calls": null,
  "tokens_used": 812,
  "model": "gpt-4o-mini"
}
```

The route always asks the cloud vision model and supplies no tools, so
`tool_calls` is `null`; `tokens_used` and `model` come from the provider
response.

Errors: `400 INVALID_REQUEST`, `400 INVALID_MULTIPART`, `413 PAYLOAD_TOO_LARGE`,
`502 INFERENCE_FAILED`, `503 ENGINE_UNAVAILABLE`.

---

## 4. Device channel admin

### `GET /v1/device/status`

**Loopback only.** Returns the current pairing code/expiry, active sessions and
the bounded audit tail for an admin CLI running on the same host.

The check uses the TCP peer address (`ConnectInfo`), **not** `X-Forwarded-For`,
so a reverse proxy on the same host makes this endpoint reachable for all proxied
clients — do not expose the HTTP port behind an unfiltered proxy.

```bash
curl -s http://127.0.0.1:8080/v1/device/status
```

```json
{
  "device_enabled": true,
  "device_ready": true,
  "pairing": { "code": "K7M2Q9XA", "expires_in_s": 287 },
  "sessions": [
    {
      "session_id": "0f1e...",
      "device_name": "Pixel 8",
      "remote_addr": "192.0.2.10:51544",
      "connected_at_unix_ms": 1730000000000
    }
  ],
  "audit_tail": [
    {
      "time_unix_ms": 1730000001000,
      "session_id": "0f1e...",
      "kind": "connected",
      "detail": "session established"
    }
  ],
  "audit_capacity": 128
}
```

* `pairing.code` auto-rotates when it expires; `--print-pairing` / rotation is
  logged at INFO by the service.
* `audit_tail` is newest-first and capped (`audit_capacity`).
* Audit entries never contain typed text, screenshot bytes, pairing codes or
  tokens (see `ale-device/src/audit.rs`).

Errors: `403 FORBIDDEN` (non-loopback peer), `503 DEVICE_DISABLED`
(`[device].enabled = false`).

---

## 5. Egress proxy

`POST /v1/egress/{chat,vision,tts,asr}` forward requests to an
OpenAI-compatible upstream. See `ale-device` docs for how a terminal obtains a
session token (`SessionReady.token`).

### 5.1 Security model

1. **Authentication** — `Authorization: Bearer <session token>` is mandatory.
   The token must still be valid (`[device].pairing.token_ttl_s`) according to the
   device session service; revoked/expired tokens get `401`.
2. **Policy switch** — `[egress].enabled = false` → `403 EGRESS_DISABLED`.
3. **Host allow-list** — the *resolved* upstream URL host must exactly match one
   entry of `[egress].allowed_hosts` (case-insensitive). Host names only; no
   wildcards; scheme/path/port are ignored for matching. A mismatch is
   `403 EGRESS_DENIED` and is never sent anywhere.
4. **No redirects** — redirect policy is `none()`. A `3xx` from the upstream is
   an error (`502 EGRESS_UPSTREAM_ERROR`), so a redirect cannot escape the
   allow-list.
5. **Credentials** — the upstream API key is read on every request from the
   environment variable named by `[egress].api_key_env` and sent as a Bearer
   header. It is never written to the config file, `/status` or logs. Missing
   variable → `503 EGRESS_NOT_CONFIGURED`.
6. **Size caps** — request bodies are capped by `[egress].max_body_bytes`
   (`413`); upstream response bodies are streamed with a hard cap of
   `[egress].max_response_bytes` and rejected with
   `502 EGRESS_RESPONSE_TOO_LARGE` when exceeded.
7. **No body/secret logging** — only route, upstream host, HTTP status and byte
   counts are logged. Upstream error bodies are not forwarded or logged (they may
   contain prompt fragments).
8. **TLS** — `https` upstreams use rustls; certificate validation cannot be
   disabled from configuration.
9. **Audit** — successful forwards and upstream failures are recorded in the
   device audit log as `egress` entries (session id, host, byte count only).

Egress routes are registered even when egress is disabled so the denial is
explicit; nothing reaches the network in that case.

### 5.2 `POST /v1/egress/chat`

Body is forwarded as-is to `POST {upstream}/v1/chat/completions` with
`model`/`messages`/… expected in OpenAI chat-completions format. `messages` is
required; `"stream": true` is rejected.

```bash
curl -s -X POST http://127.0.0.1:8080/v1/egress/chat \
  -H "Authorization: Bearer $SESSION_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}'
```

Response: the upstream 2xx JSON body is forwarded verbatim (status `200`,
upstream `Content-Type` preserved).

### 5.3 `POST /v1/egress/vision`

Builds an OpenAI chat-completions request with the image embedded as a
`data:<mime>;base64,...` `image_url` part.

```json
{
  "image": "<base64>",
  "mime": "image/png",
  "question": "What is on the screen?",
  "model": "gpt-4o-mini",
  "max_tokens": 512
}
```

* `image` required; `question` (alias `prompt`) defaults to “Describe this image
  in detail.”; `mime` must be `image/*`.
* Forwarded to `POST {upstream}/v1/chat/completions`; response forwarded verbatim.

### 5.4 `POST /v1/egress/tts`

Forwarded to `POST {upstream}/v1/audio/speech`.

```json
{ "text": "hello", "voice": "alloy", "model": "tts-1", "speed": 1.0, "response_format": "wav" }
```

Defaults inserted when omitted: `model=tts-1`, `voice=alloy`,
`response_format=wav`; `speed` must be within `0.25..=4.0`. The response is the
**raw audio** returned by the upstream (`200`, audio content type; falls back to
`audio/wav`).

### 5.5 `POST /v1/egress/asr`

Base64 audio is rebuilt as a multipart upload to
`POST {upstream}/v1/audio/transcriptions`.

```json
{ "audio": "<base64>", "mime": "audio/wav", "language": "en", "model": "whisper-1", "prompt": "…" }
```

Defaults: `model=whisper-1`; the file field is named `file`. The upstream JSON
transcript response is forwarded verbatim.

### 5.6 Egress status codes (in evaluation order)

| Condition | Status | Code |
| --- | --- | --- |
| missing/malformed `Authorization` header | 401 | `UNAUTHORIZED` |
| egress disabled | 403 | `EGRESS_DISABLED` |
| device channel disabled (cannot validate tokens) | 503 | `DEVICE_DISABLED` |
| invalid/expired session token | 401 | `UNAUTHORIZED` |
| body over `max_body_bytes` / malformed JSON | 413 / 400 | `PAYLOAD_TOO_LARGE` / `INVALID_JSON` |
| resolved host not allow-listed | 403 | `EGRESS_DENIED` |
| credential env var missing | 503 | `EGRESS_NOT_CONFIGURED` |
| upstream 4xx/5xx/timeout/redirect | 502 | `EGRESS_UPSTREAM_ERROR` |
| upstream response over cap | 502 | `EGRESS_RESPONSE_TOO_LARGE` |

---

## 6. Configuration

`ale-server` reads one TOML file; every key can be overridden with an
`ALE_SERVER__SECTION__KEY` environment variable (double underscore = nesting).
Overrides are applied **after** the file and unknown keys are hard errors (so a
typo in a systemd unit cannot silently disable a security control). List values
are comma-separated.

```toml
[server]
bind = "0.0.0.0:8080"          # HTTP listener
cors_origin = []               # explicit origin allow-list; empty = no CORS layer

[engine]
enabled = true                 # false => inference routes answer 503
config_path = "/var/lib/ale/config.json"   # optional; omit for ale-core default
secret_store = "auto"          # auto | system | file | env
secret_store_path = "/var/lib/ale/secrets.json"  # optional; file backend location

[device]
enabled = true
bind = "0.0.0.0:37655"
# [device.pairing], [device.capture], [device.automation], [device.limits]

[egress]
enabled = false
upstream = "https://api.openai.com"
allowed_hosts = ["api.openai.com"]
api_key_env = "OPENAI_API_KEY"
timeout_s = 60
max_body_bytes = 1048576
max_response_bytes = 16777216

[logging]
level = "info"                 # tracing filter, e.g. "ale_server=debug,tower_http=warn"
format = "text"                # text | json
```

`[engine].config_path` points at the vendored `ale-core` JSON configuration:
`cloud_api` there drives `/vlm/*` and `/tts/synthesize`, while `transcription`
drives `/asr/transcribe` and `model_scheduler` feeds the local entries of
`/models`.

`[device]` is the frozen `ale_device::DeviceConfig` schema (see
`docs/CONTRACTS.md` / `deploy/config/ale-server.toml`). Validation is strict:
unknown fields, invalid addresses, wildcard CORS origins, duplicate HTTP/device
bond ports, un-allow-listed egress upstreams, embedded URL credentials and
out-of-range limits all fail startup / `--check-config` with a non-zero exit.

### 6.1 Environment override examples

```bash
ALE_SERVER__SERVER__BIND=127.0.0.1:8080
ALE_SERVER__DEVICE__PAIRING__CODE_TTL_S=600
ALE_SERVER__EGRESS__ALLOWED_HOSTS=api.openai.com,api.example.com
ALE_SERVER__EGRESS__ENABLED=true
ALE_SERVER__LOGGING__LEVEL=debug
ALE_SERVER__LOGGING__FORMAT=json
```

Boolean values accept `true/false/1/0/yes/no/on/off`.

`RUST_LOG`, when it parses as a tracing filter, takes precedence over
`[logging].level` (useful for `journalctl` debugging).

### 6.2 Credential store (headless-safe)

The vendored `ale-core` defaults to the system keyring (D-Bus Secret Service),
which usually does not exist on headless Debian arm64. `ale-server` therefore
resolves the credential backend itself and passes it to
`AleEngine::new_with_secret_store`:

| `[engine].secret_store` | Behaviour |
| --- | --- |
| `auto` (default) | Probe the platform keyring (10 s max). On success use it; on any failure log a WARNING and fall back to the file store. |
| `system` | Use the platform keyring; if unusable, inference returns `503 ENGINE_UNAVAILABLE` but the process, `/health` and the device channel keep running. |
| `file` | JSON file at `[engine].secret_store_path` (default `$STATE_DIRECTORY/secrets.json`, i.e. `/var/lib/ale/secrets.json` with systemd `StateDirectory=ale`; fallback `~/.local/state/ale/secrets.json`). Written atomically with `0600` permissions. |
| `env` | Read-only `ALE_SECRET__API_KEY`, `ALE_SECRET__BACKUP_API_KEY`, `ALE_SECRET__TRANSCRIPTION_API_KEY`. |

Keyring absence is never fatal: engine initialization is lazy and its failure is
reported as `503 ENGINE_UNAVAILABLE` + `/health` `degraded`.
`--check-config` does not probe credentials.

### 6.3 CORS

No CORS headers are sent unless `[server].cors_origin` lists explicit origins
(`https://admin.example.com`). `*` and `null` are rejected by validation. When
configured, `GET`, `POST`, `OPTIONS` and the `Authorization`/`Content-Type`
request headers are allowed.

---

## 7. CLI

```text
ale-server [OPTIONS]

Options:
      --config <PATH>   Path to the TOML configuration file (default: /etc/ale/ale-server.toml)
      --check-config    Validate the configuration and exit without binding any port
      --print-pairing   Print a fresh device pairing code and its lifetime, then exit
  -h, --help            Print help
  -V, --version         Print version
```

* `--check-config` prints the redacted configuration summary to **stdout** and
  exits `0`; it never binds a socket and never probes credentials. Invalid
  configs exit non-zero (`1`; malformed CLI usage exits `2`).
* `--print-pairing` builds the device runtime from `[device]`, forces a fresh
  pairing code and prints:

  ```text
  pairing code: K7M2Q9XA
  expires in: 300s
  ```

  It exits non-zero when `[device].enabled = false`. It does not bind ports, so
  it can run while the systemd unit is active.
* Runtime behaviour: SIGINT/SIGTERM trigger graceful shutdown — stop accepting
  HTTP requests (up to 30 s drain), then stop the device listener and cancel
  sessions (up to 10 s). Logs go to stderr in text (journald friendly) or JSON
  (`[logging].format = "json"`).

---

## 8. Limits summary

| Route | Request cap | Other limits |
| --- | --- | --- |
| `/asr/transcribe` | 32 MiB multipart, 32 MiB audio field | engine 120 s init timeout; retry cooldown 30 s |
| `/tts/synthesize` | 1 MiB JSON | 4096 characters; upstream audio response ≤ 32 MiB; upstream timeout from ale-core `cloud_api.timeout` (1-80 s) |
| `/vlm/describe` | 16 MiB multipart | — |
| `/vlm/ask` | 16 MiB multipart | — |
| `/v1/egress/*` | `[egress].max_body_bytes` (default 1 MiB) | `[egress].max_response_bytes` (default 16 MiB), `[egress].timeout_s` (default 60 s) |
| device channel | `[device.limits].max_message_bytes`, `max_connections`, `max_concurrent_requests`, timeouts | see `docs/PROTOCOL.md` |

The inference multipart/JSON caps are compile-time constants in
`ale-server/src/app.rs` (`MAX_AUDIO_BODY_BYTES`, `MAX_IMAGE_BODY_BYTES`,
`MAX_JSON_BODY_BYTES`, `MAX_TTS_AUDIO_BYTES`).
