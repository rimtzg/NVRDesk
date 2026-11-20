//! NVRDesk Phase 1 CLI: Dual-stream RTSP + POS Input Overlays + ONVIF Profile S & WS-Discovery.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, ValueEnum};
use nvrdesk_capture::{
    CaptureSource, ScreenCaptureSource, TestPatternSource, focused_window_title,
};
use nvrdesk_codec::{OpenH264Encoder, VideoEncoder};
use nvrdesk_core::{
    CaptureSourceKind, KeystrokeMode, MonitorBounds, NvrDeskConfig, NvrDeskError, RawVideoFrame,
    StreamConfig,
};
use nvrdesk_input::InputTracker;
use nvrdesk_onvif::{
    OnvifAuthConfig, OnvifDeviceMeta, OnvifMediaProfile, OnvifServer, SnapshotProvider,
    WsDiscoveryConfig, run_ws_discovery,
};
use nvrdesk_pipeline::{FrameProcessor, compute_target_dimensions};
use nvrdesk_rtsp::{
    RtspAuthConfig, RtspServer, RtspStreamHandle, RtspStreamSpec, normalize_stream_path,
};
use tokio::sync::watch;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

const IDLE_GRACE_PERIOD: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliSourceKind {
    Screen,
    TestPattern,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliKeystrokeMode {
    All,
    ShortcutsAndSpecial,
    MaskedDigits,
    Off,
}

#[derive(Debug, Parser)]
#[command(
    name = "nvrdesk",
    version,
    about = "NVRDesk: Open-source ONVIF/RTSP virtual IP camera for POS & desktop monitoring"
)]
struct Cli {
    /// Path to TOML configuration file.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Print the effective TOML configuration and exit.
    #[arg(long, default_value_t = false)]
    print_config: bool,

    /// IP address to bind RTSP and ONVIF servers to.
    #[arg(long)]
    bind: Option<IpAddr>,

    /// RTSP listening port.
    #[arg(long)]
    port: Option<u16>,

    /// ONVIF HTTP/SOAP & Snapshot listening port.
    #[arg(long)]
    http_port: Option<u16>,

    /// Disable ONVIF HTTP/SOAP and WS-Discovery servers.
    #[arg(long, default_value_t = false)]
    no_onvif: bool,

    /// Disable ONVIF WS-Discovery multicast responder.
    #[arg(long, default_value_t = false)]
    no_discovery: bool,

    /// Video source backend.
    #[arg(long, value_enum)]
    source: Option<CliSourceKind>,

    /// Display index (0 = primary) when using `--source screen`.
    #[arg(long)]
    display: Option<usize>,

    /// MainStream target FPS.
    #[arg(long)]
    fps: Option<u32>,

    /// MainStream target H.264 bitrate in bps.
    #[arg(long)]
    bitrate: Option<u32>,

    /// MainStream maximum width.
    #[arg(long)]
    max_width: Option<u32>,

    /// Disable the SubStream profile (`/stream2`).
    #[arg(long, default_value_t = false)]
    no_substream: bool,

    /// Hide the OS mouse cursor in screen capture.
    #[arg(long, default_value_t = false)]
    no_cursor: bool,

    /// Disable mouse click and keystroke overlays.
    #[arg(long, default_value_t = false)]
    no_overlays: bool,

    /// Keystroke OSD visibility/privacy mode.
    #[arg(long, value_enum)]
    keystroke_mode: Option<CliKeystrokeMode>,

    /// Optional username for RTSP Digest & ONVIF WS-Security authentication.
    #[arg(long)]
    username: Option<String>,

    /// Optional password for RTSP Digest & ONVIF WS-Security authentication.
    #[arg(long)]
    password: Option<String>,

    /// Keep capturing and encoding even when zero RTSP clients are connected.
    #[arg(long, default_value_t = false)]
    always_on: bool,
}

