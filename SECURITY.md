# Security

Imperio gives one computer control of another, so security problems matter
more here than in most software. This page says how to report one, what the
design protects against, and — just as important — what it does not yet.

## Reporting a vulnerability

Email **admin@developerinayat.com** with "Imperio security" in the subject.
Please do not open a public issue for something exploitable. Include the
version (bottom right of the home window), what you did, and what happened.
You will get an answer within a few days, and credit in the release notes if
you want it.

## What protects a session

| Layer | What it does |
| --- | --- |
| Pairing | SPAKE2 from the code shown on the host. Someone watching the network learns nothing about the code and gets one guess per attempt; the host backs off after failures and issues a new code after three. |
| Encryption | ChaCha20-Poly1305, one key per direction, a fresh key every session. Relays and the signal server only ever carry ciphertext. A 4096-entry replay window rejects replayed datagrams. |
| Key confirmation | The viewer only opens a session window once a datagram from the host has opened under the session key, so a wrong code is reported as a wrong code. |
| Authorization gate | Holding the session key proves the peer knew the code, nothing more. Until the optional second factor (TOTP) and the optional "Allow?" prompt have also passed, the host ignores every input, clipboard and display message from that peer, and sends it no clipboard. |
| Host identity | Each host has a long-term Ed25519 key and signs the session's channel binding with it. The viewer pins the key on first use and refuses a different key later (the SSH `known_hosts` model). When connecting by Device ID, the host must present the ID that was asked for. |
| Open mode | "No code for my own Tailscale devices" uses a public pairing secret, so the network is the gate: only tailnet (100.64.0.0/10) and loopback peers are accepted, however the peer was found. |
| Signal server | Device IDs are bound to the device's identity key permanently (across the device being offline and server restarts). Clients accept signaling only from the server's address, and probe only genuine LAN addresses plus the address the server observed. |
| Updates | The update manifest is signed with an offline Ed25519 key and names the SHA-256 of the download; both are checked before anything is run. Only newer versions are accepted. |
| Licences | Verified offline against the same public key. No licence server, no telemetry. |

## Known limits (read these)

- **The binary is not code-signed yet.** Windows SmartScreen warns on first
  run. Verify the download's SHA-256 against `latest.json` if in doubt.
- **The config file is readable by other accounts on the same PC.**
  `%ProgramData%\Imperio\config.toml` holds the device identity seed and, if
  you set them, the unattended password and TOTP secret, in plain text.
  On a shared computer, another local user can read them. Do not configure an
  unattended password on a PC that people you do not trust can log in to.
- **The six-digit code is only as strong as its lifetime.** It is meant to be
  read out and used within minutes. For standing access use a long password
  plus TOTP, or open mode behind Tailscale.
- **Trust on first use means the first connection is the one to get right.**
  If the very first connection to a host is intercepted by someone who also
  knows the code, their key is what gets pinned.
- **Anyone who can reach the host's UDP port can make it busy.** Garbage
  handshakes cannot get in, but they can trigger the back-off and code
  rotation that legitimate users then have to wait out.
- **A remote-control tool is a scammer's favourite.** The app warns on every
  incoming session and can require a Yes click. That warning is the last line
  of defence, not a guarantee.
- **No independent audit yet.** The cryptography is assembled from reviewed
  RustCrypto / dalek crates, but the protocol built on them has not been
  examined by an outside party.

## Scope

In scope: this repository (the client, the home app, the updater, the
website under `site/`) and the wire protocol. The hosted signal/relay service
is not public yet.
