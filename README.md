# Imperio

Low-latency remote desktop for Windows. Open client, closed service.

This repository contains the **client-side** code (host agent, viewer, media
pipeline, wire protocol). The signaling, relay, account and billing services
live in a separate private repository.

## Design principles

1. **Trust is the product.** Plain-language fair-use policy, no behavioural
   "commercial use" detection, signed binaries, end-to-end encryption with
   pinned per-device keys, relays that see only ciphertext.
2. **Measured, not marketed.** Latency, connection time and bandwidth numbers
   are published with a reproducible methodology (`tools/bench`).
3. **Clean room.** No code from AGPL/GPL remote desktop projects (RustDesk,
   Sunshine, TigerVNC, ...). Architecture ideas are fair game; code is not.

## Layout

| Path | Purpose |
|---|---|
| `crates/capture` | Screen capture (DXGI Desktop Duplication, WGC fallback), cursor shape, dirty rects |
| `crates/codec` | Hardware encode/decode (NVENC / AMF / QuickSync), software fallback (VP9/AV1), D3D11 colour conversion |
| `crates/protocol` | Wire format: frames, input events, clipboard, control messages |
| `crates/transport` | ICE connectivity, custom UDP media protocol (NACK + FEC + congestion control), TCP/443 relay fallback |
| `apps/host` | Windows service + console-session helper (capture, input injection, UAC/login desktop) |
| `apps/viewer` | Desktop viewer (decode, render, input send) |
| `tools/bench` | Latency / bandwidth measurement harness |

## Status

**Phases 1-3 (media pipeline) and Phase 2 (internet transport) work and are
tested** (2026-11-08).

Pipeline: Desktop Duplication -> GPU BGRA->NV12 -> Media Foundation hardware
H.264 (Intel Quick Sync verified; NVIDIA MFT activation fails on this hybrid
laptop) -> encrypted UDP -> hardware decode -> flip-model present, with the
mouse/keyboard return path. Same-machine: 60 fps, encode ~1 ms, decode ~1 ms,
capture-to-present ~4.5 ms.

Transport: 6-digit pairing code -> SPAKE2 -> ChaCha20-Poly1305 per direction
(relays and the signal server see only ciphertext); sequence numbers + NACK
retransmission + end-of-frame trailers; send pacing; receiver reports driving
adaptive bitrate; jitter assembler with keyframe resync; 9-digit device IDs,
rendezvous server, UDP hole punching, relay fallback. At 5% bidirectional
packet loss: 0 dropped frames.

Windows integration (Phase 4): `C:\ProgramData\Imperio\config.toml`; host
cursor drawn into the stream; multi-monitor with viewer-side switching
(Ctrl+Shift+1..N); two-way clipboard text sync; unattended password or
rotating session code; a Windows service wrapper (`install-service`) that
runs a streaming helper in the active console session and follows the input
desktop for UAC / lock screen.

Security (Phase 5): persistent Ed25519 device identity with trust-on-first-use
pinning (a changed host key is refused — the SSH known-hosts model); optional
TOTP second factor for unattended access; signal server with per-session relay
tokens, relay forwarding restricted to the session's two addresses, per-IP
Connect rate limiting, and abuse reports; brute-force backoff and a consent /
scam-awareness notice on every incoming session. See `docs/test-report.md`.

Since then: one `Imperio.exe` with a home window (share this computer /
connect to another, saved computers, tray icon), host audio, MP4 session
recording, signed self-updates, offline Pro licence keys, server-assigned
device IDs bound to the device key, and a CPU bridge to the integrated GPU's
low-latency encoder on discrete-GPU-primary displays.

Hardening pass (0.3.12): nothing a peer sends acts on the host until the
second factor and the consent prompt have passed; a wrong code, a refusal or
a lost connection is reported to the person connecting instead of leaving an
empty window; clipboard texts of any size up to 256 KB sync; held keys are
released when a session ends; the stream recovers when the desktop
duplication is lost (elevation prompt, resolution change). See `SECURITY.md`
for the threat model and the known limits.

Not yet: Opus audio, an installer and code signing, accounts, a public
relay, and a TCP/443 fallback.

## Running

Most people just run `Imperio.exe` on both computers. The command-line
programs below are the same engine without the home window.

```
cargo build --release
# machine to be controlled: prints its Device ID and a 6-digit code
target\release\imperio-host.exe [--port 7700] [--server signal-host:7800] [--bitrate-kbps 8000] [--fps 60]

# controlling machine, same LAN (direct):
target\release\imperio-viewer.exe <host-ip>:7700 --code 123456
# controlling machine, anywhere (via the signal server):
target\release\imperio-viewer.exe --id 123456789 --server signal-host:7800 --code 123456
```

Host flags: `--password PW` (persistent unattended access), `--monitor N`,
`--no-cursor`, `--no-clipboard`, `--bitrate-kbps N`, `--fps N`. Viewer flags:
`--no-input` (view only), `--force-relay`, `--totp 123456` (when the host
requires a second factor), `--trust-new` (accept a changed host identity key).
In the viewer, Ctrl+Shift+1..N switches the host display. The signal server
lives in the private `imperio-cloud/signal` crate (`imperio-signal.exe
--port 7800`). TOTP and consent are set in `config.toml` (`totp_secret`,
`require_accept`).

Service (needs an elevated prompt):

```
imperio-host.exe install-service      # auto-start as LocalSystem
imperio-host.exe uninstall-service
```

## Building and testing

Requires Rust (MSVC toolchain) and Visual Studio Build Tools with the C++ workload.

```
cargo build --release
cargo test
target\release\imperio-bench.exe capture 5                        # Desktop Duplication stats
target\release\imperio-bench.exe probe-input 127.0.0.1:7700 <code> # pairs and checks SendInput
target\release\imperio-bench.exe lossy-proxy 7701 127.0.0.1:7700 5 # 5% loss proxy for NACK testing
bash tools/bench/run-e2e-tests.sh                                 # 21 end-to-end checks, one machine
bash tools/bench/run-phase2-tests.sh                              # loss / relay / latency scenarios
```

`run-e2e-tests.sh` covers pairing, wrong codes, identity pinning, TOTP, the
authorization gate, and the signal server (direct, relay, ID binding). Both
scripts run against a throwaway profile (`IMPERIO_DATA_DIR`), never your real
configuration. Set `IMPERIO_DATA_DIR` yourself to keep Imperio's settings in a
folder of your choice (a portable copy on a USB stick, for example).

## License

MIT OR Apache-2.0, at your option. See `LICENSE-MIT` and `LICENSE-APACHE`.
