# NVRDesk

**NVRDesk** is a lightweight, cross-platform, open-source virtual IP camera written in Rust (MIT License). It turns desktops, screens, POS terminals, webcams, and microphones into standard **ONVIF / RTSP** IP cameras discoverable and recordable by any NVR or VMS (Hikvision, Dahua, Milestone, Frigate, Blue Iris, ONVIF Device Manager, VLC, etc.).

Designed primarily for **Point-of-Sale (POS) visual auditing** and unattended 24/7 operation, NVRDesk focuses on minimal CPU/RAM usage, zero-idle encoding, and clear operator activity overlays.

## Current Status: Phase 1 (ONVIF Profile S MVP + POS Input Overlays)

- [x] **Phase 0:** Cargo workspace, cross-platform CI (Windows/Linux/macOS), screen+cursor capture (`xcap`), SIMD I420 (`yuvutils-rs`), `openh264`, RTSP 1.0 + RFC 6184 RTP
- [x] **Phase 1:**
  - Dual-stream (`MainStream` on `/stream1` + `SubStream` on `/stream2`, independent demand-driven encoding)
  - Global mouse click ripples, cursor highlight ring, and keystroke OSD (`nvrdesk-input`) rendered crisply in **both** Main and Sub streams
  - POS privacy modes (`masked_digits`, `shortcuts_and_special`, `all`, `off`) and sensitive-window auto-pause (`excluded_windows`)
  - RTSP HTTP Digest authentication & idle grace period
  - **ONVIF Profile S** server (`nvrdesk-onvif`): WS-Discovery (`239.255.255.250:3702`), WS-Security (`PasswordDigest`), Device & Media SOAP services, and HTTP JPEG snapshots (`/snapshot/main`, `/snapshot/sub`)
  - TOML configuration (`--config nvrdesk.toml` / `--print-config`)
- [ ] **Phase 2:** Embedded Web UI, System Tray, Hardware encoders (WMF/VideoToolbox/VA-API), optional FFmpeg backend, Webcam & Microphone, local ring-buffer recording
- [ ] **Phase 3:** POS dynamic text OSD API, ONVIF Profile T (H.265/HTTPS/RTSPS), Motion/Events

See [docs/ROADMAP.md](docs/ROADMAP.md) for the full technical plan.

## Workspace Architecture

```text
crates/
├── nvrdesk-core       # Domain types, TOML config schema, overlay snapshots, errors
├── nvrdesk-capture    # CaptureSource trait, cross-platform screen+cursor (xcap) & test pattern
├── nvrdesk-input      # Global keyboard/mouse hook (rdevin), coordinate mapping & POS privacy rules
├── nvrdesk-pipeline   # Frame scaling, AA click ripples, Noto Sans Mono OSD, JPEG snapshot & SIMD I420
├── nvrdesk-codec      # VideoEncoder trait & OpenH264 encoder (NAL/SPS/PPS extraction)
├── nvrdesk-rtsp       # Multi-stream RTSP 1.0 server, HTTP Digest auth & RFC 6184 H.264 RTP packetizer
├── nvrdesk-onvif      # WS-Discovery (UDP 3702), WS-Security, Device/Media SOAP & JPEG snapshots
└── nvrdesk-cli        # CLI orchestrator (`nvrdesk`)
```

## Quick Start

### Prerequisites

- **Rust** 1.88+ (`rustup` stable)
- **Linux only:** `libpipewire-0.3-dev`, `libclang-dev`, `libdbus-1-dev`, `libxcb1-dev`, `libxcb-randr0-dev`, `libxcb-shm0-dev`, `libx11-dev`, `libxtst-dev`, `libxi-dev`, `pkg-config`
- **macOS only:** Grant *Screen Recording* and *Accessibility / Input Monitoring* permissions to your terminal/binary in **System Settings -> Privacy & Security**

### Run

Start NVRDesk with Dual-Stream RTSP + ONVIF Profile S + POS Input Overlays:

```bash
cargo run --release -p nvrdesk-cli -- --port 8554 --http-port 8080
```

With authentication enabled (recommended for NVR/VMS deployment):

```bash
cargo run --release -p nvrdesk-cli -- --port 8554 --http-port 8080 --username admin --password secret
```

Generate a sample TOML configuration file:

```bash
cargo run --release -p nvrdesk-cli -- --print-config > nvrdesk.toml
cargo run --release -p nvrdesk-cli -- --config nvrdesk.toml
```

### Endpoints

- **MainStream RTSP:** `rtsp://<host>:8554/stream1` (alias `/stream`)
- **SubStream RTSP:** `rtsp://<host>:8554/stream2`
- **ONVIF Device Service:** `http://<host>:8080/onvif/device_service`
- **HTTP Snapshots:** `http://<host>:8080/snapshot/main` and `http://<host>:8080/snapshot/sub`

### Key CLI Flags

| Flag | Default | Description |
| :--- | :--- | :--- |
| `--config` | none | Load settings from a TOML config file |
| `--print-config` | `false` | Print effective TOML configuration and exit |
| `--port` | `8554` | RTSP listening port |
| `--http-port` | `8080` | ONVIF SOAP & HTTP Snapshot port |
| `--source` | `screen` | Capture source (`screen` or `test-pattern`) |
| `--display` | `0` | Display index when using `screen` |
| `--fps` | `15` | MainStream target FPS |
| `--bitrate` | `2000000` | MainStream target bitrate in bps |
| `--max-width` | `1920` | MainStream max output width |
| `--keystroke-mode` | `masked-digits` | Keystroke OSD mode (`all`, `shortcuts-and-special`, `masked-digits`, `off`) |
| `--no-substream` | `false` | Disable SubStream (`/stream2`) |
| `--no-overlays` | `false` | Disable click & keystroke overlays |
| `--no-onvif` | `false` | Disable ONVIF HTTP/SOAP & WS-Discovery |
| `--username` / `--password` | none | Credentials for RTSP Digest & ONVIF WS-Security |

## License

Licensed under the [MIT License](LICENSE).
