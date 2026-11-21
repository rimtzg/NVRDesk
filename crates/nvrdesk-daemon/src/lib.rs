//! NVRDesk daemon orchestrator: multi-stream RTSP, ONVIF Profile S, Web UI, webcam, mic & ring recorder.

pub mod recorder;
pub mod web_ui;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use axum::Router;
use nvrdesk_capture::{
    AudioCaptureSource, CaptureSource, ScreenCaptureSource, TestPatternSource, WebcamCaptureSource,
    focused_window_title,
};
use nvrdesk_codec::{VideoEncoder, audio::encode_g711, create_video_encoder};
use nvrdesk_core::{
    CaptureSourceKind, EncoderConfig, InputOverlaySnapshot, MonitorBounds, NvrDeskConfig,
    NvrDeskError, PipPosition, RawVideoFrame, StreamConfig, WebcamMode,
};
use nvrdesk_input::InputTracker;
use nvrdesk_onvif::{
    OnvifAuthConfig, OnvifDeviceMeta, OnvifMediaProfile, SnapshotProvider, WsDiscoveryConfig,
    build_onvif_router, run_ws_discovery,
};
use nvrdesk_pipeline::{FrameProcessor, compute_target_dimensions};
use nvrdesk_rtsp::{
    RtspAuthConfig, RtspServer, RtspStreamHandle, RtspStreamSpec, normalize_stream_path,
};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    recorder::{RecordingStats, RingRecorder},
    web_ui::{DaemonSharedState, DaemonStatus, EndpointInfo, build_web_ui_router},
};

const IDLE_GRACE_PERIOD: Duration = Duration::from_secs(2);

/// Control handle returned by [`start_daemon`] for the System Tray or CLI.
#[derive(Clone)]
pub struct DaemonHandle {
    shared: DaemonSharedState,
    http_port: u16,
    rtsp_port: u16,
    shutdown: Arc<AtomicBool>,
}

impl DaemonHandle {
    /// Returns the URL of the embedded Web UI dashboard.
    #[must_use]
    pub fn web_ui_url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.http_port)
    }

    /// Returns the RTSP listening port.
    #[must_use]
    pub const fn rtsp_port(&self) -> u16 {
        self.rtsp_port
    }

    /// Pauses video/audio streaming and encoding.
    pub fn pause(&self) {
        self.shared.paused.store(true, Ordering::Release);
    }

    /// Resumes video/audio streaming and encoding.
    pub fn resume(&self) {
        self.shared.paused.store(false, Ordering::Release);
    }

    /// Returns whether streaming is currently paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Acquire)
    }

    /// Returns a snapshot of current daemon telemetry.
    #[must_use]
    pub fn status(&self) -> DaemonStatus {
        let cfg = self.shared.config.read().expect("config lock");
        DaemonStatus {
            paused: self.is_paused(),
            active_players: (self.shared.active_players_fn)(),
            main_frames: self.shared.main_frames.load(Ordering::Relaxed),
            sub_frames: self.shared.sub_frames.load(Ordering::Relaxed),
            active_encoder: self
                .shared
                .active_encoder
                .read()
                .expect("encoder lock")
                .clone(),
            onvif_enabled: cfg.onvif.enabled,
            recording: self
                .shared
                .recording_stats
                .read()
                .expect("rec lock")
                .clone(),
            endpoints: (*self.shared.endpoints).clone(),
        }
    }

    /// Signals all daemon background workers to terminate.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

struct LiveSnapshotProvider {
    latest_frame: Arc<RwLock<Option<RawVideoFrame>>>,
    latest_bounds: Arc<RwLock<Option<MonitorBounds>>>,
    latest_pip: Arc<RwLock<Option<RawVideoFrame>>>,
    tracker: Arc<RwLock<InputTracker>>,
    config: Arc<RwLock<NvrDeskConfig>>,
}

