//! NVRDesk Phase 0 CLI: Screen + Cursor -> I420 -> OpenH264 -> RTSP -> VLC/ffplay.

use std::net::{IpAddr, SocketAddr};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, ValueEnum};
use nvrdesk_capture::{CaptureSource, ScreenCaptureSource, TestPatternSource};
use nvrdesk_codec::{OpenH264Encoder, VideoEncoder};
use nvrdesk_core::{RawVideoFrame, StreamConfig};
use nvrdesk_pipeline::FrameProcessor;
use nvrdesk_rtsp::{RtspServer, RtspServerConfig, RtspStreamHandle};
use tokio::sync::watch;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SourceKind {
    Screen,
    TestPattern,
}

#[derive(Debug, Parser)]
#[command(
    name = "nvrdesk",
    version,
    about = "Open-source virtual IP camera (Phase 0 PoC)"
)]
struct Cli {
    /// IP address to bind the RTSP server to.
    #[arg(long, default_value = "0.0.0.0")]
    bind: IpAddr,

    /// RTSP listening port.
    #[arg(long, default_value_t = 8554)]
    port: u16,

    /// RTSP stream path.
    #[arg(long, default_value = "/stream")]
    path: String,

    /// Video source backend.
    #[arg(long, value_enum, default_value_t = SourceKind::Screen)]
    source: SourceKind,

    /// Display index (0 = primary) when using `--source screen`.
    #[arg(long, default_value_t = 0)]
    display: usize,

    /// Target frames per second.
    #[arg(long, default_value_t = 15)]
    fps: u32,

    /// Target H.264 bitrate in bits per second.
    #[arg(long, default_value_t = 2_000_000)]
    bitrate: u32,

    /// Maximum output width (scales down preserving aspect ratio).
    #[arg(long, default_value_t = 1920)]
    max_width: u32,

    /// Keyframe (IDR) interval in seconds.
    #[arg(long, default_value_t = 2)]
    idr_interval: u32,

    /// Hide the OS mouse cursor in screen capture.
    #[arg(long, default_value_t = false)]
    no_cursor: bool,

    /// Keep capturing and encoding even when zero RTSP clients are connected.
    #[arg(long, default_value_t = false)]
    always_on: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let stream_cfg = StreamConfig {
        fps: cli.fps.max(1),
        bitrate_bps: cli.bitrate.max(100_000),
        max_width: cli.max_width,
        idr_interval_secs: cli.idr_interval.max(1),
        show_cursor: !cli.no_cursor,
    };

    let rtsp_cfg = RtspServerConfig {
        bind_addr: SocketAddr::new(cli.bind, cli.port),
        stream_path: cli.path.clone(),
        stream_name: "NVRDesk Screen Stream".to_string(),
    };

