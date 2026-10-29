# ale-device encrypted terminal protocol

Version: **1** (`DEVICE_PROTOCOL_VERSION` in `ale-device/src/protocol.rs`).

This document is the wire contract between an Ale control terminal (phone app,
dedicated hardware, CLI) and the headless computer service (`ale-device`). It
covers the transport, the pairing flow, the Noise handshake, `AME2` framing, the
message catalog, limits, error codes and the threat model.

The implementation lives in `ale-device/src/{crypto,session,protocol}.rs`.

### Relationship to Misaka-Network v4 (desktop)

The product contains a second, unrelated protocol: the vendored
`ale-core::remote` **Misaka-Network v4** used between the desktop app and the
phone app (`misaka-network://` pairing scheme, `_misaka-network._tcp` mDNS, PSK
label `misaka-network-remote-v4`, chunk magic `MSKN`, execution states
`outcome_unknown`/`needs_user`). This repository's terminal protocol is the
**v1** protocol described in this document: it keeps its own 8-character
Crockford pairing code, `ale-device-pairing-v1` PSK label and `AME2` chunk
magic, and is **not wire-compatible** with Misaka-Network v4. Lead decision:
the terminal protocol does not follow the upstream v4 rename; never mix the
two labels, keys or framing schemes in one connection.

## 1. Transport

* TCP port from `device.bind` (default `0.0.0.0:37655`).
* WebSocket, **binary frames only**. Each WebSocket binary frame carries exactly
  one Noise transport message. Text frames are ignored.
* The server sets `max_message_size` / `max_frame_size` to
  `MAX_ENCRYPTED_FRAME_BYTES` = 48 KiB + 16 = **49 168 bytes**; larger WebSocket
  frames are rejected by the WebSocket layer.
* All protocol messages after the Noise handshake are encrypted and
  authenticated. There is **no TLS**: Noise provides confidentiality, integrity,
  replay protection and forward secrecy on top of the raw TCP socket, so running
  TLS in addition is optional (see §9).
* Connection states: `websocket_upgrade -> pairing_handshake -> session_ready ->
  request_loop -> closed`.

## 2. Pairing

### 2.1 Pairing code

* 8 characters of **Crockford base32** (alphabet
  `0123456789ABCDEFGHJKMNPQRSTVWXYZ`), representing 40 random bits.
  `I`, `L`, `O`, `U` are never produced (ambiguous/confusable characters).
* Lifetime from `pairing.code_ttl_s` (default 300 s, allowed 1..=3600).
* The code is printed by the service (`--print-pairing`, the admin status page, or
  the startup log) and typed into the terminal. Terminals should uppercase the
  input (Crockford codes are case-insensitive in practice; this server generates
  uppercase only).
* The code is **single-use**: it is rotated as soon as a session is established,
  and it also rotates automatically when it expires.

### 2.2 PSK derivation

```
psk = HKDF-SHA256(
    salt = "ale-device-pairing-v1",
    ikm  = UTF-8 pairing code (as displayed, e.g. "7Q2M9X4B"),
    info = "",
    L    = 32)
```

The terminal and the server must derive the same 32-byte PSK. The server never
sends the code or PSK over the wire. For audit logs the server records
`psk_fingerprint = lowercase hex of the first 4 bytes of SHA-256(psk)` (8 hex
characters), which is safe to publish and lets an operator correlate failures
with the displayed code.

### 2.3 Pairing state machine

| Event | Result |
| --- | --- |
| code expires (`pairing.code_ttl_s`) | new code, new expiry (auto-rotate) |
| successful pairing | new code, new expiry (single use) |
| admin `rotate_pairing_code()` | new code, new expiry |
| failed Noise handshake | counted against the client IP |
| `pairing.max_attempts` failures from one IP in a 60 s window | that IP is blocked for 60 s |
| `pairing.enabled = false` | new pairing attempts are refused; existing sessions keep working |

Failures are counted per source IP; a successful pairing clears the counter for
that IP. The pairing code itself is **not** rotated on failure (otherwise an
attacker could invalidate the code a user is about to type).

### 2.4 Pairing close codes

The pairing phase has no session key yet, so failures cannot be expressed as an
encrypted `SessionRejected` message. The server closes the WebSocket with a
Close frame; the reason string is stable:

