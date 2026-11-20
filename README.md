# NVRDesk

**NVRDesk** is a lightweight, cross-platform, open-source virtual IP camera written in Rust (MIT License). It turns desktops, screens, POS terminals, webcams, and microphones into standard **ONVIF / RTSP** IP cameras discoverable and recordable by any NVR or VMS (Hikvision, Dahua, Milestone, Frigate, Blue Iris, VLC, etc.).

Designed primarily for **Point-of-Sale (POS) visual auditing** and unattended 24/7 operation, NVRDesk focuses on minimal CPU/RAM usage, zero-idle encoding, and clear operator activity overlays.

## Current Status: Phase 0 (PoC)

- [x] Cargo workspace architecture (`core`, `capture`, `pipeline`, `codec`, `rtsp`, `cli`)
- [x] Cross-platform screen + cursor capture (`xcap`) & synthetic test pattern source
- [x] SIMD BGRA/RGBA -> I420 conversion & resizing (`yuvutils-rs`, `fast_image_resize`)
- [x] H.264 encoding (`openh264`, MIT-compatible BSD-2-Clause)
- [x] RTSP 1.0 server with RTP/H.264 (RFC 6184) over TCP Interleaved & UDP Unicast
- [ ] Phase 1: Dual-stream (Main/Sub), keystroke & mouse-click visual overlays, ONVIF Profile S (WS-Discovery + SOAP)
- [ ] Phase 2: Embedded Web UI, System Tray, Hardware encoders (WMF/VideoToolbox/VA-API), optional FFmpeg backend, Webcam & Microphone
- [ ] Phase 3: POS dynamic text OSD API, ONVIF Profile T (H.265/HTTPS/RTSPS), Motion/Events

See [docs/ROADMAP.md](docs/ROADMAP.md) for the full technical plan.

## Workspace Architecture

```text
crates/
├── nvrdesk-core       # Domain types (RawVideoFrame, EncodedFrame, StreamConfig, errors)
├── nvrdesk-capture    # CaptureSource trait, cross-platform screen+cursor (scap) & test pattern
├── nvrdesk-pipeline   # Frame pacer/repeater, scaling & SIMD YUV420p conversion
├── nvrdesk-codec      # VideoEncoder trait & OpenH264 encoder (NAL/SPS/PPS extraction)
├── nvrdesk-rtsp       # Async RTSP 1.0 server + RFC 6184 H.264 RTP packetizer (TCP & UDP)
└── nvrdesk-cli        # Phase 0 CLI binary (`nvrdesk`)
```

## Quick Start (Phase 0 PoC)

### Prerequisites

- **Rust** 1.88+ (`rustup` stable)
- **Linux only:** `libpipewire-0.3-dev`, `libclang-dev`, `libdbus-1-dev`, `pkg-config`
- **macOS only:** Grant *Screen Recording* permission to your terminal/binary in **System Settings -> Privacy & Security -> Screen Recording**

### Run

Stream your primary display (with cursor) over RTSP:

```bash
cargo run --release -p nvrdesk-cli -- --port 8554 --fps 15 --bitrate 2000000
```

Or test without screen permissions using the built-in animated test pattern:

```bash
cargo run --release -p nvrdesk-cli -- --source test-pattern --port 8554 --fps 15
```

Then open the stream in **VLC** or **ffplay**:

```bash
ffplay -rtsp_transport tcp rtsp://127.0.0.1:8554/stream
```

### CLI Options

| Flag | Default | Description |
| :--- | :--- | :--- |
| `--port` | `8554` | RTSP listening port |
| `--bind` | `0.0.0.0` | Bind IP address |
| `--path` | `/stream` | RTSP stream path |
| `--source` | `screen` | Capture source (`screen` or `test-pattern`) |
| `--display` | `0` | Display index when using `screen` |
| `--fps` | `15` | Target frames per second |
| `--bitrate` | `2000000` | Target H.264 bitrate in bps |
| `--max-width` | `1920` | Maximum output width (scales preserving aspect ratio) |
| `--no-cursor` | `false` | Hide OS mouse cursor in screen capture |
| `--always-on` | `false` | Keep capturing/encoding even with zero RTSP clients |

## License

Licensed under the [MIT License](LICENSE).
