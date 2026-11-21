# NVRDesk Public NVR & VMS Compatibility Matrix

NVRDesk implements standard **ONVIF Profile S / Profile T (`Media2` + `Events` PullPoint)**, **WS-Discovery** (`239.255.255.250:3702`), **RTSP 1.0** (TCP Interleaved & UDP Unicast), **RTSPS/HTTPS (`rustls`)**, and **SRT + MPEG-TS**.

---

## Summary Matrix

| NVR / VMS Platform | Auto-Discovery (WS-Discovery) | Video Codecs (`Main` / `Sub`) | Auth Mode | Audio (`G.711`) & Backchannel | Events (`PullPoint`) | Notes & Recommended Transport |
| :--- | :---: | :---: | :--- | :---: | :---: | :--- |
| **Hikvision NVR / iVMS-4200** | Supported | H.264 / H.265 | RTSP Digest + WS-Security | PCMU / PCMA + Backchannel | `MotionAlarm` / `PosEvent` | Use **TCP Interleaved** RTSP; keep IDR <= `2s` (`fps * idr_interval_secs`) and fixed ports (`8554`/`8080`). |
| **Dahua NVR / SmartPSS** | Supported | H.264 / H.265 | RTSP Digest + WS-Security | PCMU / PCMA + Backchannel | `MotionAlarm` / `PosEvent` | Enable `RTSP` over **TCP** on NVR channel add; use `/snapshot/main` for grid thumbnails. |
| **Uniview (UNV) / EZStation** | Supported | H.264 / H.265 | RTSP Digest + WS-Security | PCMU / Backchannel | `MotionAlarm` | Profile S (`/onvif/media_service`) and Profile T (`/onvif/media2_service`) verified. |
| **Milestone XProtect** | Supported (ONVIF Driver) | H.264 / H.265 | WS-Security PasswordDigest | PCMU + Backchannel (`a=recvonly`) | PullPoint (`Events`) | Select universal **ONVIF Conformant** driver (1 channel or Main + Sub); HTTPS/RTSPS optional. |
| **Nx Witness / Network Optix** | Supported | H.264 / H.265 | RTSP Digest + ONVIF | PCMU / Backchannel | Motion PullPoint | Auto-detects dual stream (`/stream1` & `/stream2`) via ONVIF profiles and dedicated `/webcam` if enabled. |
| **Frigate NVR (`go2rtc`)** | Direct RTSP / ONVIF | H.264 / H.265 + SRT | Optional Digest / Open LAN | PCMU (`go2rtc` transcode to Opus/AAC) | HTTP `/api/v1/events` or ONVIF | Supports direct `rtsp://<host>:8554/stream1` + `stream2` detect role (`640x360@8fps`) or SRT ingest into `go2rtc`. |
| **Shinobi CCTV** | ONVIF Scanner / RTSP | H.264 (recommended) | RTSP Digest | PCMU | Motion webhook / PullPoint | Add `rtsp://<host>:8554/stream1` with `TCP` input or pull via `/onvif/device_service`. |
| **Synology Surveillance Station** | Supported (ONVIF) | H.264 / H.265 | WS-Security + Digest | PCMU / PCMA + Backchannel | `MotionAlarm` | Select **ONVIF Generic** brand; enable fixed `onvif.device_uuid` via `nvrdesk fleet init` for persistent IDs. |

---

## Protocol Compatibility Checklist

### ONVIF Endpoints (`http://<host>:8080` or `https://<host>:8080`)
- `POST /onvif/device_service`: `GetDeviceInformation`, `GetCapabilities`, `GetServices`, `GetSystemDateAndTime`, `GetScopes`, `GetNetworkInterfaces`
- `POST /onvif/media_service` (Profile S): `GetProfiles`, `GetStreamUri`, `GetSnapshotUri`, `GetVideoEncoderConfigurations`, `GetAudioEncoderConfigurations`
- `POST /onvif/media2_service` (Profile T): `tr2:GetProfiles`, `tr2:GetStreamUri`, `tr2:GetVideoEncoderConfigurations` (advertises both H.264 & H.265)
- `POST /onvif/events_service`: `GetServiceCapabilities`, `CreatePullPointSubscription`, `PullMessages`, `Renew`, `Unsubscribe`
- `GET /snapshot/main`, `/snapshot/sub`, `/snapshot/webcam`: HTTP JPEG snapshots with privacy masks and POS OSD baked in

### RTSP & SRT URLs
- **MainStream:** `rtsp://<user>:<pass>@<host>:8554/stream1` (alias `/stream`)
- **SubStream:** `rtsp://<user>:<pass>@<host>:8554/stream2`
- **Dedicated Webcam:** `rtsp://<user>:<pass>@<host>:8554/webcam`
- **RTSPS (TLS):** `rtsps://<host>:8322/stream1`
- **SRT WAN Push (MPEG-TS over SRT):** `srt://<vms-host>:9000` (with optional AES passphrase + `streamid`, e.g. `#!::r=store-01/pos-01,m=publish`)

---

## Recommended NVR / VMS Quirks & Tuning

### 1. Hikvision / Dahua / Synology Continuous POS Recording
Static POS screens can cause some NVRs to report "No Video" if no frames/keyframes arrive. NVRDesk guarantees constant GOP cadence (`idr_interval_secs = 2` default) with SPS/PPS headers repeated on every IDR frame.

Recommended settings:
```toml
[main_stream]
codec = "h264"
fps = 15
bitrate_bps = 2000000
idr_interval_secs = 2

[sub_stream]
enabled = true
max_width = 640
fps = 8
bitrate_bps = 450000
```

### 2. Frigate NVR `config.yml` Example
```yaml
cameras:
  pos_caja_01:
    ffmpeg:
      inputs:
        - path: rtsp://192.168.10.51:8554/stream1
          roles: [record]
        - path: rtsp://192.168.10.51:8554/stream2
          roles: [detect]
    onvif:
      host: 192.168.10.51
      port: 8080
      user: admin
      password: "ChangeMe!"
```

### 3. Cloud / Remote Matrix VMS via SRT (MediaMTX / MistServer / OBS / vMix / Nimble)
When POS branches sit behind CG-NAT/LTE without inbound firewall ports, configure NVRDesk in **SRT Caller** mode:
```bash
nvrdesk fleet init cloud-srt --site sucursal-01 --terminal-id caja-02 \
  --srt-target vms.company.com:9000 --srt-passphrase "BranchSecretPass!" --out /etc/nvrdesk/nvrdesk.toml
```
NVRDesk muxes H.264/H.265 into 1316-byte aligned MPEG-TS (`PAT + PMT + PES`) chunks in pure Rust (`srt-tokio`) and pushes them to the central relay with automatic exponential backoff.
