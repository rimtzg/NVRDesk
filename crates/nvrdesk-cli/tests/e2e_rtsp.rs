//! End-to-end integration tests for Phase 1: Dual-Stream RTSP + Overlays + ONVIF Profile S.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use nvrdesk_capture::{CaptureSource, TestPatternSource};
use nvrdesk_codec::{OpenH264Encoder, VideoEncoder};
use nvrdesk_core::{InputOverlayConfig, KeystrokeMode, MouseButton, NvrDeskError, StreamConfig};
use nvrdesk_input::{InputTracker, TrackedEvent};
use nvrdesk_onvif::{
    OnvifAuthConfig, OnvifDeviceMeta, OnvifMediaProfile, OnvifServer, SnapshotProvider,
};
use nvrdesk_pipeline::FrameProcessor;
use nvrdesk_rtsp::{RtspServer, RtspStreamSpec};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test]
async fn streams_dual_h264_with_overlays_and_passes_ffprobe() {
    let main_cfg = StreamConfig {
        fps: 15,
        bitrate_bps: 1_000_000,
        max_width: 1280,
        idr_interval_secs: 1,
        show_cursor: true,
    };
    let sub_cfg = StreamConfig {
        fps: 10,
        bitrate_bps: 400_000,
        max_width: 640,
        idr_interval_secs: 1,
        show_cursor: true,
    };

    let specs = vec![
        RtspStreamSpec {
            path: "/stream1".to_string(),
            aliases: vec!["/stream".to_string()],
            name: "NVRDesk Main E2E".to_string(),
            audio_codec: None,
        },
        RtspStreamSpec {
            path: "/stream2".to_string(),
            aliases: Vec::new(),
            name: "NVRDesk Sub E2E".to_string(),
            audio_codec: None,
        },
    ];

    let (server, addr, mut handles) = RtspServer::bind_streams(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        None,
        specs,
    )
    .await
    .expect("bind rtsp");
    let main_handle = handles.remove("/stream1").expect("main");
    let sub_handle = handles.remove("/stream2").expect("sub");

    let server_task = tokio::spawn(async move {
        let _ = server.serve().await;
    });

    let tracker = InputTracker::new(
        InputOverlayConfig {
            keystroke_mode: KeystrokeMode::All,
            ..Default::default()
        },
        None,
    );
    let t0 = Instant::now();
    tracker.push_event(TrackedEvent::MouseMove { x: 400.0, y: 240.0 }, t0);
    tracker.push_event(
        TrackedEvent::MouseDown {
            button: MouseButton::Left,
        },
        t0,
    );
    for ch in "POS123".chars() {
        tracker.push_event(
            TrackedEvent::KeyPress {
                key_name: ch.to_string(),
                printable: Some(ch),
                is_special: false,
                modifier: None,
            },
            t0,
        );
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_worker = Arc::clone(&shutdown);
    let main_demand = main_handle.subscribe_demand();
    let sub_demand = sub_handle.subscribe_demand();

    let worker = std::thread::spawn(move || {
        let mut source = TestPatternSource::new(1280, 720, main_cfg.fps);
        let bounds = source.bounds();
        let mut main_proc = FrameProcessor::new();
        let mut sub_proc = FrameProcessor::new();
        let mut main_enc = OpenH264Encoder::new(&main_cfg).expect("main enc");
        let mut sub_enc = OpenH264Encoder::new(&sub_cfg).expect("sub enc");
        let mut started = false;
        let mut frame_num = 0u64;

        while !shutdown_worker.load(Ordering::Acquire) {
            let need_main = *main_demand.borrow() > 0;
            let need_sub = *sub_demand.borrow() > 0;
            if !need_main && !need_sub {
                if started {
                    source.stop();
                    started = false;
                }
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            if !started {
                source.start().expect("start source");
                started = true;
            }
            if let Ok(Some(raw)) = source.try_next_frame(Duration::from_millis(50)) {
                let snap = tracker.snapshot(bounds, Instant::now());
                let periodic_idr = frame_num.is_multiple_of(10);
                if need_main {
                    let force_idr = periodic_idr || main_handle.take_keyframe_request();
                    if let Ok(i420) =
                        main_proc.process_with_overlay(&raw, main_cfg.max_width, Some(&snap))
                        && let Ok(enc) = main_enc.encode(&i420, force_idr)
                    {
                        main_handle.publish_frame(enc);
                    }
                }
                if need_sub {
                    let force_idr = periodic_idr || sub_handle.take_keyframe_request();
                    if let Ok(i420) =
                        sub_proc.process_with_overlay(&raw, sub_cfg.max_width, Some(&snap))
                        && let Ok(enc) = sub_enc.encode(&i420, force_idr)
                    {
                        sub_handle.publish_frame(enc);
                    }
                }
                frame_num += 1;
            }
        }
        if started {
            source.stop();
        }
    });

    // Verify MainStream (/stream1) and SubStream (/stream2) via raw RTSP + ffprobe.
    for (path, expected_w, expected_h) in [("/stream1", "1280", "720"), ("/stream2", "640", "360")]
    {
        let url = format!("rtsp://127.0.0.1:{}{path}", addr.port());
        verify_rtsp_tcp_interleaved(addr, &url).await;

        if let Ok(output) = tokio::task::spawn_blocking({
            let url = url.clone();
            move || {
                Command::new("ffprobe")
                    .args([
                        "-v",
                        "error",
                        "-rtsp_transport",
                        "tcp",
                        "-analyzeduration",
                        "2000000",
                        "-probesize",
                        "1000000",
                        "-show_entries",
                        "stream=codec_name,width,height",
                        "-of",
                        "default=nw=1",
                        &url,
                    ])
                    .output()
            }
        })
        .await
        .expect("spawn_blocking")
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "ffprobe failed for {path}: stdout={stdout} stderr={stderr}"
            );
            assert!(
                stdout.contains("codec_name=h264"),
                "unexpected codec: {stdout}"
            );
            assert!(
                stdout.contains(&format!("width={expected_w}")),
                "unexpected width: {stdout}"
            );
            assert!(
                stdout.contains(&format!("height={expected_h}")),
                "unexpected height: {stdout}"
            );
        }
    }

    shutdown.store(true, Ordering::Release);
    server_task.abort();
    let _ = worker.join();
}