| Close reason | Meaning |
| --- | --- |
| `pairing_disabled` | `pairing.enabled = false` |
| `too_many_attempts` | this IP is in pairing back-off |
| `invalid_handshake` | first frame was not a valid Noise handshake message |
| `pairing_failed` | Noise authentication failed (usually a wrong/expired code) |
| `protocol_incompatible` | `SessionInit.protocol_version` mismatch (`SessionRejected` was sent first) |
| `session_init_required` | first encrypted message was not `SessionInit` (`SessionRejected` was sent first) |

CLI clients should print these reasons verbatim: `pairing_failed` almost always
means the code was mistyped, expired, or already consumed.

## 3. Noise handshake

Pattern: **`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`** (Noise protocol framework,
`snow` implementation). `NNpsk0` has no static keys: the pairing PSK is mixed
into the first handshake message, which authenticates both peers.

```
terminal                                             service
   | -- WS upgrade ------------------------------------> |
   | -- handshake message #1 (raw binary, 0-RTT psk) --> |  SecureChannel::server_respond
   | <----------------- handshake message #2 (raw) ----- |
   | == encrypted transport (both directions) ========= |
   | -- SessionInit (encrypted) -----------------------> |
   | <-- SessionReady / SessionRejected (encrypted) ---- |
```

* Handshake message #1 and #2 are the only **unencrypted** (but PK-authenticated)
  binary frames; they are Noise handshake messages, not JSON.
* Handshake timeout: **10 s** for the WebSocket upgrade, for the first frame and
  for the `SessionInit` read (each phase).
* The first frame must be binary and ≤ 4 KiB.
* A wrong PSK fails inside the first Noise message: the service never learns the
  client's identity, records a `PairingFailed` audit entry and closes with
  `pairing_failed`.

## 4. AME2 framing (chunking)

Noise transport messages are limited to 48 KiB of plaintext. Protocol messages
(small JSON envelopes, but also base64 screenshot payloads) can be larger, so
payloads above `MAX_NOISE_PLAINTEXT` (48 KiB) are split into `AME2` chunks, each
encrypted into its own WebSocket binary frame.

```
chunk plaintext:
  offset  size  field
  0       4     magic = "AME2"
  4       8     message_id (big-endian u64, random per message)
  12      4     chunk index (big-endian u32, 0-based)
  16      4     total chunks (big-endian u32)
  20      n     payload (n <= 48 KiB - 20 = 49 132 bytes)
```

Rules:

* a plaintext ≤ 48 KiB is sent as one unframed Noise message (no `AME2` header);
* a larger message is split into consecutive `AME2` chunks, each ≤ 48 KiB plaintext
  (20-byte header + payload). `total_chunks = ceil(len / 49132)`;
* the receiver reassembles by `message_id` and rejects:
  * an out-of-order chunk (`next_chunk != index`) — `INVALID_CHUNK_SEQUENCE`;
  * a chunk that does not start a message while none is in flight —
    `MISSING_CHUNK_START`;
  * an interleaved chunked message (a second chunk 0 while another message is in
    flight) — `INVALID_CHUNK_SEQUENCE`;
  * a plain single-frame message while a chunked message is in flight —
    `INVALID_CHUNK_SEQUENCE`;
  * a truncated header — `INVALID_CHUNK_HEADER`;
  * a reassembled message above `limits.max_message_bytes` (and above the
    8 MiB protocol ceiling `MAX_MESSAGE_BYTES`) — `MESSAGE_TOO_LARGE`.
* Any `AME2` violation is a protocol violation: the server sends an `Error` frame
  with the mapped code (when the transport is still usable) and closes the
  session.
* A frame larger than 49 168 bytes is rejected before decryption
  (`MESSAGE_TOO_LARGE`).

Terminals **must** implement the same chunking when decoding `CaptureResult`
(encoded screenshots frequently exceed 48 KiB) and when sending any message above
the plaintext limit.

## 5. Message catalog

All JSON messages use a `type` tag (snake_case) and are transported inside the
encrypted Noise channel. Encryption is per frame, chunking is transparent above
the JSON layer.

```
{ "type": "<variant>", ...fields }
```