struct LiveSnapshotProvider {
    latest_frame: Arc<RwLock<Option<RawVideoFrame>>>,
    latest_bounds: Arc<RwLock<Option<MonitorBounds>>>,
    tracker: InputTracker,
    source_kind: CaptureSourceKind,
    display_index: usize,
    show_cursor: bool,
    main_max_width: u32,
    sub_max_width: u32,
    overlays_enabled: bool,
}

impl SnapshotProvider for LiveSnapshotProvider {
    fn capture_jpeg(&self, profile_slug: &str) -> Result<Vec<u8>, NvrDeskError> {
        let cached = self.latest_frame.read().expect("latest_frame lock").clone();
        let (raw, bounds) = if let Some(frame) = cached {
            let bounds = *self.latest_bounds.read().expect("latest_bounds lock");
            (frame, bounds)
        } else {
            // Zero-idle fallback: grab a single on-demand frame when no RTSP stream is active.
            let mut src = build_source(self.source_kind, self.display_index, 5, self.show_cursor);
            src.start()?;
            let frame = src
                .try_next_frame(Duration::from_millis(1500))?
                .ok_or_else(|| {
                    NvrDeskError::Capture("timed out capturing snapshot frame".into())
                })?;
            let bounds = src.bounds();
            src.stop();
            (frame, bounds)
        };

        let max_width = if profile_slug.eq_ignore_ascii_case("sub") {
            self.sub_max_width
        } else {
            self.main_max_width
        };

        let snap = if self.overlays_enabled {
            Some(self.tracker.snapshot(bounds, Instant::now()))
        } else {
            None
        };

        let mut proc = FrameProcessor::new();
        proc.encode_jpeg_snapshot(&raw, max_width, 82, snap.as_ref())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let mut cfg = if let Some(path) = &cli.config {
        NvrDeskConfig::from_toml_file(path)?
    } else {
        NvrDeskConfig::default()
    };

    apply_cli_overrides(&cli, &mut cfg);

    if cli.print_config {
        println!("{}", toml::to_string_pretty(&cfg)?);
        return Ok(());
    }

    let bind_ip: IpAddr = cfg.rtsp.bind.parse()?;
    let main_path = normalize_stream_path(&cfg.main_stream.path);
    let sub_path = normalize_stream_path(&cfg.sub_stream.path);

    let rtsp_auth = match (&cfg.rtsp.username, &cfg.rtsp.password) {
        (Some(u), Some(p)) if !u.is_empty() => Some(RtspAuthConfig {
            username: u.clone(),
            password: p.clone(),
        }),
        _ => None,
    };

    let mut specs = vec![RtspStreamSpec {
        path: main_path.clone(),
        aliases: if main_path == "/stream" {
            Vec::new()
        } else {
            vec!["/stream".to_string()]
        },
        name: format!("{} (Main)", cfg.onvif.device_name),
    }];

    if cfg.sub_stream.enabled {
        specs.push(RtspStreamSpec {
            path: sub_path.clone(),
            aliases: Vec::new(),
            name: format!("{} (Sub)", cfg.onvif.device_name),
        });
    }

    let (rtsp_server, rtsp_addr, mut handles) =
        RtspServer::bind_streams(SocketAddr::new(bind_ip, cfg.rtsp.port), rtsp_auth, specs).await?;
    let main_handle = handles.remove(&main_path).expect("main handle");
    let sub_handle = if cfg.sub_stream.enabled {
        handles.remove(&sub_path)
    } else {
        None
    };

    let tracker = InputTracker::new(
        cfg.input_overlay.clone(),
        Some(Arc::new(focused_window_title)),
    );
    if cfg.input_overlay.enabled {
        tracker.start_os_listener();
    }

    let latest_frame = Arc::new(RwLock::new(None));
    let latest_bounds = Arc::new(RwLock::new(None));

    let display_ip = if rtsp_addr.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        rtsp_addr.ip().to_string()
    };

