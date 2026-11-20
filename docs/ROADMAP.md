# NVRDesk Technical Roadmap

NVRDesk is a cross-platform, MIT-licensed virtual IP camera in Rust focused on POS terminal monitoring and general desktop-to-NVR streaming.

## Design Principles

1. **MIT-Clean Multimedia Core (Route 1 + Optional FFmpeg):**
   - Default pipeline uses native OS hardware encoders (Windows Media Foundation, macOS VideoToolbox, Linux VA-API) and `openh264` (BSD-2-Clause) as software fallback.
   - Optional FFmpeg backend via external CLI/pipe adapter (or opt-in feature flag) so official releases remain strictly MIT.
2. **Zero-Idle Encoding:**
   - Pause video encoding when no RTSP client or local recorder is active to keep background CPU near 0%.
3. **NVR Compatibility First:**
   - Support RTP over TCP Interleaved and UDP Unicast.
   - Maintain constant frame cadence and periodic IDR + SPS/PPS headers even on static POS screens.
   - Provide MainStream + SubStream profiles for NVR live-grid and continuous recording.
4. **POS Security & Auditability:**
   - Visible mouse cursor, animated mouse click indicators, and keystroke OSD rendered into both MainStream and SubStream.
   - Configurable privacy masks, digit masking, and sensitive-window exclusions.

## Milestones

- **Phase 0 — Foundations & PoC:** Cargo workspace, cross-platform CI (Windows/Linux/macOS), screen+cursor -> I420 -> OpenH264 -> RTSP -> VLC/ffplay.
- **Phase 1 — ONVIF Profile S MVP & POS Input Overlay:** Dual stream (Main/Sub), global mouse-click + keystroke overlays (`nvrdesk-input`), RTSP Digest auth, WS-Discovery (`239.255.255.250:3702`), SOAP `Device`/`Media`, HTTP snapshots, validation against ONVIF Device Manager & physical NVRs.
- **Phase 2 — Daemon, Embedded Web UI, Tray & Webcam/Mic:** `axum` + `rust-embed` Web UI, `tray-icon`, Windows Session 0 daemon/user-agent split, optional webcam (`nokhwa`, channel or PiP), optional microphone (`cpal` G.711/AAC), hardware encoders + optional FFmpeg backend, optional local ring-buffer recording.
- **Phase 3 — POS Integration & Profile T:** HTTP/WS dynamic POS ticket OSD API, static privacy masks, H.265/Media2, HTTPS & RTSPS (`rustls`), audio backchannel, motion/event PullPoint, native installers (`.msi`, `.dmg`, `.deb`).
- **Phase 4 — Enterprise Scale:** Fleet configuration provisioning, SRT transport, external RTMP/RTSP restreaming, compatibility matrix.
