# Test report - Imperio

Parts, newest first: Phase 5 (security), Phase 4 (Windows integration),
Phase 2 (internet transport), Phase 1-3 (LAN MVP). All on one machine:
Windows 11 Pro 10.0.26200, Intel UHD + NVIDIA Quadro RTX 3000, 1920x1080.

---

# Phase 5 - security hardening

Date: 2026-11-08. Adds device identity pinning, a TOTP second factor, signal
server authorization/rate-limiting, brute-force backoff, and a consent notice.

## What was tested

| Check | Result |
|---|---|
| Unit tests (identity, TOTP, rate limiter, relay token) | 25 client + 2 signal, all pass. TOTP matches RFC 6238 SHA-1 vectors |
| Identity: first connect | `host identity pinned (first use)` with fingerprint, recorded in `%APPDATA%\Imperio\known_hosts` |
| Identity: reconnect | `host identity verified` (same fingerprint) |
| Identity: host key changed | **viewer refuses**: "HOST IDENTITY CHANGED — possible impersonation. Disconnecting." and closes |
| Identity: `--trust-new` after change | re-pins the new key and proceeds |
| Identity: forged signature | `verify_session` rejects a signature from a different key or a different session (unit test) |
| TOTP: correct code | host logs `viewer authenticated; streaming` |
| TOTP: wrong code (`000000`) | host logs `second factor (TOTP) rejected`, no stream |
| Signal: relay bind with wrong token / stranger address | rejected (unit test + live token path in Phase 2 scenario 4) |
| Signal: Connect rate limit | burst of 10 per IP, then blocked, refills ~1/s (unit test) |
| Consent notice | printed on every accepted session ("Only allow people you trust…"); `require_accept` waits for Enter |
| Brute-force backoff | failed auth escalates a sleep (1,2,4,…,30 s) and rotates the session code after 3 |
| Phase 2 + Phase 4 regression | all scenarios still pass |

## Design

