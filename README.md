# NVRDesk

**NVRDesk** is a lightweight, cross-platform, open-source virtual IP camera written in Rust (MIT License). It turns desktops, screens, POS terminals, webcams, and microphones into standard **ONVIF / RTSP** IP cameras discoverable and recordable by any NVR or VMS (Hikvision, Dahua, Milestone, Frigate, Blue Iris, ONVIF Device Manager, VLC, etc.).

Designed primarily for **Point-of-Sale (POS) visual auditing** and unattended 24/7 operation, NVRDesk focuses on minimal CPU/RAM usage, zero-idle encoding, and clear operator activity overlays.

## Current Status: All Phases Complete (`v0.4.0` — Enterprise POS Fleet, SRT & Compatibility Matrix)

- [x] **Phase 0:** Cargo workspace, cross-platform CI (Windows/Linux/macOS), screen+cursor capture (`xcap`), SIMD I420 (`yuvutils-rs`), `openh264`, RTSP 1.0 + RFC 6184 RTP
- [x] **Phase 1:**
  - Dual-stream (`MainStream` on `/stream1` + `SubStream` on `/stream2`, independent demand-driven encoding)
  - Global mouse click ripples (visible only on click) and borderless keystroke OSD (`nvrdesk-input`) rendered crisply in **both** Main and Sub streams
  - POS privacy modes (`masked_digits`, `shortcuts_and_special`, `all`, `off`) and sensitive-window auto-pause (`excluded_windows`)
  - RTSP HTTP Digest authentication & idle grace period
  - **ONVIF Profile S** server (`nvrdesk-onvif`): WS-Discovery (`239.255.255.250:3702`), WS-Security (`PasswordDigest`), Device & Media SOAP services, and HTTP JPEG snapshots (`/snapshot/main`, `/snapshot/sub`)
  - TOML configuration (`--config nvrdesk.toml` / `--print-config`)
- [x] **Phase 2:**
  - **Daemon & Embedded Web UI** (`nvrdesk-daemon` + `web-ui/` via `rust-embed` sharing HTTP port `8080`): live MJPEG preview (`/preview/main`, `/preview/sub`, `/preview/webcam`), telemetry (`/api/v1/status`), hardware device inventory (`/api/v1/devices`), live hot-reload (`/api/v1/config`), and pause/resume controls
  - **System Tray** (`nvrdesk-tray` via `tray-icon` + Linux `ksni`): Open Web UI, Pause/Resume streaming, Quit, and live show/hide toggle from Web UI
  - **Encoder Backends** (`nvrdesk-codec`): `OpenH264` software fallback + optional external **FFmpeg CLI/pipe backend** (`--encoder auto|openh264|ffmpeg`) with OS hardware acceleration
  - **Webcam & Microphone**: Optional `nokhwa` webcam capture (Picture-in-Picture or hot-registered `/webcam` RTSP channel) + `cpal` microphone G.711 `PCMU`/`PCMA` (`trackID=1`)
  - **Local Ring-Buffer Recorder**: Segmented Annex-B `.h264` recording (`--record`) with automatic disk quota eviction
- [x] **Phase 3:**
  - **Dynamic POS OSD API & Privacy Masks**: `POST /api/v1/osd/text`, `GET/DELETE /api/v1/osd/text`, WebSocket `/api/v1/ws/osd`, and normalized screen privacy masks (`pos.privacy_masks`) rendered before encoding & snapshots
  - **ONVIF Profile T (`Media2`) & H.265/HEVC**: `/onvif/media2_service` (`tr2:*`), RFC 7798 H.265 RTP packetizer + SDP (`sprop-vps/sps/pps`), and FFmpeg HEVC hardware/software encoding (`--codec h265`)
  - **Pure-Rust TLS Encryption (`rustls` + `rcgen`)**: optional **HTTPS** (`--https`) for Web UI / WebSocket / ONVIF SOAP and **RTSPS** (`--rtsps` on port `8322`) with self-signed SAN cert or custom PEM cert/key
  - **Audio Backchannel (VMS -> POS)**: ONVIF `a=recvonly` `trackID=2` over TCP interleaved / UDP decoded from G.711 PCMU/PCMA to `cpal` speaker output (`--backchannel`)
  - **Motion Detection & ONVIF Events (`PullPointSubscription`)**: frame-delta activity detector + `/onvif/events_service` (`CreatePullPointSubscription`, `PullMessages`, `Renew`, `Unsubscribe`) for `MotionAlarm` and `PosEvent`
  - **Release Packaging Workflow**: [release.yml](.github/workflows/release.yml) for `.deb`/`.tar.gz` (Linux), `.dmg` (macOS), `.msi`/`.zip` (Windows), and automated GitHub Releases
- [x] **Phase 4:**
  - **POS Fleet Management**: Built-in provisioning templates (`pos-cashier`, `pos-self-checkout`, `kiosk-audit`, `pci-strict`, `cloud-srt`), `nvrdesk fleet init|validate|templates`, `NVRDESK_*` env overrides, `/api/v1/fleet/*` endpoints, and Ansible / GPO PowerShell / `systemd` / `LaunchAgent` installers in `packaging/fleet/`
  - **Pure-Rust SRT Transport (`srt-tokio` + MPEG-TS Muxer)**: Secure Reliable Transport (`caller`/`listener`, AES passphrase, `streamid`, 1316-byte `7*188` MPEG-TS PAT/PMT/PES) for cloud/WAN VMS relays
  - **Additional FFmpeg Pull Sources**: `--source mp4|mjpeg|rtmp|rtsp-relay|ffmpeg-uri` (`--source-uri`) enabling MP4 test loops and legacy camera restreaming through ONVIF Profile S/T & RTSP
  - **Public Compatibility Matrix**: Validated guide for Hikvision, Dahua, Uniview, Milestone XProtect, Nx Witness, Frigate, Shinobi, and Synology in [docs/COMPATIBILITY_MATRIX.md](docs/COMPATIBILITY_MATRIX.md)