Request ids are opaque strings chosen by the terminal (UUIDs recommended). They
must be unique among currently in-flight requests; a duplicate id is rejected
with `PROTOCOL_VIOLATION`.

Directions: `T -> S` terminal to service, `S -> T` service to terminal.

### 5.1 Session establishment (`T -> S`)

`SessionInit` — first encrypted message. Sent exactly once.

```json
{ "type": "session_init",
  "protocol_version": 1,
  "device_name": "Pixel 9 / blind-terminal",
  "client_kind": "terminal" }
```

`client_kind`: `terminal` | `mobile` | `cli`.

### 5.2 Session establishment (`S -> T`)

`SessionReady` — sent after a valid `SessionInit`.

```json
{ "type": "session_ready",
  "protocol_version": 1,
  "session_id": "5c3f2d4a-5b3c-4f2c-9d8e-0123456789ab",
  "token": "sV3w...43urlsafe-chars",
  "server_version": "0.2.0",
  "capabilities": {
    "capture": { "available": true, "backend": "grim", "error": null },
    "automation": { "available": true, "backend": "xdotool", "dry_run": false,
                    "allow_high_risk": false, "allow_process_actions": false,
                    "error": null },
    "displays": [ { "id": 0, "name": "eDP-1", "x": 0, "y": 0,
                    "width": 1920, "height": 1080, "scale_factor": 1.0,
                    "primary": true, "backend": "xrandr" } ]
  } }
```

* `token` is a 32-byte random bearer token (base64url without padding) for the
  HTTP egress API. It is valid for `pairing.token_ttl_s` (default 24 h) and
  deliberately outlives the WebSocket session.
* `displays` may be empty when no display tool is installed; captured frames then
  report a synthetic display derived from the image.

`SessionRejected` — sent instead of `SessionReady`, then the socket is closed.

```json
{ "type": "session_rejected",
  "code": "PROTOCOL_INCOMPATIBLE",
  "message": "server speaks protocol version 1, client sent 2" }
```

Codes used here: `PROTOCOL_INCOMPATIBLE`, `PROTOCOL_VIOLATION`.

### 5.3 Capture (`T -> S`, `S -> T`)

```json
{ "type": "capture_request", "request_id": "0b0f...",
  "request": { "display_id": null, "max_width": 1600,
               "format": "png", "fresh": true } }
```

`format`: `jpeg` | `png`. `fresh = false` may reuse a cached frame
(`capture.cache_ttl_ms`). `display_id` selects a display from `SessionReady`.

```json
{ "type": "capture_result", "request_id": "0b0f...",
  "frame": { "frame_id": "9f2a...", "display_id": 0, "mime": "png",
             "width": 1600, "height": 900,
             "captured_at_unix_ms": 1761280000000, "backend": "grim" },
  "data_base64": "iVBORw0KGgo..." }
```

* `data_base64` is standard base64 (not URL-safe) of the encoded image.
* Encoded image size is capped at `MAX_FRAME_BYTES` (6 MiB); the JSON message is
  also capped at `MAX_MESSAGE_BYTES` (8 MiB) and is chunked with `AME2` when
  needed.
* The frame is cached on the service for the action-plan freshness guard
  (`frame_id`).

### 5.4 Displays (`T -> S`, `S -> T`)

```json
{ "type": "display_list_request", "request_id": "…" }
{ "type": "display_list", "request_id": "…", "displays": [ … ] }
```

Display discovery failure is not fatal: the list may be empty. When a screenshot
exists, the captured frame's display list is used as a fallback.

### 5.5 Health (`T -> S`, `S -> T`)

```json
{ "type": "health_request", "request_id": "…" }
{ "type": "health", "request_id": "…",
  "status": { "server_version": "0.2.0", "protocol_version": 1,
              "uptime_s": 1234, "sessions": 1,
              "capture_backend": "grim", "automation_backend": "xdotool" } }
```

### 5.6 Action plans (`T -> S`, `S -> T`)

```json
{ "type": "action_plan_request", "request_id": "…",
  "plan": { "actions": [ { "type": "click", "x": 640.0, "y": 360.0,
                           "button": "left" } ],
            "risk_level": "medium",
            "explanation": "click the OK button",
            "requires_confirmation": false },
  "approved": true,
  "expected_frame_id": "9f2a…" }
```