- **Identity (TOFU).** Each host holds a persistent Ed25519 key
  (`config.identity_key`). After the encrypted link is up, the host signs the
  session *channel binding* — `SHA256(sorted(send_key, recv_key))`, which both
  peers derive identically — and sends its public key. The signature proves
  key possession *and* liveness in this exact session (a captured signature
  will not verify against a different session's binding). The viewer pins
  `device_id -> key` and refuses a later mismatch. This detects a different
  machine answering for a device ID, or a relay that tries to sit in the
  middle, even if the pairing code leaked.
- **TOTP.** Verified inside the AEAD channel after Hello (SPAKE2 cannot try
  several passwords once the host has sent its message), so the stored
  password alone is not enough for unattended access. ±1 window, constant-time
  compare.
- **Signal server.** Each session carries a random 64-bit token; the relay
  forwards only between the two addresses recorded at Connect time and only
  when the token matches, so a third party cannot attach to someone's relay.
  Per-source-IP token bucket on Connect (10 burst, 1/s) makes brute-forcing
  the 9e8 device-ID space impractical (~2.8 years/IP). Abuse reports are
  logged.

## Not yet (future security work)

- Accounts, non-enumerable IDs, and a company-domain fair-use rule (the plan's
  Phase 5 business side).
- A GUI consent dialog (today the notice is a console line; `require_accept`
  blocks on Enter, which only suits attended console use).
- Signed auto-updates, HSM-backed release signing, external pen test (Phase 6+).
- The pairing code is 6 digits; SPAKE2 limits it to one online guess per
  attempt and the backoff throttles retries, but a longer code or lockout
  policy would strengthen unattended hosts further.

---

# Part 0 - Phase 4: Windows integration

Date: 2026-11-08 (late). Adds config, the Windows service wrapper, cursor
rendering, multi-monitor, clipboard, and input-desktop following.

## What was tested

| Check | Result |
|---|---|
| Build (release) + 19 unit tests | clean, all pass |
| Config file | `C:\ProgramData\Imperio\config.toml` created on first run; `device_id` persisted across runs; `None` fields (password, server) omitted |
| Device ID / code display | zero-padded (`425 922 010`); `--code` shows a session code, `--password` shows "unattended access configured" |
| Encrypted session, direct | streams 60 fps, RTT 1.5 ms from the first second (receive thread now starts before the encoder build) |
| Cursor painter | initializes and streams (GDI `DrawIconEx` onto a GDI-compatible texture) |
| Clipboard sync | thread spawns both sides; wired host↔viewer (degenerate to verify on one machine, which shares one clipboard) |
| Input desktop following | builds; `follow_input_desktop` attaches on the receive thread every 200 ms |
| Multi-monitor | one display on this machine: `StreamInfo` reports `monitor=0 monitors=1`; viewer shortcut Ctrl+Shift+1..N and `SelectMonitor` path build and send |
| Service install, no elevation | clean error: "open SCM (run as administrator?)" |
| Phase 2 regression (all 5 scenarios) | still PASS (input probe, 5% loss, signal direct, relay, wrong code) |

## Encoder latency finding (important)

The capture→present time depends on which GPU drives the display, because
keyed-mutex shared textures are same-adapter only, so the codec device is
built on the capture adapter:

- **Integrated-GPU-driven display (the common laptop case):** Intel Quick
  Sync is selected, ~4.5 ms added latency (as in Phase 2).
- **Discrete-GPU-driven display (this laptop, currently):** the Media
  Foundation **NVENC** transform is selected and buffers a deep pipeline,
  adding ~30-180 ms (varies by run). This is a known MF-NVENC limitation;
  OBS and Sunshine bypass MF and use the NVENC SDK directly for low latency.

Mitigations in place: `IMPERIO_ENCODER=<substr>` forces an encoder by name.
Planned fix: a CPU-copy bridge from the capture adapter to the integrated
encoder, or a direct NVENC-SDK backend, so the low-latency path is available
regardless of which GPU drives the panel. **This does not affect correctness
or throughput (steady 60 fps); it is a latency issue on discrete-primary
displays only.**

## Bugs found and fixed

1. RTT showed a false ~440 ms warmup because the host built the ~420 ms NVENC
   pipeline before starting the thread that answers Ping. The receive thread
   now starts first; RTT reads ~1.5 ms immediately.
2. Device ID printed `425 922 10` (missing zero pad) -> `{:03}`.
3. `--code` was treated as the unattended password; now distinct.
4. `GetImmediateContext` / `GetResource` / `CreateSharedHandle` signature
   fixes for windows-rs 0.62.

## Not yet verified (needs an elevated / multi-seat machine)

- Service install → SCM start → session-helper launch in the console session,
  and relaunch across sign-out / fast-user-switch (requires admin install;
  the code path compiles and the non-elevated error path is clean).
- UAC-prompt / lock-screen input (requires the service running as the machine
  account; `follow_input_desktop` is a no-op for an unprivileged process while
  the secure desktop is up, by design).
- Multi-monitor switching on a machine with 2+ displays.
- Clipboard round-trip between two physical machines.

---

# Part A - Phase 2: encrypted, loss-recovering transport + rendezvous

Date: 2026-11-08 (afternoon). Same machine as Part B. All scenarios run by
`tools/bench/run-phase2-tests.sh` on one machine over loopback; the lossy
proxy (`imperio-bench lossy-proxy`) injects random loss in both directions.

## Scenarios

| # | Scenario | Result |
|---|---|---|
| 1 | Pair with code, authenticate, inject mouse move through the encrypted session (`probe-input`) | **PASS** - cursor landed on (768, 432) exactly, restored afterwards |
| 2 | 5% random loss *each way* between viewer and host, 16 s | **PASS** - 0 frames dropped, 0 datagrams unrecoverable, 685 recovered via NACK, bitrate adapted 8000 -> 6400 -> back up |
| 3 | Device ID lookup via signal server, UDP hole punch | **PASS** - direct path established, 0 loss |
| 4 | Same with `--force-relay` (server forwards ciphertext) | **PASS** - relay bound, ~1 MB forwarded in 5 s, 0 loss, viewer decodes normally |
| 5 | Wrong pairing code | **PASS** - host logs "no authenticated Hello", viewer receives nothing |

Unit tests: 19/19 (SPAKE2 agreement and mismatch, AEAD tamper, replay
window, NACK recovery over real sockets, report parsing, jitter deadline and
keyframe resync, congestion backoff/recovery, pacer timing, framing).

## Measured (release, loopback)

| Metric | Value |
|---|---|
| RTT via Ping/Pong | 0.2-1.8 ms |
| Capture -> present, direct | 4-6 ms |
| Capture -> present, through relay | 4-7 ms |
| Capture -> present, 5% loss | 9-16 ms (NACK recovery adds ~one RTT + 2 ms) |
| Assembler deadline (auto) | 2x RTT + 30 ms = ~33 ms |
| Per-datagram overhead | 1 + 8 + 16 bytes outer, 13 bytes inner |
| Binary size | host 1.41 MB, viewer 1.66 MB, signal server 1.12 MB |

## Bugs found and fixed during Phase 2 testing

1. Frames missed the deadline while still legitimately in flight: with
   pacing, a 150-chunk keyframe takes ~50 ms to send. The deadline now
   counts from the frame's last *progress*, not its first chunk.
2. Loss of a frame's last chunk went undetected until the next frame (up to
   70 ms on an idle desktop). Host now sends two empty end-of-frame trailer
   datagrams so the gap is visible immediately.
3. **Winsock rounds `SO_RCVTIMEO` below 500 ms up to 500 ms.** The viewer's
   3 ms receive timeout never fired, so NACK/deadline housekeeping only ran
   when a datagram arrived. Receive loop is now non-blocking with a 1 ms
   sleep when idle. This was the dominant cause of dropped frames.
4. Windows UDP `recv` returns `ConnectionReset` after an earlier `send_to`
   triggered ICMP port-unreachable; it killed the signal server and the
   proxy. All receive loops now ignore it.
5. Default 64 KB socket buffers dropped bursts even on loopback; all sockets
   now request 4-8 MB and the host paces sends at max(2.5x target, 25 Mbps).
