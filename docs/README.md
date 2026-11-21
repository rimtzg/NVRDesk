# NVRDesk Documentation

**NVRDesk** is an open-source, cross-platform virtual IP camera written in Rust (MIT License) for **POS / TPV visual auditing**, desktop monitoring, and NVR/VMS streaming.

It converts desktops, screens, webcams, microphones, or external media streams (`MP4`, `MJPEG`, `RTMP`, `RTSP`) into standard **ONVIF Profile S/T**, **RTSP / RTSPS**, and **SRT** camera feeds with hardware-accelerated encoding and **Zero-Idle** CPU usage.

## Highlights

- **Dual Video Streams:** Simultaneous `MainStream` (`/stream1`, up to 1080p) and `SubStream` (`/stream2`, lightweight preview/AI detection), plus optional dedicated `/webcam` channel or PiP overlay.
- **POS Visual Auditing:** Animated mouse click ripples, borderless PCI-safe keystroke OSD (`masked_digits`, `shortcuts_and_special`, `all`, `off`), and real-time POS ticket OSD via HTTP & WebSocket.
- **Visual Privacy Masks:** Interactive live-preview mask editor in the embedded Web UI to black out PIN pads and sensitive areas before encoding or snapshots.
- **ONVIF Profile S & T:** WS-Discovery (`239.255.255.250:3702`), WS-Security (`PasswordDigest`), `Media` / `Media2` (`H.264` & `H.265`), Motion + POS `PullPoint` Events, Two-Way Audio Backchannel, and HTTP JPEG Snapshots.
- **TLS & WAN Delivery:** Built-in HTTPS + RTSPS (`rustls`) and pure-Rust **SRT** (`srt-tokio`) over MPEG-TS for branches behind NAT/LTE.
- **Enterprise Fleet Tools:** Built-in templates (`nvrdesk fleet init|validate`), `NVRDESK_*` env provisioning, Ansible, Windows silent MSI GPO script, `systemd`, and macOS `LaunchAgent`.

## Next Sections

- [POS Dynamic OSD & Privacy API](POS_OSD_API.md)
- [Fleet Deployment, SRT & Media Sources](FLEET_DEPLOYMENT.md)
- [NVR & VMS Compatibility Matrix](COMPATIBILITY_MATRIX.md)
- [Technical Roadmap](ROADMAP.md)
