//! Pure-Rust SRT (Secure Reliable Transport) publisher streaming MPEG-TS over Caller/Listener sockets.

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use bytes::Bytes;
use nvrdesk_core::{SrtConfig, SrtMode};
use serde::{Deserialize, Serialize};
use srt_tokio::SrtSocket;
use tokio::{net::lookup_host, sync::broadcast, sync::watch};
use tracing::{info, warn};

use crate::{server::RtspStreamHandle, ts_mux::TsMuxer};

/// Live telemetry for the SRT transport publisher exposed via `GET /api/v1/status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SrtStats {
    pub enabled: bool,
    pub mode: SrtMode,
    pub target: Option<String>,
    pub connected: bool,
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub last_error: Option<String>,
}

/// Spawns a managed SRT transport task that watches `srt_cfg_rx` for hot-reload and streams MPEG-TS chunks.
pub fn spawn_srt_manager(
    mut srt_cfg_rx: watch::Receiver<SrtConfig>,
    mut stream_rx: watch::Receiver<Option<RtspStreamHandle>>,
    stats: Arc<RwLock<SrtStats>>,
    paused: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);

        while !shutdown.load(Ordering::Acquire) {
            let cfg = srt_cfg_rx.borrow_and_update().clone();
            let stream_opt = stream_rx.borrow_and_update().clone();

            {
                let mut st = stats.write().expect("srt stats lock");
                st.enabled = cfg.enabled;
                st.mode = cfg.mode;
                st.target = cfg.target.clone();
                if !cfg.enabled {
                    st.connected = false;
                    st.last_error = None;
                }
            }

            if !cfg.enabled {
                tokio::select! {
                    _ = srt_cfg_rx.changed() => {}
                    _ = stream_rx.changed() => {}
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                }
                backoff = Duration::from_secs(1);
                continue;
            }

            let Some(stream) = stream_opt else {
                tokio::select! {
                    _ = srt_cfg_rx.changed() => {}
                    _ = stream_rx.changed() => {}
                    _ = tokio::time::sleep(Duration::from_millis(300)) => {}
                }
                continue;
            };

            let target = cfg.target.as_deref().map(str::trim).unwrap_or("");
            if target.is_empty() {
                set_srt_error(&stats, "srt.target is empty".into());
                tokio::select! {
                    _ = srt_cfg_rx.changed() => {}
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
                continue;
            }

            let resolved = match resolve_socket_target(target).await {
                Ok(addr) => addr,
                Err(err) => {
                    set_srt_error(&stats, err);
                    tokio::select! {
                        _ = srt_cfg_rx.changed() => {}
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                    continue;
                }
            };

            let builder = build_srt_socket_builder(&cfg);
            let connect_fut = async {
                match cfg.mode {
                    SrtMode::Caller => builder.call(resolved, cfg.stream_id.as_deref()).await,
                    SrtMode::Listener => builder.listen_on(resolved).await,
                }
            };

            let socket_res = tokio::select! {
                res = connect_fut => Some(res),
                _ = srt_cfg_rx.changed() => None,
                _ = stream_rx.changed() => None,
            };

            let Some(socket_res) = socket_res else {
                continue;
            };

            match socket_res {
                Ok(mut socket) => {
                    backoff = Duration::from_secs(1);
                    {
                        let mut st = stats.write().expect("srt stats lock");
                        st.connected = true;
                        st.last_error = None;
                    }
                    info!(mode = ?cfg.mode, %resolved, "SRT peer connected");

                    stream_srt_loop(
                        &mut socket,
                        &stream,
                        &mut srt_cfg_rx,
                        &mut stream_rx,
                        &stats,
                        &paused,
                        &shutdown,
                    )
                    .await;

                    {
                        let mut st = stats.write().expect("srt stats lock");
                        st.connected = false;
                    }
                    let _ = socket.close_and_finish().await;
                }
                Err(err) => {
                    warn!(mode = ?cfg.mode, %resolved, error = %err, "SRT connection failed");
                    set_srt_error(&stats, err.to_string());
                    tokio::select! {
                        _ = srt_cfg_rx.changed() => {}
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                }
            }
        }
    })
}

async fn resolve_socket_target(target: &str) -> Result<std::net::SocketAddr, String> {
    if let Ok(addr) = target.parse() {
        return Ok(addr);
    }
    lookup_host(target)
        .await
        .map_err(|e| format!("failed to resolve SRT target '{target}': {e}"))?
        .next()
        .ok_or_else(|| format!("SRT target '{target}' resolved to zero addresses"))
}

fn build_srt_socket_builder(cfg: &SrtConfig) -> srt_tokio::SrtSocketBuilder {
    let mut builder =
        SrtSocket::builder().latency(Duration::from_millis(u64::from(cfg.latency_ms.max(20))));
    if let Some(pass) = &cfg.passphrase
        && (10..=79).contains(&pass.len())
    {
        builder = builder.encryption(16, pass.clone());
    }
    builder
}

fn set_srt_error(stats: &Arc<RwLock<SrtStats>>, msg: String) {
    let mut st = stats.write().expect("srt stats lock");
    st.connected = false;
    st.last_error = Some(msg);
}