struct TestSnapshot;

impl SnapshotProvider for TestSnapshot {
    fn capture_jpeg(&self, _profile_slug: &str) -> Result<Vec<u8>, NvrDeskError> {
        let mut src = TestPatternSource::new(640, 360, 15);
        src.start()?;
        let frame = src
            .try_next_frame(Duration::from_millis(200))?
            .expect("test frame");
        src.stop();
        let mut proc = FrameProcessor::new();
        proc.encode_jpeg_snapshot(&frame, 640, 80, None)
    }
}

#[tokio::test]
async fn onvif_soap_ws_security_and_snapshot_work_end_to_end() {
    let auth = OnvifAuthConfig {
        username: "pos_admin".into(),
        password: "secret_password".into(),
    };
    let meta = OnvifDeviceMeta {
        device_name: "Caja 1".into(),
        manufacturer: "NVRDesk".into(),
        model: "Virtual IP Camera".into(),
        firmware_version: "0.1.0".into(),
        serial_number: "TEST-01".into(),
        hardware_id: "HW-01".into(),
        rtsp_port: 8554,
        http_port: 0,
    };
    let profiles = vec![
        OnvifMediaProfile {
            token: "MainProfile".into(),
            name: "MainStream".into(),
            encoder_token: "MainEnc".into(),
            width: 1920,
            height: 1080,
            fps: 15,
            bitrate_kbps: 2000,
            gov_length: 30,
            rtsp_path: "/stream1".into(),
            snapshot_slug: "main".into(),
        },
        OnvifMediaProfile {
            token: "SubProfile".into(),
            name: "SubStream".into(),
            encoder_token: "SubEnc".into(),
            width: 640,
            height: 360,
            fps: 8,
            bitrate_kbps: 450,
            gov_length: 16,
            rtsp_path: "/stream2".into(),
            snapshot_slug: "sub".into(),
        },
    ];

    let server = OnvifServer::bind(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        meta,
        profiles,
        Some(auth),
        Arc::new(TestSnapshot),
    )
    .await
    .expect("bind onvif");
    let addr = server.local_addr();
    let task = tokio::spawn(async move {
        let _ = server.serve().await;
    });

    // 1. GetSystemDateAndTime works without auth.
    let dt_resp = send_http(
        addr,
        "POST",
        "/onvif/device_service",
        &[],
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:GetSystemDateAndTime/></s:Body></s:Envelope>"#,
    )
    .await;
    assert!(
        dt_resp.starts_with("HTTP/1.1 200 OK"),
        "unexpected response: {dt_resp}"
    );
    assert!(dt_resp.contains("GetSystemDateAndTimeResponse"));

    // 2. GetProfiles without auth returns 401.
    let unauth_resp = send_http(
        addr,
        "POST",
        "/onvif/media_service",
        &[],
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><trt:GetProfiles/></s:Body></s:Envelope>"#,
    )
    .await;
    assert!(
        unauth_resp.starts_with("HTTP/1.1 401"),
        "expected 401: {unauth_resp}"
    );

    // 3. WS-Security GetStreamUri for SubProfile returns rtsp://.../stream2.
    let nonce = b"nonce-12345";
    let created = "2026-11-20T23:55:00Z";
    let mut hasher = Sha1::new();
    hasher.update(nonce);
    hasher.update(created.as_bytes());
    hasher.update(b"secret_password");
    let digest = BASE64.encode(hasher.finalize());
    let nonce_b64 = BASE64.encode(nonce);

    let stream_uri_soap = format!(
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
                      xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"
                      xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">
            <s:Header>
                <wsse:Security>
                    <wsse:UsernameToken>
                        <wsse:Username>pos_admin</wsse:Username>
                        <wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password>
                        <wsse:Nonce>{nonce_b64}</wsse:Nonce>
                        <wsu:Created>{created}</wsu:Created>
                    </wsse:UsernameToken>
                </wsse:Security>
            </s:Header>
            <s:Body>
                <trt:GetStreamUri>
                    <trt:ProfileToken>SubProfile</trt:ProfileToken>
                </trt:GetStreamUri>
            </s:Body>
        </s:Envelope>"#
    );
    let uri_resp = send_http(
        addr,
        "POST",
        "/onvif/media_service",
        &[("Host", &format!("127.0.0.1:{}", addr.port()))],
        &stream_uri_soap,
    )
    .await;
    assert!(
        uri_resp.starts_with("HTTP/1.1 200 OK"),
        "GetStreamUri failed: {uri_resp}"
    );
    assert!(uri_resp.contains("rtsp://127.0.0.1:8554/stream2"));

    // 4. Snapshot endpoint returns valid JPEG with Basic Auth.
    let basic_cred = BASE64.encode("pos_admin:secret_password");
    let basic_hdr = format!("Basic {basic_cred}");
    let snap_bytes = send_http_raw(
        addr,
        "GET",
        "/snapshot/main",
        &[("Authorization", &basic_hdr)],
        "",
    )
    .await;
    let body_start = snap_bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http headers")
        + 4;
    let head = String::from_utf8_lossy(&snap_bytes[..body_start]);
    assert!(
        head.starts_with("HTTP/1.1 200 OK"),
        "snapshot failed: {head}"
    );
    assert!(
        snap_bytes[body_start..].starts_with(&[0xFF, 0xD8]),
        "expected JPEG SOI"
    );

    task.abort();
}