    info!(
        main_rtsp = %format!("rtsp://{display_ip}:{}{main_path}", rtsp_addr.port()),
        sub_rtsp = %if cfg.sub_stream.enabled {
            format!("rtsp://{display_ip}:{}{sub_path}", rtsp_addr.port())
        } else {
            "disabled".to_string()
        },
        source = ?cfg.source.kind,
        overlays = cfg.input_overlay.enabled,
        keystroke_mode = ?cfg.input_overlay.keystroke_mode,
        "NVRDesk RTSP streams ready"
    );

    // Bind ONVIF server + WS-Discovery if enabled.
    let onvif_server = if cfg.onvif.enabled {
        let onvif_auth = match (&cfg.onvif.username, &cfg.onvif.password) {
            (Some(u), Some(p)) if !u.is_empty() => Some(OnvifAuthConfig {
                username: u.clone(),
                password: p.clone(),
            }),
            _ => None,
        };

        let (main_w, main_h) = compute_target_dimensions(1920, 1080, cfg.main_stream.max_width);
        let mut profiles = vec![OnvifMediaProfile {
            token: "MainProfile".into(),
            name: "MainStream".into(),
            encoder_token: "MainEncoder".into(),
            width: main_w,
            height: main_h,
            fps: cfg.main_stream.fps,
            bitrate_kbps: cfg.main_stream.bitrate_bps / 1000,
            gov_length: cfg.main_stream.fps * cfg.main_stream.idr_interval_secs,
            rtsp_path: main_path.clone(),
            snapshot_slug: "main".into(),
        }];

        if cfg.sub_stream.enabled {
            let (sub_w, sub_h) = compute_target_dimensions(1920, 1080, cfg.sub_stream.max_width);
            profiles.push(OnvifMediaProfile {
                token: "SubProfile".into(),
                name: "SubStream".into(),
                encoder_token: "SubEncoder".into(),
                width: sub_w,
                height: sub_h,
                fps: cfg.sub_stream.fps,
                bitrate_kbps: cfg.sub_stream.bitrate_bps / 1000,
                gov_length: cfg.sub_stream.fps * cfg.sub_stream.idr_interval_secs,
                rtsp_path: sub_path.clone(),
                snapshot_slug: "sub".into(),
            });
        }

        let snapshots = Arc::new(LiveSnapshotProvider {
            latest_frame: Arc::clone(&latest_frame),
            latest_bounds: Arc::clone(&latest_bounds),
            tracker: tracker.clone(),
            source_kind: cfg.source.kind,
            display_index: cfg.source.display,
            show_cursor: cfg.source.show_cursor,
            main_max_width: cfg.main_stream.max_width,
            sub_max_width: cfg.sub_stream.max_width,
            overlays_enabled: cfg.input_overlay.enabled,
        });

        let meta = OnvifDeviceMeta {
            device_name: cfg.onvif.device_name.clone(),
            manufacturer: cfg.onvif.manufacturer.clone(),
            model: cfg.onvif.model.clone(),
            firmware_version: env!("CARGO_PKG_VERSION").to_string(),
            serial_number: cfg.onvif.serial_number.clone(),
            hardware_id: "NVRDESK-RUST-V1".to_string(),
            rtsp_port: rtsp_addr.port(),
            http_port: cfg.onvif.http_port,
        };

        let srv = OnvifServer::bind(
            SocketAddr::new(bind_ip, cfg.onvif.http_port),
            meta,
            profiles,
            onvif_auth,
            snapshots,
        )
        .await?;

        let http_port = srv.local_addr().port();
        info!(
            device_service = %format!("http://{display_ip}:{http_port}/onvif/device_service"),
            snapshot_main = %format!("http://{display_ip}:{http_port}/snapshot/main"),
            "NVRDesk ONVIF Profile S ready"
        );

        if cfg.onvif.discovery_enabled {
            let uuid = cfg
                .onvif
                .device_uuid
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok())
                .unwrap_or_else(|| {
                    Uuid::new_v5(&Uuid::NAMESPACE_DNS, cfg.onvif.serial_number.as_bytes())
                });
            let disc_cfg = WsDiscoveryConfig {
                device_uuid: uuid,
                http_port,
                device_name: cfg.onvif.device_name.clone(),
                model: cfg.onvif.model.clone(),
            };
            tokio::spawn(async move {
                if let Err(err) = run_ws_discovery(disc_cfg).await {
                    warn!(error = %err, "WS-Discovery responder stopped");
                }
            });
        }