impl SnapshotProvider for LiveSnapshotProvider {
    fn capture_jpeg(&self, profile_slug: &str) -> Result<Vec<u8>, NvrDeskError> {
        let cfg = self.config.read().expect("config lock").clone();
        let cached = self.latest_frame.read().expect("latest_frame lock").clone();
        let (raw, bounds) = if let Some(frame) = cached {
            let bounds = *self.latest_bounds.read().expect("latest_bounds lock");
            (frame, bounds)
        } else {
            let mut src = build_screen_source(
                cfg.source.kind,
                cfg.source.display,
                5,
                cfg.source.show_cursor,
            );
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
            cfg.sub_stream.max_width
        } else {
            cfg.main_stream.max_width
        };

        let snap = if cfg.input_overlay.enabled {
            Some(
                self.tracker
                    .read()
                    .expect("tracker lock")
                    .snapshot(bounds, Instant::now()),
            )
        } else {
            None
        };

        let pip_cached = if cfg.webcam.enabled && cfg.webcam.mode == WebcamMode::Pip {
            self.latest_pip.read().expect("latest_pip lock").clone()
        } else {
            None
        };
        let pip = pip_cached
            .as_ref()
            .map(|f| (f, cfg.webcam.pip_position, cfg.webcam.pip_scale));

        let mut proc = FrameProcessor::new();
        proc.encode_jpeg_snapshot_full(&raw, max_width, 82, snap.as_ref(), pip)
    }
}