The plan format is `ale_core::actions::ActionPlan`; actions include `click`,
`double_click`, `mouse_move`, `scroll`, `type`, `key`, `wait`, `open_app`,
`close_app`, `open_url`, `file_operation`, `controlled_test_click`. The service
always re-validates the plan (`plan.validate()`, risk re-derivation, policy
gates); untrusted `risk_level`/`requires_confirmation` fields are ignored.

**Frame freshness guard**: when `expected_frame_id` is `Some(id)`:

* `id` must equal the most recent `CaptureService::last_frame_id()`, otherwise
  `Error { code: "STALE_FRAME" }`;
* that frame must be younger than `automation.frame_max_age_ms` (default 5 s),
  otherwise `STALE_FRAME`.

If `expected_frame_id` is `null`/omitted the freshness check is skipped. A
terminal that derived the plan from a screenshot **should always send the id** of
the frame it used.

Results:

```json
{ "type": "action_result", "request_id": "…", "actions_executed": 2,
  "state": "completed", "dry_run": false }
```

`state`: `completed` | `cancelled` | `failed`. A cancelled plan reports
`state: "cancelled"` and an `action_result` (not an `error`) because partial
execution may have happened. `ActionProgress` frames are optional and currently
not sent.

### 5.7 Cancellation (`T -> S`)

```json
{ "type": "cancel", "request_id": "…" }
```

Cancels the matching in-flight request. Action execution is cooperative: the
executor checks the per-request `ExecutionControl` before/between actions and
inside `wait`, kills running child processes and reports
`action_result.state = "cancelled"` promptly. Capture/display/health requests
cannot be aborted mid-backend, but their results are still bounded by the
request timeout. Cancelling an unknown request id is a no-op.

### 5.8 Keepalive / control (`T -> S`, `S -> T`)

```json
{ "type": "ping", "request_id": "…", "nonce": 12345 }
{ "type": "pong", "request_id": "…", "nonce": 12345 }
```

`Ping` only exercises the session (it is not rate-limited with requests); the
server echoes the nonce. Terminals should send it well within
`limits.idle_timeout_s` (default 120 s) to keep an otherwise idle session open.

### 5.9 Errors (`S -> T`)

```json
{ "type": "error", "request_id": "…", "code": "STALE_FRAME",
  "message": "expected frame … but the most recent frame is …" }
```

`request_id` is `null` for connection-level errors (e.g. decrypt failures). See
§7 for the code table.

## 6. Limits and session policy

All bounds come from `DeviceConfig` (`ale-device/src/config.rs`):

| Limit | Config | Default |
| --- | --- | --- |
| Pairing code lifetime | `pairing.code_ttl_s` | 300 s (max 3600) |
| Failed pairing attempts per IP | `pairing.max_attempts` | 5 per 60 s |
| Pairing back-off | fixed | 60 s |
| Egress token lifetime | `pairing.token_ttl_s` | 86 400 s |
| Concurrent sessions | `limits.max_connections` | 4 (max 32) |
| Unauthenticated connections per IP | `limits.max_connections_per_ip` | 2 (max 32) |
| Concurrent requests per service | `limits.max_concurrent_requests` | 2 (max 16) |
| Reassembled message size | `limits.max_message_bytes` | 8 MiB (hard ceiling 8 MiB) |
| Idle timeout (no frames) | `limits.idle_timeout_s` | 120 s |
| Request timeout | `limits.request_timeout_s` | 90 s |
| Request rate per session | fixed | 30 requests / 60 s |
| Plaintext per Noise message | fixed | 48 KiB |
| Encrypted frame | fixed | 49 168 B |
| Screenshot payload | fixed (`MAX_FRAME_BYTES`) | 6 MiB |
| Frame freshness | `automation.frame_max_age_ms` | 5 000 ms |
| Capture command timeout | `capture.timeout_ms` | 5 000 ms |
| Automation plan timeout | `automation.timeout_ms` | 30 000 ms |

Behaviour:

* connections above `max_connections` are accepted and immediately dropped (the
  server does not upgrade them);
* connections from one peer IP above `max_connections_per_ip` are accepted and
  immediately dropped before the Noise handshake; this stops a single peer from
  occupying every session slot with unauthenticated sockets (the handshake phase
  is bounded by a 10 s timeout);