See [docs/ROADMAP.md](docs/ROADMAP.md), [docs/POS_OSD_API.md](docs/POS_OSD_API.md), [docs/FLEET_DEPLOYMENT.md](docs/FLEET_DEPLOYMENT.md), and [docs/COMPATIBILITY_MATRIX.md](docs/COMPATIBILITY_MATRIX.md).

## Workspace Architecture

```text
crates/
├── nvrdesk-core       # Domain types, TOML config schema, TLS cert builder, overlay snapshots, errors
├── nvrdesk-capture    # Cross-platform screen+cursor (xcap), webcam (nokhwa), audio in/out (cpal)
├── nvrdesk-input      # Global keyboard/mouse hook (rdevin), coordinate mapping & POS privacy rules
├── nvrdesk-pipeline   # Scaling, privacy masks, webcam PiP, click/POS OSD, motion detector, JPEG & I420
├── nvrdesk-codec      # VideoEncoder (OpenH264 + optional FFmpeg H.264/H.265) & G.711 encode/decode
├── nvrdesk-rtsp       # Multi-stream RTSP/RTSPS server, RFC 6184/7798 RTP & G.711 backchannel
├── nvrdesk-onvif      # WS-Discovery, WS-Security, Device/Media/Media2/Events PullPoint & snapshots
├── nvrdesk-daemon     # Daemon runtime, POS OSD REST/WS API, RingRecorder & HTTPS/HTTP Web UI
├── nvrdesk-tray       # Cross-platform System Tray menu with live show/hide toggle
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

- **Embedded Web UI:** `http(s)://<host>:8080/`
- **Live MJPEG Preview:** `http(s)://<host>:8080/preview/main`, `/preview/sub`, `/preview/webcam`
- **REST & WebSocket API:** `/api/v1/status`, `/api/v1/devices`, `/api/v1/config`, `/api/v1/osd/text`, `ws://<host>:8080/api/v1/ws/osd`, `/api/v1/events`, `/api/v1/control/pause`, `/api/v1/control/resume`
- **MainStream RTSP / RTSPS:** `rtsp://<host>:8554/stream1` (alias `/stream`), `rtsps://<host>:8322/stream1` (with `--rtsps`)
- **SubStream RTSP:** `rtsp://<host>:8554/stream2`
- **Webcam Channel RTSP:** `rtsp://<host>:8554/webcam`
- **ONVIF Services:** `/onvif/device_service`, `/onvif/media_service`, `/onvif/media2_service` (Profile T), `/onvif/events_service` (PullPoint)
- **HTTP Snapshots:** `http(s)://<host>:8080/snapshot/main`, `/snapshot/sub`, `/snapshot/webcam`

### Key CLI Flags

| Flag | Default | Description |
| :--- | :--- | :--- |
| `--config` | none | Load (or create) settings in a TOML config file |
| `--print-config` | `false` | Print effective TOML configuration and exit |
| `--port` | `8554` | RTSP listening port |
| `--http-port` | `8080` | HTTP/HTTPS port for Web UI, REST/WS API, ONVIF SOAP & Snapshots |
| `--https` / `--rtsps` | `false` | Enable TLS encryption on HTTP (HTTPS) and RTSP (`--rtsps-port 8322`) |
| `--source` | `screen` | Capture source (`screen` or `test-pattern`) |
| `--display` | `0` | Display index when using `screen` |
| `--codec` | `h264` | MainStream video codec (`h264` or `h265` via FFmpeg HEVC) |
| `--encoder` | `auto` | Video encoder backend (`auto`, `openh264`, or `ffmpeg`) |
| `--ffmpeg-path` / `--ffmpeg-codec` | auto | Custom FFmpeg binary path and codec (e.g. `h264_videotoolbox`, `hevc_nvenc`) |
| `--fps` | `15` | MainStream target FPS |
| `--bitrate` | `2000000` | MainStream target bitrate in bps |
| `--max-width` | `1920` | MainStream max output width |
| `--keystroke-mode` | `masked-digits` | Keystroke OSD mode (`all`, `shortcuts-and-special`, `masked-digits`, `off`) |
| `--webcam` / `--webcam-mode` | `false` / `pip` | Enable webcam (`pip` overlay or `channel` at `/webcam`) |
| `--audio` / `--audio-codec` | `false` / `pcmu` | Enable microphone audio (`pcmu` or `pcma` on `trackID=1`) |
| `--backchannel` | `false` | Enable ONVIF audio backchannel (`trackID=2` VMS -> POS speakers) |
| `--no-motion` | `false` | Disable motion detection ONVIF events |
| `--record` / `--record-dir` | `false` / `recordings` | Enable local ring-buffer `.h264` recording |
| `--no-tray` / `--no-web-ui` | `false` | Disable System Tray (headless) or Embedded Web UI |
| `--no-substream` / `--no-overlays` / `--no-onvif` | `false` | Disable SubStream, input overlays, or ONVIF |
| `--username` / `--password` | none | Credentials for RTSP Digest & ONVIF WS-Security |

## License

Licensed under the [MIT License](LICENSE).