/// Starts all NVRDesk daemon subsystems on the current Tokio runtime and returns a control handle.
pub async fn start_daemon(
    cfg: NvrDeskConfig,
    config_path: Option<PathBuf>,
    shutdown: Arc<AtomicBool>,
) -> Result<(DaemonHandle, JoinHandle<()>), NvrDeskError> {
    let bind_ip: IpAddr = cfg
        .rtsp
        .bind
        .parse()
        .map_err(|e| NvrDeskError::Config(format!("invalid bind ip: {e}")))?;

    let main_path = normalize_stream_path(&cfg.main_stream.path);
    let sub_path = normalize_stream_path(&cfg.sub_stream.path);
    let webcam_path = normalize_stream_path(&cfg.webcam.channel_path);

    let rtsp_auth = match (&cfg.rtsp.username, &cfg.rtsp.password) {
        (Some(u), Some(p)) if !u.is_empty() => Some(RtspAuthConfig {
            username: u.clone(),
            password: p.clone(),
        }),
        _ => None,
    };

    let audio_codec = if cfg.audio.enabled {
        Some(cfg.audio.codec)
    } else {
        None
    };

    let mut specs = vec![RtspStreamSpec {
        path: main_path.clone(),
        aliases: if main_path == "/stream" {
            Vec::new()
        } else {
            vec!["/stream".to_string()]
        },
        name: format!("{} (Main)", cfg.onvif.device_name),
        audio_codec,
    }];

    if cfg.sub_stream.enabled {
        specs.push(RtspStreamSpec {
            path: sub_path.clone(),
            aliases: Vec::new(),
            name: format!("{} (Sub)", cfg.onvif.device_name),
            audio_codec,
        });
    }

    if cfg.webcam.enabled && cfg.webcam.mode == WebcamMode::Channel {
        specs.push(RtspStreamSpec {
            path: webcam_path.clone(),
            aliases: Vec::new(),
            name: format!("{} (Webcam)", cfg.onvif.device_name),
            audio_codec,
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
    let webcam_handle = if cfg.webcam.enabled && cfg.webcam.mode == WebcamMode::Channel {
        handles.remove(&webcam_path)
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
    let tracker_arc = Arc::new(RwLock::new(tracker));
    let shared_cfg = Arc::new(RwLock::new(cfg.clone()));
    let latest_frame = Arc::new(RwLock::new(None));
    let latest_bounds = Arc::new(RwLock::new(None));
    let latest_pip = Arc::new(RwLock::new(None));

    let snapshots: Arc<dyn SnapshotProvider> = Arc::new(LiveSnapshotProvider {
        latest_frame: Arc::clone(&latest_frame),
        latest_bounds: Arc::clone(&latest_bounds),
        latest_pip: Arc::clone(&latest_pip),
        tracker: Arc::clone(&tracker_arc),
        config: Arc::clone(&shared_cfg),
    });

    // Bind combined HTTP listener (Web UI + REST API + MJPEG Preview + ONVIF SOAP + Snapshots).
    let http_needed = cfg.onvif.enabled || cfg.daemon.web_ui_enabled;
    let mut http_port = cfg.onvif.http_port;
    let http_listener = if http_needed {
        let listener = TcpListener::bind(SocketAddr::new(bind_ip, cfg.onvif.http_port)).await?;
        http_port = listener.local_addr()?.port();
        Some(listener)
    } else {
        None
    };

    let display_ip = if rtsp_addr.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        rtsp_addr.ip().to_string()
    };

    let mut endpoints = vec![EndpointInfo {
        label: "MainStream RTSP".into(),
        url: format!("rtsp://{display_ip}:{}{main_path}", rtsp_addr.port()),
    }];
    if cfg.sub_stream.enabled {
        endpoints.push(EndpointInfo {
            label: "SubStream RTSP".into(),
            url: format!("rtsp://{display_ip}:{}{sub_path}", rtsp_addr.port()),
        });
    }
    if webcam_handle.is_some() {
        endpoints.push(EndpointInfo {
            label: "Webcam RTSP".into(),
            url: format!("rtsp://{display_ip}:{}{webcam_path}", rtsp_addr.port()),
        });
    }
    if cfg.onvif.enabled {
        endpoints.push(EndpointInfo {
            label: "ONVIF Device Service".into(),
            url: format!("http://{display_ip}:{http_port}/onvif/device_service"),
        });
        endpoints.push(EndpointInfo {
            label: "Snapshot Main".into(),
            url: format!("http://{display_ip}:{http_port}/snapshot/main"),
        });
    }
    if cfg.daemon.web_ui_enabled {
        endpoints.push(EndpointInfo {
            label: "Web Control Panel".into(),
            url: format!("http://{display_ip}:{http_port}/"),
        });
    }

    let main_frames = Arc::new(AtomicU64::new(0));
    let sub_frames = Arc::new(AtomicU64::new(0));
    let active_encoder = Arc::new(RwLock::new("idle".to_string()));
    let recording_stats = Arc::new(RwLock::new(RecordingStats::default()));
    let paused = Arc::new(AtomicBool::new(false));
    let config_gen = Arc::new(AtomicU64::new(0));

    let main_for_players = main_handle.clone();
    let sub_for_players = sub_handle.clone();
    let cam_for_players = webcam_handle.clone();
    let active_players_fn = Arc::new(move || {
        main_for_players.active_players()
            + sub_for_players.as_ref().map_or(0, |h| h.active_players())
            + cam_for_players.as_ref().map_or(0, |h| h.active_players())
    });

    let shared_state = DaemonSharedState {
        config: Arc::clone(&shared_cfg),
        config_path,
        config_gen: Arc::clone(&config_gen),
        paused: Arc::clone(&paused),
        main_frames: Arc::clone(&main_frames),
        sub_frames: Arc::clone(&sub_frames),
        active_encoder: Arc::clone(&active_encoder),
        recording_stats: Arc::clone(&recording_stats),
        active_players_fn,
        endpoints: Arc::new(endpoints.clone()),
        snapshots: Arc::clone(&snapshots),
    };

    if let Some(listener) = http_listener {
        let mut router = Router::new();
        if cfg.daemon.web_ui_enabled {
            router = router.merge(build_web_ui_router(shared_state.clone()));
        }
        if cfg.onvif.enabled {
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
                let (sub_w, sub_h) =
                    compute_target_dimensions(1920, 1080, cfg.sub_stream.max_width);
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
            if cfg.webcam.enabled && cfg.webcam.mode == WebcamMode::Channel {
                let (cam_w, cam_h) = compute_target_dimensions(1280, 720, cfg.webcam.max_width);
                profiles.push(OnvifMediaProfile {
                    token: "WebcamProfile".into(),
                    name: "Webcam".into(),
                    encoder_token: "WebcamEncoder".into(),
                    width: cam_w,
                    height: cam_h,
                    fps: cfg.webcam.fps,
                    bitrate_kbps: cfg.webcam.bitrate_bps / 1000,
                    gov_length: cfg.webcam.fps * 2,
                    rtsp_path: webcam_path.clone(),
                    snapshot_slug: "main".into(),
                });
            }
            let meta = OnvifDeviceMeta {
                device_name: cfg.onvif.device_name.clone(),
                manufacturer: cfg.onvif.manufacturer.clone(),
                model: cfg.onvif.model.clone(),
                firmware_version: env!("CARGO_PKG_VERSION").to_string(),
                serial_number: cfg.onvif.serial_number.clone(),
                hardware_id: "NVRDESK-RUST-V2".to_string(),
                rtsp_port: rtsp_addr.port(),
                http_port,
            };
            router = router.merge(build_onvif_router(meta, profiles, onvif_auth, snapshots));

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
        }

        tokio::spawn(async move {
            info!(addr = %listener.local_addr().unwrap_or(SocketAddr::new(bind_ip, http_port)), "HTTP server (Web UI + ONVIF) listening");
            if let Err(err) = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            {
                error!(error = %err, "HTTP server exited with error");
            }
        });
    }

    tokio::spawn(async move {
        if let Err(err) = rtsp_server.serve().await {
            error!(error = %err, "RTSP server exited with error");
        }
    });

    for ep in &endpoints {
        info!(endpoint = %ep.label, url = %ep.url, "endpoint ready");
    }

    let worker_shutdown = Arc::clone(&shutdown);
    let worker_handle = thread::Builder::new()
        .name("nvrdesk-daemon-worker".into())
        .spawn(move || {
            run_worker(
                shared_cfg,
                config_gen,
                paused,
                main_handle,
                sub_handle,
                webcam_handle,
                tracker_arc,
                latest_frame,
                latest_bounds,
                latest_pip,
                main_frames,
                sub_frames,
                active_encoder,
                recording_stats,
                worker_shutdown,
            );
        })?;

    let handle = DaemonHandle {
        shared: shared_state,
        http_port,
        rtsp_port: rtsp_addr.port(),
        shutdown,
    };

    Ok((handle, worker_handle))
}

fn build_screen_source(
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
    enc_cfg: EncoderConfig,
    processor: FrameProcessor,
    encoder: Option<Box<dyn VideoEncoder>>,
    frame_interval: Duration,
    idr_interval: Duration,
    next_emit: Instant,
    last_idr: Instant,
    frames_encoded: Arc<AtomicU64>,
}

impl StreamPipeline {
    fn new(
        handle: RtspStreamHandle,
        stream_cfg: StreamConfig,
        enc_cfg: EncoderConfig,
        frames_encoded: Arc<AtomicU64>,
    ) -> Self {
        let demand_rx = handle.subscribe_demand();
        let frame_interval = Duration::from_secs_f64(1.0 / f64::from(stream_cfg.fps.max(1)));
        let idr_interval = Duration::from_secs(u64::from(stream_cfg.idr_interval_secs.max(1)));
        let now = Instant::now();
        Self {
            handle,
            demand_rx,
            stream_cfg,
            enc_cfg,
            processor: FrameProcessor::new(),
            encoder: None,
            frame_interval,
            idr_interval,
            next_emit: now,
            last_idr: now - idr_interval,
            frames_encoded,
        }
    }

    fn has_demand(&self, always_on: bool) -> bool {
        always_on || *self.demand_rx.borrow() > 0
    }

    fn reconfigure(&mut self, stream_cfg: StreamConfig, enc_cfg: EncoderConfig) {
        self.stream_cfg = stream_cfg;
        self.enc_cfg = enc_cfg;
        self.frame_interval = Duration::from_secs_f64(1.0 / f64::from(self.stream_cfg.fps.max(1)));
        self.idr_interval =
            Duration::from_secs(u64::from(self.stream_cfg.idr_interval_secs.max(1)));
        self.encoder = None;
    }
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    shared_cfg: Arc<RwLock<NvrDeskConfig>>,
    config_gen: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
    main_handle: RtspStreamHandle,
    sub_handle: Option<RtspStreamHandle>,
    webcam_handle: Option<RtspStreamHandle>,
    tracker_arc: Arc<RwLock<InputTracker>>,
    latest_frame: Arc<RwLock<Option<RawVideoFrame>>>,
    latest_bounds: Arc<RwLock<Option<MonitorBounds>>>,
    latest_pip: Arc<RwLock<Option<RawVideoFrame>>>,
    main_frames: Arc<AtomicU64>,
    sub_frames: Arc<AtomicU64>,
    active_encoder: Arc<RwLock<String>>,
    recording_stats: Arc<RwLock<RecordingStats>>,
    shutdown: Arc<AtomicBool>,
) {
    let init_cfg = shared_cfg.read().expect("config lock").clone();
    let mut last_gen = config_gen.load(Ordering::Acquire);

    let mut main_pipe = StreamPipeline::new(
        main_handle,
        init_cfg
            .main_stream
            .to_stream_config(init_cfg.source.show_cursor),
        init_cfg.encoder.clone(),
        main_frames,
    );
    let mut sub_pipe = sub_handle.map(|h| {
        StreamPipeline::new(
            h,
            init_cfg
                .sub_stream
                .to_stream_config(init_cfg.source.show_cursor),
            init_cfg.encoder.clone(),
            sub_frames,
        )
    });
    let mut webcam_pipe = webcam_handle.map(|h| {
        let cam_stream_cfg = StreamConfig {
            fps: init_cfg.webcam.fps.max(1),
            bitrate_bps: init_cfg.webcam.bitrate_bps.max(100_000),
            max_width: init_cfg.webcam.max_width,
            idr_interval_secs: 2,
            show_cursor: false,
        };
        StreamPipeline::new(
            h,
            cam_stream_cfg,
            init_cfg.encoder.clone(),
            Arc::new(AtomicU64::new(0)),
        )
    });

    let mut recorder = RingRecorder::new(init_cfg.recording.clone()).unwrap_or_else(|e| {
        warn!(error = %e, "failed to initialize ring recorder; disabling recording");
        RingRecorder::new(Default::default()).expect("default disabled recorder")
    });

    while !shutdown.load(Ordering::Acquire) {
        let cfg = shared_cfg.read().expect("config lock").clone();
        let cur_gen = config_gen.load(Ordering::Acquire);
        if cur_gen != last_gen {
            last_gen = cur_gen;
            main_pipe.reconfigure(
                cfg.main_stream.to_stream_config(cfg.source.show_cursor),
                cfg.encoder.clone(),
            );
            if let Some(sub) = sub_pipe.as_mut() {
                sub.reconfigure(
                    cfg.sub_stream.to_stream_config(cfg.source.show_cursor),
                    cfg.encoder.clone(),
                );
            }
            recorder.update_config(cfg.recording.clone());
        }

        if paused.load(Ordering::Acquire) {
            *active_encoder.write().expect("encoder lock") = "paused".to_string();
            std::thread::sleep(Duration::from_millis(80));
            continue;
        }

        let need_desktop = main_pipe.has_demand(cfg.always_on)
            || sub_pipe
                .as_ref()
                .is_some_and(|p| p.has_demand(cfg.always_on))
            || cfg.recording.enabled;
        let need_webcam_channel = webcam_pipe
            .as_ref()
            .is_some_and(|p| p.has_demand(cfg.always_on));

        if !need_desktop && !need_webcam_channel {
            *active_encoder.write().expect("encoder lock") = "idle".to_string();
            *recording_stats.write().expect("rec lock") = recorder.stats();
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        let base_fps = cfg
            .main_stream
            .fps
            .max(if cfg.sub_stream.enabled {
                cfg.sub_stream.fps
            } else {
                1
            })
            .max(1);

        let mut screen_src = build_screen_source(
            cfg.source.kind,
            cfg.source.display,
            base_fps,
            cfg.source.show_cursor,
        );
        if need_desktop && let Err(err) = screen_src.start() {
            error!(error = %err, source = %screen_src.name(), "failed to start screen source");
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        *latest_bounds.write().expect("latest_bounds lock") = screen_src.bounds();

        let mut webcam_src = if cfg.webcam.enabled {
            let mut cam = WebcamCaptureSource::new(cfg.webcam.device_index);
            if let Err(err) = cam.start() {
                warn!(error = %err, "failed to start webcam source");
                None
            } else {
                Some(cam)
            }
        } else {
            None
        };

        let mut audio_src = if cfg.audio.enabled {
            let mut mic = AudioCaptureSource::new(cfg.audio.device_name.clone());
            if let Err(err) = mic.start() {
                warn!(error = %err, "failed to start microphone capture");
                None
            } else {
                Some(mic)
            }
        } else {
            None
        };

        run_active_loop(
            &shared_cfg,
            &config_gen,
            &mut last_gen,
            &paused,
            screen_src.as_mut(),
            webcam_src.as_mut(),
            audio_src.as_mut(),
            &mut main_pipe,
            sub_pipe.as_mut(),
            webcam_pipe.as_mut(),
            &tracker_arc,
            &latest_frame,
            &latest_pip,
            &active_encoder,
            &mut recorder,
            &recording_stats,
            &shutdown,
        );

        screen_src.stop();
        if let Some(mut cam) = webcam_src {
            cam.stop();
        }
        if let Some(mut mic) = audio_src {
            mic.stop();
        }
        main_pipe.encoder = None;
        if let Some(sub) = sub_pipe.as_mut() {
            sub.encoder = None;
        }
        if let Some(cam) = webcam_pipe.as_mut() {
            cam.encoder = None;
        }
        recorder.flush_and_close();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_active_loop(
    shared_cfg: &Arc<RwLock<NvrDeskConfig>>,
    config_gen: &Arc<AtomicU64>,
    last_gen: &mut u64,
    paused: &Arc<AtomicBool>,
    screen_src: &mut dyn CaptureSource,
    mut webcam_src: Option<&mut WebcamCaptureSource>,
    mut audio_src: Option<&mut AudioCaptureSource>,
    main_pipe: &mut StreamPipeline,
    mut sub_pipe: Option<&mut StreamPipeline>,
    mut webcam_pipe: Option<&mut StreamPipeline>,
    tracker_arc: &Arc<RwLock<InputTracker>>,
    latest_frame: &Arc<RwLock<Option<RawVideoFrame>>>,
    latest_pip: &Arc<RwLock<Option<RawVideoFrame>>>,
    active_encoder: &Arc<RwLock<String>>,
    recorder: &mut RingRecorder,
    recording_stats: &Arc<RwLock<RecordingStats>>,
    shutdown: &AtomicBool,
) {
    let mut cfg = shared_cfg.read().expect("config lock").clone();
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
    let mut last_cam_frame: Option<RawVideoFrame> = None;
    let mut last_demand_at = Instant::now();
    let mut audio_pts: u32 = 0;
    let bounds = screen_src.bounds();

    while !shutdown.load(Ordering::Acquire) && !paused.load(Ordering::Acquire) {
        let cur_gen = config_gen.load(Ordering::Acquire);
        if cur_gen != *last_gen {
            cfg = shared_cfg.read().expect("config lock").clone();
            *last_gen = cur_gen;
            main_pipe.reconfigure(
                cfg.main_stream.to_stream_config(cfg.source.show_cursor),
                cfg.encoder.clone(),
            );
            if let Some(sub) = sub_pipe.as_deref_mut() {
                sub.reconfigure(
                    cfg.sub_stream.to_stream_config(cfg.source.show_cursor),
                    cfg.encoder.clone(),
                );
            }
            recorder.update_config(cfg.recording.clone());
            // Break out so webcam/audio/display source changes re-open cleanly.
            break;
        }

        let now = Instant::now();
        let main_active = main_pipe.has_demand(cfg.always_on) || cfg.recording.enabled;
        let sub_active = sub_pipe
            .as_ref()
            .is_some_and(|p| p.has_demand(cfg.always_on));
        let cam_active = webcam_pipe
            .as_ref()
            .is_some_and(|p| p.has_demand(cfg.always_on));

        if main_active || sub_active || cam_active {
            last_demand_at = now;
        } else if now.saturating_duration_since(last_demand_at) >= IDLE_GRACE_PERIOD {
            info!("no active demand after grace period; pausing (zero-idle)");
            break;
        }

        if main_active || sub_active {
            match screen_src.try_next_frame(tick_interval) {
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
                    warn!(error = %err, "screen capture error; restarting");
                    break;
                }
            }
        }

        if let Some(cam) = webcam_src.as_deref_mut()
            && let Ok(Some(cam_raw)) = cam.try_next_frame(Duration::from_millis(2))
        {
            *latest_pip.write().expect("latest_pip lock") = Some(cam_raw.clone());
            last_cam_frame = Some(cam_raw);
        }

        if let Some(mic) = audio_src.as_deref_mut() {
            while let Ok(Some(samples)) = mic.try_next_chunk(Duration::ZERO) {
                let pkt = encode_g711(&samples, cfg.audio.codec, audio_pts);
                audio_pts = audio_pts.wrapping_add(samples.len() as u32);
                main_pipe.handle.publish_audio(pkt.clone());
                if let Some(sub) = sub_pipe.as_deref() {
                    sub.handle.publish_audio(pkt.clone());
                }
                if let Some(cam_p) = webcam_pipe.as_deref() {
                    cam_p.handle.publish_audio(pkt);
                }
            }
        }

        let now = Instant::now();
        if let Some(raw) = &last_frame {
            let overlay = if cfg.input_overlay.enabled {
                Some(
                    tracker_arc
                        .read()
                        .expect("tracker lock")
                        .snapshot(bounds, now),
                )
            } else {
                None
            };

            let pip = if cfg.webcam.enabled && cfg.webcam.mode == WebcamMode::Pip {
                last_cam_frame
                    .as_ref()
                    .map(|f| (f, cfg.webcam.pip_position, cfg.webcam.pip_scale))
            } else {
                None
            };

            if main_active {
                if let Some(encoded) = tick_stream_pipeline(
                    main_pipe,
                    raw,
                    overlay.as_ref(),
                    pip,
                    now,
                    Some(active_encoder),
                ) {
                    recorder.write_frame(&encoded);
                    main_pipe.handle.publish_frame(encoded);
                    *recording_stats.write().expect("rec lock") = recorder.stats();
                }
            } else {
                main_pipe.encoder = None;
            }

            if let Some(sub) = sub_pipe.as_deref_mut() {
                if sub_active {
                    if let Some(encoded) =
                        tick_stream_pipeline(sub, raw, overlay.as_ref(), pip, now, None)
                    {
                        sub.handle.publish_frame(encoded);
                    }
                } else {
                    sub.encoder = None;
                }
            }
        }

        if let Some(cam_pipe) = webcam_pipe.as_deref_mut()
            && cam_active
            && let Some(cam_raw) = &last_cam_frame
            && let Some(encoded) = tick_stream_pipeline(cam_pipe, cam_raw, None, None, now, None)
        {
            cam_pipe.handle.publish_frame(encoded);
        }

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
    overlay: Option<&InputOverlaySnapshot>,
    pip: Option<(&RawVideoFrame, PipPosition, f32)>,
    now: Instant,
    encoder_label: Option<&Arc<RwLock<String>>>,
) -> Option<nvrdesk_core::EncodedFrame> {
    if now < pipe.next_emit {
        return None;
    }
    pipe.next_emit += pipe.frame_interval;
    if now > pipe.next_emit + pipe.frame_interval * 2 {
        pipe.next_emit = now;
    }

    if pipe.encoder.is_none() {
        match create_video_encoder(&pipe.stream_cfg, &pipe.enc_cfg) {
            Ok(enc) => {
                if let Some(lbl) = encoder_label {
                    *lbl.write().expect("encoder lock") = enc.name().to_string();
                }
                pipe.encoder = Some(enc);
                pipe.last_idr = now - pipe.idr_interval;
            }
            Err(err) => {
                warn!(error = %err, "failed to initialize encoder");
                return None;
            }
        }
    }

    let encoder = pipe.encoder.as_mut()?;
    let i420 = pipe
        .processor
        .process_full(raw, pipe.stream_cfg.max_width, overlay, pip)
        .ok()?;
    let force_idr = pipe.handle.take_keyframe_request()
        || now.saturating_duration_since(pipe.last_idr) >= pipe.idr_interval;

    match encoder.encode(&i420, force_idr) {
        Ok(encoded) => {
            if encoded.is_keyframe {
                pipe.last_idr = now;
            }
            pipe.frames_encoded.fetch_add(1, Ordering::Relaxed);
            Some(encoded)
        }
        Err(err) => {
            warn!(error = %err, "video encode failed");
            None
        }
    }
}