async fn stream_srt_loop(
    socket: &mut SrtSocket,
    stream: &RtspStreamHandle,
    srt_cfg_rx: &mut watch::Receiver<SrtConfig>,
    stream_rx: &mut watch::Receiver<Option<RtspStreamHandle>>,
    stats: &Arc<RwLock<SrtStats>>,
    paused: &Arc<AtomicBool>,
    shutdown: &Arc<AtomicBool>,
) {
    let _demand = stream.acquire_demand();
    stream.request_keyframe();

    let mut video_rx = stream.subscribe_video();
    let mut muxer = TsMuxer::new();
    let mut sent_keyframe = false;

    while !shutdown.load(Ordering::Acquire) {
        let frame = tokio::select! {
            _ = srt_cfg_rx.changed() => break,
            _ = stream_rx.changed() => break,
            res = video_rx.recv() => match res {
                Ok(f) => f,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    muxer.reset();
                    sent_keyframe = false;
                    stream.request_keyframe();
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        };

        if paused.load(Ordering::Acquire) {
            continue;
        }
        if !sent_keyframe {
            if !frame.is_keyframe {
                continue;
            }
            sent_keyframe = true;
        }

        let chunks: Vec<Bytes> = muxer.mux_video_frame(&frame);
        let now = Instant::now();
        for chunk in chunks {
            let chunk_len = chunk.len() as u64;
            let mut pending = (now, chunk);
            let mut retries = 0u8;
            loop {
                match socket.try_send(pending.0, pending.1) {
                    Ok(()) => {
                        let mut st = stats.write().expect("srt stats lock");
                        st.packets_sent += 1;
                        st.bytes_sent += chunk_len;
                        break;
                    }
                    Err(returned) => {
                        retries = retries.saturating_add(1);
                        if retries > 10 || shutdown.load(Ordering::Acquire) {
                            set_srt_error(
                                stats,
                                "SRT send buffer disconnected or saturated".into(),
                            );
                            return;
                        }
                        pending = returned;
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RtspServer, RtspServerConfig};
    use nvrdesk_core::{EncodedFrame, VideoCodecKind};
    use tokio::net::UdpSocket;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn srt_loopback_caller_to_listener_transfers_mpegts_chunks() {
        let probe = UdpSocket::bind("127.0.0.1:0").await.expect("bind probe");
        let port = probe.local_addr().expect("addr").port();
        drop(probe);

        let (_srv, _addr, stream_handle) = RtspServer::bind(RtspServerConfig {
            bind_addr: "127.0.0.1:0".parse().expect("ip"),
            ..Default::default()
        })
        .await
        .expect("rtsp bind");

        let listen_addr = format!("127.0.0.1:{port}");
        let receiver_handle = tokio::spawn(async move {
            let mut rx = SrtSocket::builder()
                .latency(Duration::from_millis(40))
                .listen_on(listen_addr.as_str())
                .await
                .expect("srt listener bind");
            use futures_core::Stream;
            use std::task::Poll;
            std::future::poll_fn(|cx| match std::pin::Pin::new(&mut rx).poll_next(cx) {
                Poll::Ready(item) => Poll::Ready(item),
                Poll::Pending => Poll::Pending,
            })
            .await
        });

        // Give listener a moment to open UDP socket, then start manager as Caller.
        tokio::time::sleep(Duration::from_millis(60)).await;

        let srt_cfg = SrtConfig {
            enabled: true,
            mode: SrtMode::Caller,
            target: Some(format!("127.0.0.1:{port}")),
            stream_id: Some("pos-test-01".into()),
            passphrase: None,
            latency_ms: 40,
            source_stream: "main".into(),
        };
        let (_cfg_tx, cfg_rx) = watch::channel(srt_cfg);
        let (_st_tx, st_rx) = watch::channel(Some(stream_handle.clone()));
        let stats = Arc::new(RwLock::new(SrtStats::default()));
        let paused = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));

        let mgr = spawn_srt_manager(
            cfg_rx,
            st_rx,
            Arc::clone(&stats),
            paused,
            Arc::clone(&shutdown),
        );

        // Publish keyframes periodically until SRT connects and delivers the first TS datagram.
        let mut recv_fut = receiver_handle;
        let payload = tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                stream_handle.publish_frame(EncodedFrame {
                    codec: VideoCodecKind::H264,
                    nals: vec![Bytes::from_static(&[0x65, 0x88, 0x84, 0x21])],
                    vps: None,
                    sps: Some(Bytes::from_static(&[0x67, 0x42, 0xE0, 0x1E])),
                    pps: Some(Bytes::from_static(&[0x68, 0xCE, 0x38, 0x80])),
                    is_keyframe: true,
                    pts_90khz: 90_000,
                });
                tokio::select! {
                    res = &mut recv_fut => break res,
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
        })
        .await
        .expect("SRT packet timeout")
        .expect("receiver task")
        .expect("received item")
        .expect("valid srt item");

        assert!(!payload.1.is_empty());
        assert_eq!(payload.1[0], 0x47);
        assert_eq!(payload.1.len() % 188, 0);

        shutdown.store(true, Ordering::Release);
        mgr.abort();
    }
}
