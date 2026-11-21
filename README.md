# NVRDesk

**NVRDesk** is a lightweight, cross-platform, open-source virtual IP camera written in Rust (MIT License). It turns desktops, screens, POS terminals, webcams, and microphones into standard **ONVIF / RTSP** IP cameras discoverable and recordable by any NVR or VMS (Hikvision, Dahua, Milestone, Frigate, Blue Iris, ONVIF Device Manager, VLC, etc.).

Designed primarily for **Point-of-Sale (POS) visual auditing** and unattended 24/7 operation, NVRDesk focuses on minimal CPU/RAM usage, zero-idle encoding, and clear operator activity overlays.

## Current Status: Phase 2 (Daemon, Embedded Web UI, System Tray, Webcam/Mic & Local Recording)

- [x] **Phase 0:** Cargo workspace, cross-platform CI (Windows/Linux/macOS), screen+cursor capture (`xcap`), SIMD I420 (`yuvutils-rs`), `openh264`, RTSP 1.0 + RFC 6184 RTP
- [x] **Phase 1:**
  - Dual-stream (`MainStream` on `/stream1` + `SubStream` on `/stream2`, independent demand-driven encoding)
  - Global mouse click ripples (visible only on click) and borderless keystroke OSD (`nvrdesk-input`) rendered crisply in **both** Main and Sub streams
  - POS privacy modes (`masked_digits`, `shortcuts_and_special`, `all`, `off`) and sensitive-window auto-pause (`excluded_windows`)
  - RTSP HTTP Digest authentication & idle grace period
  - **ONVIF Profile S** server (`nvrdesk-onvif`): WS-Discovery (`239.255.255.250:3702`), WS-Security (`PasswordDigest`), Device & Media SOAP services, and HTTP JPEG snapshots (`/snapshot/main`, `/snapshot/sub`)
  - TOML configuration (`--config nvrdesk.toml` / `--print-config`)
- [x] **Phase 2:**
  - **Daemon & Embedded Web UI** (`nvrdesk-daemon` + `web-ui/` via `rust-embed` sharing HTTP port `8080`): live MJPEG preview (`/preview/main`, `/preview/sub`), telemetry/status (`/api/v1/status`), hardware device inventory (`/api/v1/devices`), live config save & hot-reload (`/api/v1/config`), and pause/resume controls
  - **System Tray** (`nvrdesk-tray` via `tray-icon` + Linux `ksni`): Open Web UI, Pause/Resume streaming, and Quit across Windows, Linux, and macOS
  - **Encoder Backends** (`nvrdesk-codec`): `OpenH264` software fallback + optional external **FFmpeg CLI/pipe backend** (`--encoder auto|openh264|ffmpeg`) with auto-probed OS hardware acceleration (`h264_videotoolbox`, `h264_nvenc`, `h264_qsv`, `h264_amf`, `h264_vaapi`, `libx264`)
  - **Webcam & Microphone**: Optional `nokhwa` webcam capture (Picture-in-Picture overlay or dedicated `/webcam` RTSP channel) + `cpal` microphone capture encoded to G.711 `PCMU`/`PCMA` (`trackID=1` in RTSP SDP)
  - **Local Ring-Buffer Recorder**: Segmented Annex-B `.h264` recording (`--record`) with automatic disk quota eviction
- [ ] **Phase 3:** POS dynamic text OSD API, ONVIF Profile T (H.265/HTTPS/RTSPS), Motion/Events

See [docs/ROADMAP.md](docs/ROADMAP.md) for the full technical plan.

## Workspace Architecture

```text
crates/
├── nvrdesk-core       # Domain types, TOML config schema, overlay snapshots, device inventory
├── nvrdesk-capture    # Cross-platform screen+cursor (xcap), webcam (nokhwa), audio (cpal) & test pattern
├── nvrdesk-input      # Global keyboard/mouse hook (rdevin), coordinate mapping & POS privacy rules
├── nvrdesk-pipeline   # Frame scaling, webcam PiP, AA click ripples, Noto Sans Mono OSD, JPEG & SIMD I420
├── nvrdesk-codec      # VideoEncoder (OpenH264 + optional FFmpeg HW/CLI pipe) & G.711 PCMU/PCMA audio encoder
├── nvrdesk-rtsp       # Multi-stream RTSP 1.0 server (H.264 + G.711 audio), HTTP Digest auth & RTP packetizers
├── nvrdesk-onvif      # WS-Discovery (UDP 3702), WS-Security, Device/Media SOAP & JPEG snapshots
├── nvrdesk-daemon     # Daemon runtime, hot-reload worker, RingRecorder & Axum Embedded Web UI + REST API
├── nvrdesk-tray       # Cross-platform System Tray menu (Open Web UI, Pause/Resume, Quit)
└── nvrdesk-cli        # CLI & tray orchestrator (`nvrdesk`)
```