    let (server, local_addr, handle) = RtspServer::bind(rtsp_cfg).await?;
    let display_ip = if local_addr.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        local_addr.ip().to_string()
    };
    let clean_path = if cli.path.starts_with('/') {
        cli.path.clone()
    } else {
        format!("/{}", cli.path)
    };

    info!(
        rtsp_url = %format!("rtsp://{display_ip}:{}{clean_path}", local_addr.port()),
        source = ?cli.source,
        fps = stream_cfg.fps,
        bitrate_bps = stream_cfg.bitrate_bps,
        max_width = stream_cfg.max_width,
        show_cursor = stream_cfg.show_cursor,
        always_on = cli.always_on,
        "NVRDesk RTSP server ready — open the URL above in VLC or ffplay"
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_worker = Arc::clone(&shutdown);
    let source_kind = cli.source;
    let display_index = cli.display;
    let always_on = cli.always_on;

    let worker_handle = std::thread::Builder::new()
        .name("nvrdesk-pipeline-worker".into())
        .spawn(move || {
            run_pipeline_worker(
                source_kind,
                display_index,
                stream_cfg,
                handle,
                always_on,
                shutdown_worker,
            );
        })?;

    tokio::select! {
        res = server.serve() => {
            if let Err(err) = res {
                error!(error = %err, "RTSP server exited with error");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("received Ctrl+C, shutting down");
        }
    }

    shutdown.store(true, Ordering::Release);
    let _ = worker_handle.join();
    Ok(())
}

fn build_source(
    kind: SourceKind,
    display_index: usize,
    cfg: &StreamConfig,
) -> Box<dyn CaptureSource> {
    match kind {
        SourceKind::Screen => Box::new(ScreenCaptureSource::new(
            display_index,
            cfg.fps,
            cfg.show_cursor,
        )),
        SourceKind::TestPattern => Box::new(TestPatternSource::new(1280, 720, cfg.fps)),
    }
}

fn run_pipeline_worker(
    source_kind: SourceKind,
    display_index: usize,
    cfg: StreamConfig,
    handle: RtspStreamHandle,
    always_on: bool,
    shutdown: Arc<AtomicBool>,
) {
    let demand_rx = handle.subscribe_demand();

    while !shutdown.load(Ordering::Acquire) {
        if !always_on && *demand_rx.borrow() == 0 {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        let mut source = build_source(source_kind, display_index, &cfg);
        if let Err(err) = source.start() {
            error!(error = %err, source = %source.name(), "failed to start capture source");
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }

        let mut encoder = match OpenH264Encoder::new(&cfg) {
            Ok(enc) => enc,
            Err(err) => {
                error!(error = %err, "failed to initialize H.264 encoder");
                source.stop();
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        let mut processor = FrameProcessor::new();
        run_active_session(
            source.as_mut(),
            &mut processor,
            &mut encoder,
            &cfg,
            &handle,
            &demand_rx,
            always_on,
            &shutdown,
        );
        source.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_active_session(
    source: &mut dyn CaptureSource,
    processor: &mut FrameProcessor,
    encoder: &mut dyn VideoEncoder,
    cfg: &StreamConfig,
    handle: &RtspStreamHandle,
    demand_rx: &watch::Receiver<usize>,
    always_on: bool,
    shutdown: &AtomicBool,
) {
    info!(source = %source.name(), "capture & encoding session started");
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(cfg.fps.max(1)));
    let idr_interval = Duration::from_secs(u64::from(cfg.idr_interval_secs.max(1)));

    let session_start = Instant::now();
    let mut last_frame: Option<RawVideoFrame> = None;
    let mut last_idr = Instant::now() - idr_interval;
    let mut last_stats = Instant::now();
    let mut frames_encoded: u64 = 0;

    while !shutdown.load(Ordering::Acquire) {
        if !always_on && *demand_rx.borrow() == 0 {
            info!("no active RTSP demand; pausing capture & encoding (zero-idle)");
            break;
        }

        let tick_start = Instant::now();

        match source.try_next_frame(frame_interval) {
            Ok(Some(raw)) => {
                last_frame = Some(raw);
            }
            Ok(None) => {
                // Static desktop: repeat previous frame with updated timestamp to keep NVR/VLC alive.
                if let Some(prev) = &mut last_frame {
                    prev.timestamp_us = session_start.elapsed().as_micros() as u64;
                }
            }
            Err(err) => {
                warn!(error = %err, "capture error; restarting session");
                break;
            }
        }

        if let Some(raw) = &last_frame {
            match processor.process(raw, cfg.max_width) {
                Ok(i420) => {
                    let force_idr = last_idr.elapsed() >= idr_interval;
                    match encoder.encode(&i420, force_idr) {
                        Ok(encoded) => {
                            if encoded.is_keyframe {
                                last_idr = Instant::now();
                            }
                            handle.publish_frame(encoded);
                            frames_encoded += 1;
                        }
                        Err(err) => warn!(error = %err, "video encode failed"),
                    }
                }
                Err(err) => warn!(error = %err, "frame processing failed"),
            }
        }

        if last_stats.elapsed() >= Duration::from_secs(10) {
            info!(
                frames_encoded,
                active_players = handle.active_players(),
                "streaming stats"
            );
            last_stats = Instant::now();
        }

        let elapsed = tick_start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }
}