6. Keyframe requests on an idle desktop waited for the next desktop change;
   the host now re-encodes the last frame as a keyframe immediately.

## Known limitations after Phase 2

- Hole punching and relay are verified on loopback only. A two-machine,
  two-network test (and symmetric NAT / CGNAT) is still required.
- Relay is UDP only; the TCP/TLS-443 fallback for UDP-blocking networks is
  not built. No FEC yet (NACK only), so links with RTT > ~100 ms will see
  more deadline drops.
- Signal server has no accounts, rate limiting or relay authorisation yet
  (Phase 5). Device IDs are random 9-digit numbers.
- Congestion control is loss/queue AIMD, not delay-gradient; no probing.
- Single viewer per host; a second connection waits for the first to end.

---

# Part B - Phase 1-3 LAN MVP

Date: 2026-11-08. Machine: Windows 11 Pro 10.0.26200, Intel UHD Graphics +
NVIDIA Quadro RTX 3000 (hybrid laptop), primary display 1920x1080 @ 60 Hz.
Toolchain: Rust 1.99.1 (MSVC), VS 2022 Build Tools. Host and viewer ran on
the same machine over UDP loopback, so network latency is ~0 and the
capture-to-present figure measures the pipeline only.

## What was tested

| Check | Command | Result |
|---|---|---|
| Build (debug + release) | `cargo build`, `cargo build --release` | clean, 0 warnings |
| Unit tests | `cargo test --release` | 4/4 pass (control-message roundtrip; chunk split/reassemble; empty frame; abandoned-frame accounting) |
| Desktop Duplication | `imperio-bench capture 5` | 1920x1080 BGRA, 73 frames / 5 s on an idle desktop, 1.0 dirty rect per frame avg, 0 coalesced, 0 recreates |
| End-to-end stream, debug | host + viewer `--no-input`, 15 s | 60.0 fps sustained, 7.9-8.0 Mbps, keyframe sent on connect |
| End-to-end stream, release | host + viewer `--no-input`, 20 s | 15-17 fps on an idle desktop (frames only when the desktop changes), 1.5-2.3 Mbps |
| Input path | `imperio-bench probe-input` against the running host | **PASS**: cursor moved to (768, 432) exactly; restored to (731, 573) |

## Measured pipeline numbers (release)

| Stage | Measurement |
|---|---|
| Encoder selected | Intel Quick Sync Video H.264 Encoder MFT (hardware, D3D11-bound, async) |
| Encode time per 1080p frame | 1.0-2.0 ms mean, 2-17 ms max (first frame 32 ms warm-up) |
| Decoder | Microsoft H264 Video Decoder MFT, D3D11VA, NV12 output |
| Decode time per frame | 0.3-1.1 ms |
| Capture -> present (same clock) | **3.7-5.3 ms** |
| Bitrate, idle desktop | ~2 Mbps |
| Bitrate, 60 fps active | ~8 Mbps at the 8000 kbps CBR target |
| Binary size | host 1.17 MB, viewer 1.43 MB, bench 1.05 MB (no runtime dependencies beyond Windows) |

Against the plan's scorecard: added LAN latency is well under one frame
(16.7 ms); host binary is 1.2 MB against a <15 MB installer target; office
workload bandwidth is ~2 Mbps against a <1.5 Mbps target (needs
dirty-rect-driven encoding and a lower idle bitrate, Phase 2).

## Bugs found and fixed during testing

1. Hardware encoder raised `MF_E_TRANSFORM_STREAM_CHANGE` on the first
   output; renegotiation was looking for NV12 (decoder logic). Now each
   transform re-selects its own output subtype.
2. After renegotiation the code called `ProcessOutput` again immediately;
   async MFTs return `E_UNEXPECTED` unless a `METransformHaveOutput` event is
   pending. Now waits for the event.
3. `DxgiDuplicationFrameBuffer::as_raw_buffer` includes row padding; the CPU
   path now uses the unpadded accessor.

## Known issues

- **NVIDIA H.264 Encoder MFT fails `ActivateObject` with E_UNEXPECTED** on
  this hybrid-GPU laptop, so the Intel encoder is used. Likely adapter
  selection (the default D3D11 adapter is the iGPU). To investigate: create
  the codec device on the NVIDIA adapter explicitly, or use the NVENC SDK
  directly as the plan intends.
- (Resolved in Part A) No loss recovery, congestion control, encryption,
  authentication or NAT traversal at this stage.
- Frame rate is capture-driven: an idle desktop yields ~15 fps of real
  changes. This is correct (saves bandwidth) but the viewer title will show
  the lower number.
- Cursor is composited into the frame by DWM; no separate cursor-shape
  channel yet (Phase 4).
- Monitor 1 only; UAC prompts and the login screen are not captured
  (requires the SYSTEM helper, Phase 4).

## Raw logs

`bench-results/host.log`, `bench-results/viewer.log` (debug run),
`bench-results/host-release.log`, `bench-results/viewer-release.log`.