## Quick Start

### Prerequisites

- **Rust** 1.88+ (`rustup` stable)
- **Linux only:** `libpipewire-0.3-dev`, `libclang-dev`, `libdbus-1-dev`, `libwayland-dev`, `libdrm-dev`, `libgbm-dev`, `libegl-dev`, `libxcb1-dev`, `libxcb-randr0-dev`, `libxcb-shm0-dev`, `libx11-dev`, `libxtst-dev`, `libxi-dev`, `libasound2-dev`, `libv4l-dev`, `pkg-config`
- **macOS only:** Grant *Screen Recording* and *Accessibility / Input Monitoring* permissions to your terminal/binary in **System Settings -> Privacy & Security**
- **Optional:** Install `ffmpeg` in `PATH` (or pass `--ffmpeg-path`) to enable hardware-accelerated H.264 encoders (`--encoder auto` / `--encoder ffmpeg`)

### Run

Start NVRDesk with System Tray + Embedded Web UI + Dual-Stream RTSP + ONVIF Profile S:

```bash
cargo run --release -p nvrdesk-cli -- --port 8554 --http-port 8080
```

Headless / service mode (without System Tray) with webcam PiP, G.711 microphone audio, and authentication:

```bash
cargo run --release -p nvrdesk-cli -- --no-tray --webcam --audio --username admin --password secret
```

Generate or load a TOML configuration file (missing paths are initialized automatically):

```bash
cargo run --release -p nvrdesk-cli -- --print-config > nvrdesk.toml
cargo run --release -p nvrdesk-cli -- --config nvrdesk.toml
```

### Endpoints

- **Embedded Web UI:** `http://<host>:8080/`
- **Live MJPEG Preview:** `http://<host>:8080/preview/main` and `http://<host>:8080/preview/sub`
- **REST API:** `/api/v1/status`, `/api/v1/devices`, `/api/v1/config`, `/api/v1/control/pause`, `/api/v1/control/resume`
- **MainStream RTSP:** `rtsp://<host>:8554/stream1` (alias `/stream`)
- **SubStream RTSP:** `rtsp://<host>:8554/stream2`
- **Webcam Channel RTSP:** `rtsp://<host>:8554/webcam` (when `--webcam --webcam-mode channel`)
- **ONVIF Device Service:** `http://<host>:8080/onvif/device_service`
- **HTTP Snapshots:** `http://<host>:8080/snapshot/main` and `http://<host>:8080/snapshot/sub`

### Key CLI Flags

| Flag | Default | Description |
| :--- | :--- | :--- |
| `--config` | none | Load (or create) settings in a TOML config file |
| `--print-config` | `false` | Print effective TOML configuration and exit |
| `--port` | `8554` | RTSP listening port |
| `--http-port` | `8080` | HTTP port for Embedded Web UI, REST API, ONVIF SOAP & Snapshots |
| `--source` | `screen` | Capture source (`screen` or `test-pattern`) |
| `--display` | `0` | Display index when using `screen` |
| `--encoder` | `auto` | Video encoder backend (`auto`, `openh264`, or `ffmpeg`) |
| `--ffmpeg-path` / `--ffmpeg-codec` | auto | Custom FFmpeg binary path and codec (e.g. `h264_videotoolbox`, `h264_nvenc`) |
| `--fps` | `15` | MainStream target FPS |
| `--bitrate` | `2000000` | MainStream target bitrate in bps |
| `--max-width` | `1920` | MainStream max output width |
| `--keystroke-mode` | `masked-digits` | Keystroke OSD mode (`all`, `shortcuts-and-special`, `masked-digits`, `off`) |
| `--webcam` / `--webcam-mode` | `false` / `pip` | Enable webcam (`pip` overlay or `channel` at `/webcam`) |
| `--audio` / `--audio-codec` | `false` / `pcmu` | Enable microphone audio (`pcmu` or `pcma` on `trackID=1`) |
| `--record` / `--record-dir` | `false` / `recordings` | Enable local ring-buffer `.h264` recording |
| `--no-tray` / `--no-web-ui` | `false` | Disable System Tray (headless) or Embedded Web UI |
| `--no-substream` / `--no-overlays` / `--no-onvif` | `false` | Disable SubStream, input overlays, or ONVIF |
| `--username` / `--password` | none | Credentials for RTSP Digest & ONVIF WS-Security |

## License

Licensed under the [MIT License](LICENSE).