* requests above `max_concurrent_requests` in flight are rejected with
  `RATE_LIMITED` (the terminal should retry after the previous response);
* the per-session request budget (30/min) returns `RATE_LIMITED` for excess
  requests; `Ping`, `Pong`, `Cancel` are not counted;
* an in-flight request that exceeds `request_timeout_s` gets
  `Error { code: "REQUEST_TIMEOUT" }` and its `ExecutionControl` is cancelled;
* a session with no frames for `idle_timeout_s` is closed (`Away` close code);
* egress tokens are bound to a session: when the session ends (peer close, idle
  timeout, protocol violation, server shutdown) every token issued to it is
  revoked immediately, independent of `pairing.token_ttl_s`;
* protocol violations (unexpected message, bad chunking, decrypt failure,
  duplicate request id) produce an `Error` and close the session; three
  unexpected messages in a row also close it.

## 7. Error codes

`Error.code` values are stable machine-readable strings:

| Code | Meaning |
| --- | --- |
| `PROTOCOL_INCOMPATIBLE` | `SessionInit.protocol_version` is not supported |
| `NOT_PAIRED` | reserved (pairing/session not established) |
| `PAIRING_DISABLED` | pairing disabled by config |
| `PAIRING_EXPIRED` | pairing code expired |
| `TOO_MANY_ATTEMPTS` | pairing back-off active for the client IP |
| `REQUEST_TIMEOUT` | handler exceeded `request_timeout_s` |
| `CANCELLED` | request cancelled |
| `STALE_FRAME` | action plan references a missing/outdated screenshot |
| `DENIED` | policy denial (risk, capability, process action, …) |
| `CAPABILITY_UNAVAILABLE` | capture/automation backend unavailable |
| `BACKEND_ERROR` | external command/capture/automation failed |
| `MESSAGE_TOO_LARGE` | frame/message exceeds a size limit |
| `RATE_LIMITED` | per-session rate limit or concurrency limit |
| `PROTOCOL_VIOLATION` | malformed message, bad chunk sequence, unexpected message |
| `CONFIG_INVALID` | internal configuration problem |
| `INVALID_JSON` | payload is not valid protocol JSON |
| `IO_ERROR` | local I/O failure |

`DeviceError::code()` is the source of truth; `MESSAGE_TOO_LARGE` is additionally
preserved for chunking failures whose detail starts with that token.

## 8. Versioning

* `DEVICE_PROTOCOL_VERSION` (currently `1`) is bumped for any incompatible change
  to the message catalog or framing.
* This version number is **independent** from the desktop Misaka-Network version
  (currently v4). They are separate protocols and can advance independently; do
  not rename this protocol's `AME2`/`ale-device-pairing-v1` identifiers to match
  the desktop v4 names.
* Terminals must send their version in `SessionInit`. A mismatch produces
  `SessionRejected { code: "PROTOCOL_INCOMPATIBLE" }` plus a
  `protocol_incompatible` close; the terminal should upgrade or show an error.
* Adding new optional message variants/fields is a compatible change; unknown
  `DeviceMessage` variants cause `PROTOCOL_VIOLATION` on the current server, so
  terminals must negotiate by version before using new messages.

## 9. Threat model

Assumed environment: the terminal and the computer are on the **same LAN or an
otherwise trusted network path**. The pairing code is transferred out of band
(displayed on the computer, typed into the terminal).

Properties provided by this design:

* **Confidentiality and integrity** of all session messages: Noise
  `ChaChaPoly1305` AEAD over TCP, keyed by the pairing PSK. Screenshots, action
  plans and tokens are not readable on the wire.
* **Mutual authentication** by knowledge of the pairing code (40 bits). Both
  peers prove PSK possession during the handshake.
* **Forward secrecy**: the handshake uses ephemeral X25519 keys; a later PSK
  compromise does not decrypt recorded past sessions.
* **Replay/reorder protection**: Noise transport nonces plus strict `AME2`
  ordering reject replayed or reordered frames.
* **Online guessing is bounded**: a code lives at most 1 hour (default 5 min),
  and each IP gets at most `pairing.max_attempts` failures per 60 s before a
  60 s back-off. 40 bits is far more than an online attacker can enumerate in the
  TTL; offline brute force of a captured handshake is bounded by the same TTL and
  is not practical without the code.
