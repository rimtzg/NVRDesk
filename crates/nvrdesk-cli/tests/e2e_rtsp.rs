//! End-to-end RTSP streaming integration test (TestPattern -> Pipeline -> OpenH264 -> RTSP).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use nvrdesk_capture::{CaptureSource, TestPatternSource};
use nvrdesk_codec::{OpenH264Encoder, VideoEncoder};
use nvrdesk_core::StreamConfig;
use nvrdesk_pipeline::FrameProcessor;
use nvrdesk_rtsp::{RtspServer, RtspServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test]
async fn streams_h264_over_rtsp_tcp_interleaved_and_passes_ffprobe() {
    let cfg = StreamConfig {
        fps: 15,
        bitrate_bps: 800_000,
        max_width: 640,
        idr_interval_secs: 1,
        show_cursor: true,
    };

    let rtsp_cfg = RtspServerConfig {
        bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        stream_path: "/stream".to_string(),
        stream_name: "NVRDesk E2E Test".to_string(),
    };

    let (server, addr, handle) = RtspServer::bind(rtsp_cfg).await.expect("bind rtsp");
    let server_task = tokio::spawn(async move {
        let _ = server.serve().await;
    });

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_worker = Arc::clone(&shutdown);
    let demand_rx = handle.subscribe_demand();
    let cfg_clone = cfg.clone();

    let worker = std::thread::spawn(move || {
        let mut source = TestPatternSource::new(640, 360, cfg_clone.fps);
        let mut processor = FrameProcessor::new();
        let mut encoder = OpenH264Encoder::new(&cfg_clone).expect("encoder");
        let frame_interval = Duration::from_millis(40);
        let mut started = false;
        let mut frame_num = 0u64;

        while !shutdown_worker.load(Ordering::Acquire) {
            if *demand_rx.borrow() == 0 {
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
            if let Ok(Some(raw)) = source.try_next_frame(frame_interval)
                && let Ok(i420) = processor.process(&raw, cfg_clone.max_width)
            {
                let force_idr = frame_num.is_multiple_of(15);
                if let Ok(enc) = encoder.encode(&i420, force_idr) {
                    handle.publish_frame(enc);
                    frame_num += 1;
                }
            }
        }
        if started {
            source.stop();
        }
    });

    // 1. Verify RTSP handshake + RTP interleaved packet delivery with raw client.
    let url = format!("rtsp://127.0.0.1:{}/stream", addr.port());
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

    // Read interleaved RTP packets until we verify at least 3 valid H.264 RTP packets.
    let mut rtp_packets_seen = 0usize;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = Vec::new();
    while Instant::now() < deadline && rtp_packets_seen < 3 {
        let mut tmp = [0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut tmp))
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
            assert_eq!(rtp[0] >> 6, 2, "RTP version must be 2");
            assert_eq!(rtp[1] & 0x7F, 96, "RTP payload type must be 96");
            rtp_packets_seen += 1;
            buf.drain(..4 + pkt_len);
        }
    }
    assert!(
        rtp_packets_seen >= 3,
        "expected at least 3 RTP packets, got {rtp_packets_seen}"
    );
    drop(sock);

    // 2. If ffprobe is available, verify real demuxing & decoding of H.264 640x360 stream.
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
            "ffprobe failed (status={:?}): stdout={stdout} stderr={stderr}",
            output.status.code()
        );
        assert!(
            stdout.contains("codec_name=h264"),
            "unexpected ffprobe output: {stdout}"
        );
        assert!(stdout.contains("width=640"), "unexpected width: {stdout}");
        assert!(stdout.contains("height=360"), "unexpected height: {stdout}");
    }

    shutdown.store(true, Ordering::Release);
    server_task.abort();
    let _ = worker.join();
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