async fn verify_rtsp_tcp_interleaved(addr: SocketAddr, url: &str) {
    let mut sock = TcpStream::connect(addr).await.expect("connect rtsp");

    let opts_resp = send_rtsp(
        &mut sock,
        &format!("OPTIONS {url} RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
    )
    .await;
    assert!(
        opts_resp.starts_with("RTSP/1.0 200 OK"),
        "OPTIONS failed: {opts_resp}"
    );

    let desc_resp = send_rtsp(
        &mut sock,
        &format!("DESCRIBE {url} RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\n\r\n"),
    )
    .await;
    assert!(
        desc_resp.starts_with("RTSP/1.0 200 OK"),
        "DESCRIBE failed: {desc_resp}"
    );
    assert!(desc_resp.contains("a=rtpmap:96 H264/90000"));
    assert!(desc_resp.contains("sprop-parameter-sets="));

    let setup_resp = send_rtsp(
        &mut sock,
        &format!(
            "SETUP {url}/trackID=0 RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    )
    .await;
    assert!(
        setup_resp.starts_with("RTSP/1.0 200 OK"),
        "SETUP failed: {setup_resp}"
    );
    let session_id = setup_resp
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("session:"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.split(';').next().unwrap_or("").trim().to_string())
        .expect("Session header");

    let play_resp = send_rtsp(
        &mut sock,
        &format!("PLAY {url} RTSP/1.0\r\nCSeq: 4\r\nSession: {session_id}\r\n\r\n"),
    )
    .await;
    assert!(
        play_resp.starts_with("RTSP/1.0 200 OK"),
        "PLAY failed: {play_resp}"
    );

    let mut rtp_packets_seen = 0usize;
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut buf = Vec::new();
    while Instant::now() < deadline && rtp_packets_seen < 3 {
        let mut tmp = [0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut tmp))
            .await
            .expect("rtp read timeout")
            .expect("rtp read");
        assert!(n > 0, "unexpected EOF waiting for RTP");
        buf.extend_from_slice(&tmp[..n]);

        while buf.len() >= 4 {
            if buf[0] != b'$' {
                buf.remove(0);
                continue;
            }
            let pkt_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            if buf.len() < 4 + pkt_len {
                break;
            }
            let rtp = &buf[4..4 + pkt_len];
            assert!(rtp.len() >= 12);
            assert_eq!(rtp[0] >> 6, 2);
            assert_eq!(rtp[1] & 0x7F, 96);
            rtp_packets_seen += 1;
            buf.drain(..4 + pkt_len);
        }
    }
    assert!(rtp_packets_seen >= 3);
}

async fn send_rtsp(sock: &mut TcpStream, req: &str) -> String {
    sock.write_all(req.as_bytes()).await.expect("write rtsp");
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(3), sock.read(&mut buf))
        .await
        .expect("read rtsp timeout")
        .expect("read rtsp");
    String::from_utf8_lossy(&buf[..n]).to_string()
}

async fn send_http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> String {
    let raw = send_http_raw(addr, method, path, headers, body).await;
    String::from_utf8_lossy(&raw).to_string()
}

async fn send_http_raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Vec<u8> {
    let mut sock = TcpStream::connect(addr).await.expect("connect http");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Type: application/soap+xml; charset=utf-8\r\nContent-Length: {}\r\n",
        addr.port(),
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);

    sock.write_all(req.as_bytes()).await.expect("write http");
    let mut out = Vec::new();
    sock.read_to_end(&mut out).await.expect("read http");
    out
}