* **No action without a fresh frame**: action plans referencing a screenshot must
  name the current cached frame and be younger than `frame_max_age_ms`.
* **Policy gates**: the automation service re-validates every plan (risk,
  high-risk/process-action gates, confirmation, action-count and coordinate
  bounds); `ControlledTestClick` is always rejected.
* **Bounded resource use**: connection/request/message/frame/time limits above
  prevent trivial memory or CPU exhaustion from one client.
* **Audit trail** (`AuditKind`: `Connected`, `Disconnected`, `PairingFailed`,
  `Capture`, `Action`, `Denied`, `Cancelled`, `Error`, `Egress`) records
  session ids and frame ids but never pairing codes, tokens, typed text or image
  bytes.

Explicit non-goals / residual risks:

* **No TLS in this layer** (Noise is the encryption layer). The HTTP egress API is
  a separate listener and should be bound to loopback/behind TLS: its bearer
  tokens are only as safe as that listener.
* The protocol assumes the TCP path is not fully attacker-controlled; the
  handshake and rate limits assume a single network hop, not an active
  man-in-the-middle able to proxy and observe the terminal (Noise still protects
  content, but DoS and metadata are not defended).
* The pairing code is only 40 bits. Anyone who can read the code (shoulder
  surfing, screenshot of the admin UI) can pair; the code is single-use and short
  lived to limit that window.
* Denial of service (packet floods, repeated valid-but-abusive pairings) is only
  partially mitigated; systemd/firewall level protection is expected on the
  host.
* The service trusts the OS/display session for capture and automation
  availability; it never links X11/Wayland and cannot sandbox the external
  commands it runs beyond timeouts and the policy gates above.

## 10. Implementing a terminal (client)

1. **Read the pairing code** from the user (8 Crockford chars, uppercase).
2. **Open** `ws://<host>:<port>/` (or `wss://` through a TLS proxy).
3. **Derive** `psk = HKDF-SHA256("ale-device-pairing-v1", code)` (§2.2) and run
   the Noise initiator with pattern
   `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` and `.psk(0, psk)`.
4. Send handshake message #1 as the first **binary** frame; read the reply and
   switch to transport mode (10 s timeout each).
5. Send the encrypted `SessionInit`; read `SessionReady` (or `SessionRejected` +
   close). Persist `token` and `session_id`; check `capabilities` and
   `protocol_version` before offering capture/actions.
6. Implement **encoder**: serialize JSON, if ≤ 48 KiB encrypt one frame, else
   split into `AME2` chunks (49132 payload bytes each) and encrypt each chunk.
7. Implement **decoder**: decrypt each binary frame, reassemble `AME2` chunks
   strictly, then `serde`/JSON-decode. Capture results are frequently chunked.
8. Send `capture_request{fresh:true}` to obtain a frame, display it with
   `frame.frame_id` associated, plan actions, then send `action_plan_request`
   with `expected_frame_id = frame.frame_id` and `approved` only after the user
   confirmed anything the plan flags as medium/high risk.
9. Implement `cancel` for long plans; handle `action_result.state = cancelled`
   and `error` frames without treating them as transport failures.
10. Send `ping` periodically (e.g. every 30 s) on an idle session; reconnect with
    a **new pairing code** after any close (the old code is consumed/rotated).
11. Never log the pairing code, the PSK, the token or screenshot bytes. Store the
    egress token like a password.
12. On `pairing_failed`/`protocol_incompatible` close reasons, surface a clear
    error to the user; on `too_many_attempts`, wait before retrying.

For a Rust reference terminal, see `ale-device/tests/session_support/mod.rs`
(the test client used by `ale-device/tests/session_*.rs`): it implements the
initiator handshake, `AME2` encoder/decoder and the request flow against a real
loopback listener.

## 11. References

* `docs/CONTRACTS.md` — repository contracts and module ownership.
* `docs/ARCHITECTURE.md` — service architecture.
* `ale-device/src/crypto.rs` — Noise setup, `AME2` framing, PSK derivation.
* `ale-device/src/session.rs` — pairing state, session loop, cancellation, limits.
* `ale-device/src/protocol.rs` — message definitions and error codes.
