# NVRDesk Fleet Deployment, SRT Relay & Media Sources Guide

Phase 4 introduces enterprise POS fleet provisioning tools, pure-Rust SRT cloud streaming, and FFmpeg-backed additional media sources.

---

## 1. Fleet Templates & CLI Provisioning

List built-in POS fleet templates:

```bash
nvrdesk fleet templates
```

Available profiles:
- `pos-cashier`: 1080p@15fps H.264, `masked_digits` PCI keystroke OSD, click ripples, motion events, Zero-Idle.
- `pos-self-checkout`: Dual stream + customer PiP webcam + G.711 microphone audio + local ring backup.
- `kiosk-audit`: Lightweight 720p@10fps (`900 kbps`) with `shortcuts_and_special` keystroke OSD.
- `pci-strict`: Input overlays off, bottom-right `PIN-PAD` privacy mask, HTTPS + RTSPS TLS enabled.
- `cloud-srt`: Always-on 1080p@15fps H.264 configured for SRT Caller push + ring backup.

Generate & validate a terminal configuration with deterministic ONVIF UUIDv5 identity:

```bash
nvrdesk fleet init pos-cashier \
  --site store-01 \
  --terminal-id caja-04 \
  --camera-name "POS Caja 04" \
  --username admin \
  --password "StrongPassword!" \
  --out /etc/nvrdesk/nvrdesk.toml

nvrdesk fleet validate --config /etc/nvrdesk/nvrdesk.toml
```

### Environment Variables (`NVRDESK_*`)
Any setting below overrides loaded TOML values automatically (highest precedence stays with explicit CLI flags):

`NVRDESK_SITE`, `NVRDESK_TERMINAL_ID`, `NVRDESK_CAMERA_NAME`, `NVRDESK_SERIAL_NUMBER`, `NVRDESK_BIND`, `NVRDESK_RTSP_PORT`, `NVRDESK_HTTP_PORT`, `NVRDESK_USERNAME`, `NVRDESK_PASSWORD`, `NVRDESK_SRT_TARGET`, `NVRDESK_SRT_PASSPHRASE`, `NVRDESK_SRT_STREAM_ID`, `NVRDESK_SOURCE_URI`, `NVRDESK_CONTROLLER_URL`.

### Provisioning REST API
- `GET /api/v1/fleet/templates`: returns built-in template catalog.
- `POST /api/v1/fleet/apply-template`: applies `{ "template": "pos-cashier", "overrides": { "site": "...", "terminal_id": "..." } }` live with disk persistence and hot-reload.
- `GET /api/v1/fleet/manifest`: exports inventory JSON (`site`, `terminal_id`, `device_uuid`, `endpoints`, `validation`).
- `POST /api/v1/config/validate`: validates a candidate `NvrDeskConfig` JSON payload.

---

## 2. Deployment Templates (`packaging/fleet/`)

- **Linux Ansible:** [`packaging/fleet/ansible/nvrdesk-pos.yml`](../packaging/fleet/ansible/nvrdesk-pos.yml)
- **Linux `systemd` unit:** [`packaging/fleet/linux/nvrdesk.service`](../packaging/fleet/linux/nvrdesk.service)
- **Windows GPO / Silent MSI PowerShell:** [`packaging/fleet/windows/Install-NvrDeskPos.ps1`](../packaging/fleet/windows/Install-NvrDeskPos.ps1) (installs `.msi`, provisions config, opens firewall ports 8554/8080/3702, schedules interactive user-session task to bypass Session 0 capture isolation)
- **macOS `LaunchAgent`:** [`packaging/fleet/macos/com.nvrdesk.agent.plist`](../packaging/fleet/macos/com.nvrdesk.agent.plist)

---

## 3. SRT Cloud / WAN Push (Pure-Rust MPEG-TS over SRT)

Run as SRT Caller pushing encrypted MPEG-TS (`7 * 188 = 1316` byte chunks) to a cloud VMS/MediaMTX server:

```bash
nvrdesk --no-tray \
  --srt-mode caller \
  --srt-target vms.example.com:9000 \
  --srt-stream-id "#!::r=store-01/pos-01,m=publish" \
  --srt-passphrase "SecretSrtKey123" \
  --srt-latency-ms 120
```

---

## 4. Additional Media Sources (MP4, MJPEG, RTMP, RTSP Relay)

Requires `ffmpeg` installed on `PATH` (or `--ffmpeg-path`). All NVRDesk overlays, privacy masks, POS OSD, snapshots, motion events, dual streams, and SRT push work seamlessly on external sources:

```bash
# Loop an MP4 file for NVR/POS testing
nvrdesk --no-tray --source mp4 --source-uri ./samples/pos_demo.mp4

# Convert a legacy HTTP MJPEG camera into ONVIF Profile S/T + RTSP + H.264/H.265
nvrdesk --no-tray --source mjpeg --source-uri http://192.168.1.90/mjpg/video.mjpg

# Restream an RTMP or upstream RTSP feed
nvrdesk --no-tray --source rtmp --source-uri rtmp://127.0.0.1/live/cam1
nvrdesk --no-tray --source rtsp-relay --source-uri rtsp://user:pass@192.168.1.91:554/h264
```