        Some(srv)
    } else {
        None
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_worker = Arc::clone(&shutdown);
    let worker_cfg = cfg.clone();

    let worker_handle = std::thread::Builder::new()
        .name("nvrdesk-pipeline-worker".into())
        .spawn(move || {
            run_pipeline_worker(
                worker_cfg,
                main_handle,
                sub_handle,
                tracker,
                latest_frame,
                latest_bounds,
                shutdown_worker,
            );
        })?;

    tokio::select! {
        res = rtsp_server.serve() => {
            if let Err(err) = res {
                error!(error = %err, "RTSP server exited with error");
            }
        }
        res = async {
            if let Some(onvif) = onvif_server {
                onvif.serve().await
            } else {
                std::future::pending().await
            }
        } => {
            if let Err(err) = res {
                error!(error = %err, "ONVIF server exited with error");
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

fn apply_cli_overrides(cli: &Cli, cfg: &mut NvrDeskConfig) {
    if let Some(bind) = cli.bind {
        cfg.rtsp.bind = bind.to_string();
    }
    if let Some(port) = cli.port {
        cfg.rtsp.port = port;
    }
    if let Some(http_port) = cli.http_port {
        cfg.onvif.http_port = http_port;
    }
    if cli.no_onvif {
        cfg.onvif.enabled = false;
    }
    if cli.no_discovery {
        cfg.onvif.discovery_enabled = false;
    }
    if let Some(source) = cli.source {
        cfg.source.kind = match source {
            CliSourceKind::Screen => CaptureSourceKind::Screen,
            CliSourceKind::TestPattern => CaptureSourceKind::TestPattern,
        };
    }
    if let Some(display) = cli.display {
        cfg.source.display = display;
    }
    if let Some(fps) = cli.fps {
        cfg.main_stream.fps = fps.max(1);
    }
    if let Some(bitrate) = cli.bitrate {
        cfg.main_stream.bitrate_bps = bitrate.max(100_000);
    }
    if let Some(max_width) = cli.max_width {
        cfg.main_stream.max_width = max_width;
    }
    if cli.no_substream {
        cfg.sub_stream.enabled = false;
    }
    if cli.no_cursor {
        cfg.source.show_cursor = false;
    }
    if cli.no_overlays {
        cfg.input_overlay.enabled = false;
    }
    if let Some(mode) = cli.keystroke_mode {
        cfg.input_overlay.keystroke_mode = match mode {
            CliKeystrokeMode::All => KeystrokeMode::All,
            CliKeystrokeMode::ShortcutsAndSpecial => KeystrokeMode::ShortcutsAndSpecial,
            CliKeystrokeMode::MaskedDigits => KeystrokeMode::MaskedDigits,
            CliKeystrokeMode::Off => KeystrokeMode::Off,
        };
    }
    if let Some(user) = &cli.username {
        cfg.rtsp.username = Some(user.clone());
        cfg.onvif.username = Some(user.clone());
    }
    if let Some(pass) = &cli.password {
        cfg.rtsp.password = Some(pass.clone());
        cfg.onvif.password = Some(pass.clone());
    }
    if cli.always_on {
        cfg.always_on = true;
    }
}

fn build_source(
    kind: CaptureSourceKind,
    display_index: usize,
    fps: u32,
    show_cursor: bool,
) -> Box<dyn CaptureSource> {
    match kind {
        CaptureSourceKind::Screen => {
            Box::new(ScreenCaptureSource::new(display_index, fps, show_cursor))
        }
        CaptureSourceKind::TestPattern => Box::new(TestPatternSource::new(1280, 720, fps)),
    }
}

struct StreamPipeline {
    handle: RtspStreamHandle,
    demand_rx: watch::Receiver<usize>,
    stream_cfg: StreamConfig,
    processor: FrameProcessor,
    encoder: Option<OpenH264Encoder>,
    frame_interval: Duration,
    idr_interval: Duration,
    next_emit: Instant,
    last_idr: Instant,
    frames_encoded: u64,
}

impl StreamPipeline {
    fn new(handle: RtspStreamHandle, stream_cfg: StreamConfig) -> Self {
        let demand_rx = handle.subscribe_demand();
        let frame_interval = Duration::from_secs_f64(1.0 / f64::from(stream_cfg.fps.max(1)));
        let idr_interval = Duration::from_secs(u64::from(stream_cfg.idr_interval_secs.max(1)));
        let now = Instant::now();
        Self {
            handle,
            demand_rx,
            stream_cfg,
            processor: FrameProcessor::new(),
            encoder: None,
            frame_interval,
            idr_interval,
            next_emit: now,
            last_idr: now - idr_interval,
            frames_encoded: 0,
        }
    }

    fn has_demand(&self, always_on: bool) -> bool {
        always_on || *self.demand_rx.borrow() > 0
    }
}

fn run_pipeline_worker(
    cfg: NvrDeskConfig,
    main_handle: RtspStreamHandle,
    sub_handle: Option<RtspStreamHandle>,
    tracker: InputTracker,
    latest_frame: Arc<RwLock<Option<RawVideoFrame>>>,
    latest_bounds: Arc<RwLock<Option<MonitorBounds>>>,
    shutdown: Arc<AtomicBool>,
) {
    let mut main_pipe = StreamPipeline::new(
        main_handle,
        cfg.main_stream.to_stream_config(cfg.source.show_cursor),
    );
    let mut sub_pipe = sub_handle
        .map(|h| StreamPipeline::new(h, cfg.sub_stream.to_stream_config(cfg.source.show_cursor)));

    while !shutdown.load(Ordering::Acquire) {
        let any_demand = main_pipe.has_demand(cfg.always_on)
            || sub_pipe
                .as_ref()
                .is_some_and(|p| p.has_demand(cfg.always_on));

        if !any_demand {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        let base_fps = cfg.main_stream.fps.max(if cfg.sub_stream.enabled {
            cfg.sub_stream.fps
        } else {
            1
        });

        let mut source = build_source(
            cfg.source.kind,
            cfg.source.display,
            base_fps,
            cfg.source.show_cursor,
        );
        if let Err(err) = source.start() {
            error!(error = %err, source = %source.name(), "failed to start capture source");
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }

        *latest_bounds.write().expect("latest_bounds lock") = source.bounds();

        run_active_session(
            source.as_mut(),
            &cfg,
            &mut main_pipe,
            sub_pipe.as_mut(),
            &tracker,
            &latest_frame,
            &shutdown,
        );
        source.stop();
        main_pipe.encoder = None;
        if let Some(sub) = sub_pipe.as_mut() {
            sub.encoder = None;
        }
    }
}

fn run_active_session(
    source: &mut dyn CaptureSource,
    cfg: &NvrDeskConfig,
    main_pipe: &mut StreamPipeline,
    mut sub_pipe: Option<&mut StreamPipeline>,
    tracker: &InputTracker,
    latest_frame: &Arc<RwLock<Option<RawVideoFrame>>>,
    shutdown: &AtomicBool,
) {
    info!(source = %source.name(), "capture & dual-stream session started");

    let base_fps = cfg
        .main_stream
        .fps
        .max(if cfg.sub_stream.enabled {
            cfg.sub_stream.fps
        } else {
            1
        })
        .max(1);
    let tick_interval = Duration::from_secs_f64(1.0 / f64::from(base_fps));

    let session_start = Instant::now();
    let mut next_tick = Instant::now();
    let mut last_frame: Option<RawVideoFrame> = None;
    let mut last_demand_at = Instant::now();
    let mut last_stats = Instant::now();
    let bounds = source.bounds();

    while !shutdown.load(Ordering::Acquire) {
        let now = Instant::now();
        let main_active = main_pipe.has_demand(cfg.always_on);
        let sub_active = sub_pipe
            .as_ref()
            .is_some_and(|p| p.has_demand(cfg.always_on));

        if main_active || sub_active {
            last_demand_at = now;
        } else if now.saturating_duration_since(last_demand_at) >= IDLE_GRACE_PERIOD {
            info!(
                "no active RTSP demand after grace period; pausing capture & encoding (zero-idle)"
            );
            break;
        }

        match source.try_next_frame(tick_interval) {
            Ok(Some(raw)) => {
                *latest_frame.write().expect("latest_frame lock") = Some(raw.clone());
                last_frame = Some(raw);
            }
            Ok(None) => {
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
            let now = Instant::now();
            let overlay = if cfg.input_overlay.enabled {
                Some(tracker.snapshot(bounds, now))
            } else {
                None
            };

            if main_active {
                tick_stream_pipeline(main_pipe, raw, overlay.as_ref(), now);
            } else {
                main_pipe.encoder = None;
            }

            if let Some(sub) = sub_pipe.as_deref_mut() {
                if sub_active {
                    tick_stream_pipeline(sub, raw, overlay.as_ref(), now);
                } else {
                    sub.encoder = None;
                }
            }
        }

        if last_stats.elapsed() >= Duration::from_secs(10) {
            let sub_frames = sub_pipe.as_ref().map_or(0, |s| s.frames_encoded);
            let sub_players = sub_pipe.as_ref().map_or(0, |s| s.handle.active_players());
            info!(
                main_frames = main_pipe.frames_encoded,
                main_players = main_pipe.handle.active_players(),
                sub_frames,
                sub_players,
                "streaming stats"
            );
            last_stats = Instant::now();
        }

        // Fixed-cadence sleep keeps exact FPS without cumulative drift.
        next_tick += tick_interval;
        let now = Instant::now();
        if next_tick > now {
            std::thread::sleep(next_tick - now);
        } else if now.duration_since(next_tick) > tick_interval * 3 {
            next_tick = now;
        }
    }
}

fn tick_stream_pipeline(
    pipe: &mut StreamPipeline,
    raw: &RawVideoFrame,
    overlay: Option<&nvrdesk_core::InputOverlaySnapshot>,
    now: Instant,
) {
    if now < pipe.next_emit {
        return;
    }
    pipe.next_emit += pipe.frame_interval;
    if now > pipe.next_emit + pipe.frame_interval * 2 {
        pipe.next_emit = now;
    }

    if pipe.encoder.is_none() {
        match OpenH264Encoder::new(&pipe.stream_cfg) {
            Ok(enc) => {
                pipe.encoder = Some(enc);
                pipe.last_idr = now - pipe.idr_interval;
            }
            Err(err) => {
                warn!(error = %err, "failed to initialize encoder");
                return;
            }
        }
    }

    let Some(encoder) = pipe.encoder.as_mut() else {
        return;
    };

    match pipe
        .processor
        .process_with_overlay(raw, pipe.stream_cfg.max_width, overlay)
    {
        Ok(i420) => {
            let force_idr = now.saturating_duration_since(pipe.last_idr) >= pipe.idr_interval;
            match encoder.encode(&i420, force_idr) {
                Ok(encoded) => {
                    if encoded.is_keyframe {
                        pipe.last_idr = now;
                    }
                    pipe.handle.publish_frame(encoded);
                    pipe.frames_encoded += 1;
                }
                Err(err) => warn!(error = %err, "video encode failed"),
            }
        }
        Err(err) => warn!(error = %err, "frame processing failed"),
    }
}