#[tokio::test]
async fn daemon_web_ui_rest_api_and_audio_rtsp_track_work_end_to_end() {
    use nvrdesk_codec::audio::encode_g711;
    use nvrdesk_core::{AudioCodecKind, CaptureSourceKind, EncoderBackendKind, NvrDeskConfig};
    use nvrdesk_daemon::start_daemon;

    let mut cfg = NvrDeskConfig::default();
    cfg.source.kind = CaptureSourceKind::TestPattern;
    cfg.encoder.backend = EncoderBackendKind::Openh264;
    cfg.rtsp.bind = "127.0.0.1".to_string();
    cfg.rtsp.port = 0;
    cfg.onvif.http_port = 0;
    cfg.onvif.discovery_enabled = false;
    cfg.daemon.tray_enabled = false;
    cfg.input_overlay.enabled = false;

    let shutdown = Arc::new(AtomicBool::new(false));
    let (handle, worker) = start_daemon(cfg, None, Arc::clone(&shutdown))
        .await
        .expect("start_daemon");

    let web_url = handle.web_ui_url();
    let http_port: u16 = web_url
        .trim_end_matches('/')
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let http_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), http_port);

    // 1. Embedded Web UI HTML served on `/`
    let index_resp = send_http(http_addr, "GET", "/", &[], "").await;
    assert!(index_resp.starts_with("HTTP/1.1 200 OK"), "{index_resp}");
    assert!(index_resp.contains("NVRDesk Control Panel"));

    // 2. REST API status & pause/resume
    let status_resp = send_http(http_addr, "GET", "/api/v1/status", &[], "").await;
    assert!(status_resp.starts_with("HTTP/1.1 200 OK"));
    assert!(status_resp.contains("\"paused\":false"));

    let pause_resp = send_http(http_addr, "POST", "/api/v1/control/pause", &[], "").await;
    assert!(pause_resp.starts_with("HTTP/1.1 200 OK"));
    assert!(handle.is_paused());

    let resume_resp = send_http(http_addr, "POST", "/api/v1/control/resume", &[], "").await;
    assert!(resume_resp.starts_with("HTTP/1.1 200 OK"));
    assert!(!handle.is_paused());

    // 3. Multi-track RTSP (Video trackID=0 + Audio PCMU trackID=1)
    let (rtsp_srv, rtsp_addr, mut handles) = RtspServer::bind_streams(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        None,
        vec![RtspStreamSpec {
            path: "/av".to_string(),
            aliases: Vec::new(),
            name: "AV Test".to_string(),
            audio_codec: Some(AudioCodecKind::Pcmu),
        }],
    )
    .await
    .expect("bind av rtsp");
    let av_handle = handles.remove("/av").expect("av handle");
    let av_task = tokio::spawn(async move {
        let _ = rtsp_srv.serve().await;
    });

    let av_url = format!("rtsp://127.0.0.1:{}/av", rtsp_addr.port());
    let mut sock = TcpStream::connect(rtsp_addr).await.expect("connect av");

    // Background feeder publishes 1 keyframe + 1 PCMU audio packet once PLAY starts.
    let av_pub = av_handle.clone();
    let feeder = tokio::spawn(async move {
        let mut enc = OpenH264Encoder::new(&StreamConfig {
            fps: 10,
            bitrate_bps: 400_000,
            max_width: 320,
            idr_interval_secs: 1,
            show_cursor: false,
        })
        .expect("enc");
        let frame = nvrdesk_core::I420Frame::new(320, 240, 0).expect("i420");
        for _ in 0..15 {
            let encoded = enc.encode(&frame, true).expect("encode");
            av_pub.publish_frame(encoded);
            av_pub.publish_audio(encode_g711(&[0i16; 160], AudioCodecKind::Pcmu, 160));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    let desc = send_rtsp(
        &mut sock,
        &format!("DESCRIBE {av_url} RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
    )
    .await;
    assert!(
        desc.contains("m=audio 0 RTP/AVP 0"),
        "missing audio in SDP: {desc}"
    );

    let s_v = send_rtsp(
        &mut sock,
        &format!("SETUP {av_url}/trackID=0 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"),
    )
    .await;
    assert!(s_v.starts_with("RTSP/1.0 200 OK"));
    let session_id = s_v
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("session:"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.split(';').next().unwrap_or("").trim().to_string())
        .expect("session id");

    let s_a = send_rtsp(
        &mut sock,
        &format!("SETUP {av_url}/trackID=1 RTSP/1.0\r\nCSeq: 3\r\nSession: {session_id}\r\nTransport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n\r\n"),
    )
    .await;
    assert!(
        s_a.starts_with("RTSP/1.0 200 OK"),
        "audio SETUP failed: {s_a}"
    );

    let play = send_rtsp(
        &mut sock,
        &format!("PLAY {av_url} RTSP/1.0\r\nCSeq: 4\r\nSession: {session_id}\r\n\r\n"),
    )
    .await;
    assert!(play.starts_with("RTSP/1.0 200 OK"));

    let mut saw_video_ch = false;
    let mut saw_audio_ch = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = Vec::new();
    while Instant::now() < deadline && (!saw_video_ch || !saw_audio_ch) {
        let mut tmp = [0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut tmp))
            .await
            .expect("av rtp timeout")
            .expect("av rtp read");
        assert!(n > 0);
        buf.extend_from_slice(&tmp[..n]);
        while buf.len() >= 4 {
            if buf[0] != b'$' {
                buf.remove(0);
                continue;
            }
            let ch = buf[1];
            let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            if buf.len() < 4 + len {
                break;
            }
            if ch == 0 {
                saw_video_ch = true;
            } else if ch == 2 {
                saw_audio_ch = true;
            }
            buf.drain(..4 + len);
        }
    }
    assert!(
        saw_video_ch && saw_audio_ch,
        "expected both video (ch 0) and audio (ch 2) RTP packets"
    );

    feeder.abort();
    av_task.abort();
    shutdown.store(true, Ordering::Release);
    let _ = worker.join();
}
