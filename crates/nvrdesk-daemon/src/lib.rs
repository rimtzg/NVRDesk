//! NVRDesk daemon orchestrator: multi-stream RTSP/RTSPS, ONVIF Profile S/T, Web UI, POS OSD, motion events, webcam, mic/backchannel & ring recorder.

pub mod pos_osd;
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

use axum::{Router, extract::ConnectInfo};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder as HttpAutoBuilder,
    service::TowerToHyperService,
};
use nvrdesk_capture::{
    AudioCaptureSource, AudioOutputSink, CaptureSource, FfmpegPullSource, ScreenCaptureSource,
    TestPatternSource, WebcamCaptureSource, capture_single_screen_frame, focused_window_title,
};
use nvrdesk_codec::{
    VideoEncoder,
    audio::{decode_g711, encode_g711},
    create_video_encoder,
};
use nvrdesk_core::{
    CaptureSourceKind, EncoderConfig, InputOverlaySnapshot, MonitorBounds, NvrDeskConfig,
    NvrDeskError, PipPosition, PosOverlaySnapshot, RawVideoFrame, SourceConfig, StreamConfig,
    VideoCodecKind, tls::build_tls_server_config,
};
use nvrdesk_input::InputTracker;
use nvrdesk_onvif::{
    EventBroker, OnvifAuthConfig, OnvifDeviceMeta, OnvifMediaProfile, SnapshotProvider,
    WsDiscoveryConfig, build_onvif_router_with_events, run_ws_discovery_watch,
};
use nvrdesk_pipeline::{FrameProcessor, compute_target_dimensions, motion::MotionDetector};
use nvrdesk_rtsp::{
    BackchannelSink, RtspAuthConfig, RtspRegistry, RtspServer, RtspStreamHandle, RtspStreamSpec,
    SrtStats, normalize_stream_path, spawn_srt_manager,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle as AsyncJoinHandle;
use tokio_rustls::{TlsAcceptor, rustls};
use tower::ServiceExt;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    pos_osd::PosOsdStore,
    recorder::{RecordingStats, RingRecorder},
    web_ui::{
        DaemonSharedState, DaemonStatus, VirtualWebcamPorts, build_endpoints_with_virtual,
        build_web_ui_router, select_srt_handle,
    },
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
    /// Returns the URL of the embedded Web UI dashboard (`http://` or `https://`).
    #[must_use]
    pub fn web_ui_url(&self) -> String {
        let scheme = if self.shared.https_enabled {
            "https"
        } else {
            "http"
        };
        format!("{scheme}://127.0.0.1:{}/", self.http_port)
    }

    /// Returns the RTSP listening port.
    #[must_use]
    pub const fn rtsp_port(&self) -> u16 {
        self.rtsp_port
    }

    /// Returns the RTSPS (TLS) listening port, if enabled.
    #[must_use]
    pub const fn rtsps_port(&self) -> Option<u16> {
        self.shared.rtsps_port
    }

    /// Returns whether HTTPS is enabled on the HTTP/ONVIF/Web UI port.
    #[must_use]
    pub const fn https_enabled(&self) -> bool {
        self.shared.https_enabled
    }

    /// Returns the RTSP port of the standalone Virtual ONVIF Webcam server, if active.
    #[must_use]
    pub fn virtual_webcam_rtsp_port(&self) -> Option<u16> {
        self.shared
            .virtual_webcam_ports
            .read()
            .expect("virtual webcam ports lock")
            .map(|p| p.rtsp_port)
    }

    /// Returns the ONVIF HTTP(S) port of the standalone Virtual ONVIF Webcam server, if active.
    #[must_use]
    pub fn virtual_webcam_http_port(&self) -> Option<u16> {
        self.shared
            .virtual_webcam_ports
            .read()
            .expect("virtual webcam ports lock")
            .and_then(|p| p.http_port)
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

    /// Returns whether the System Tray icon should currently be shown.
    #[must_use]
    pub fn tray_enabled(&self) -> bool {
        self.shared
            .config
            .read()
            .expect("config lock")
            .daemon
            .tray_enabled
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
            https_enabled: self.shared.https_enabled,
            rtsps_port: self.shared.rtsps_port,
            backchannel_packets: self.shared.backchannel_packets.load(Ordering::Relaxed),
            recording: self
                .shared
                .recording_stats
                .read()
                .expect("rec lock")
                .clone(),
            srt: self
                .shared
                .srt_stats
                .read()
                .expect("srt stats lock")
                .clone(),
            endpoints: build_endpoints_with_virtual(
                &cfg,
                &self.shared.display_ip,
                self.shared.rtsp_port,
                self.shared.rtsps_port,
                self.shared.http_port,
                self.shared.https_enabled,
                self.shared
                    .virtual_webcam_ports
                    .read()
                    .expect("virtual webcam ports lock")
                    .as_ref(),
            ),
            active_osd_lines: self.shared.pos_osd.active_lines(Instant::now()),
            recent_events: self.shared.events.recent(10),
        }
    }

    /// Signals all daemon background workers to terminate.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

fn capture_webcam_jpeg_snapshot(
    latest_pip: &RwLock<Option<RawVideoFrame>>,
    cfg: &NvrDeskConfig,
    max_width: u32,
) -> Result<Vec<u8>, NvrDeskError> {
    let cam_frame = if let Some(cached) = latest_pip.read().expect("latest_pip lock").clone() {
        cached
    } else if cfg.webcam.enabled {
        let mut cam = WebcamCaptureSource::new(cfg.webcam.device_index);
        cam.start()?;
        let frame = cam
            .try_next_frame(Duration::from_millis(1500))?
            .ok_or_else(|| NvrDeskError::Capture("timed out capturing webcam snapshot".into()))?;
        cam.stop();
        frame
    } else {
        return Err(NvrDeskError::Capture("webcam is disabled".into()));
    };
    let mut proc = FrameProcessor::new();
    proc.encode_jpeg_snapshot_full(&cam_frame, max_width, 82, None, None)
}

struct LiveSnapshotProvider {
    latest_frame: Arc<RwLock<Option<RawVideoFrame>>>,
    latest_bounds: Arc<RwLock<Option<MonitorBounds>>>,
    latest_pip: Arc<RwLock<Option<RawVideoFrame>>>,
    tracker: Arc<RwLock<InputTracker>>,
    pos_osd: Arc<PosOsdStore>,
    config: Arc<RwLock<NvrDeskConfig>>,
}

impl SnapshotProvider for LiveSnapshotProvider {
    fn capture_jpeg(&self, profile_slug: &str) -> Result<Vec<u8>, NvrDeskError> {
        let cfg = self.config.read().expect("config lock").clone();
        let now = Instant::now();

        if profile_slug.eq_ignore_ascii_case("webcam") {
            return capture_webcam_jpeg_snapshot(&self.latest_pip, &cfg, cfg.webcam.max_width);
        }

        let cached = self.latest_frame.read().expect("latest_frame lock").clone();
        let (raw, bounds) = if let Some(frame) = cached {
            let bounds = *self.latest_bounds.read().expect("latest_bounds lock");
            (frame, bounds)
        } else if cfg.source.kind == CaptureSourceKind::Screen {
            capture_single_screen_frame(cfg.source.display)?
        } else {
            let mut src = build_screen_source(&cfg.source, &cfg.encoder.ffmpeg_path, 5);
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
                    .snapshot(bounds, now),
            )
        } else {
            None
        };

        let pip_cached = if cfg.webcam.uses_pip() {
            self.latest_pip.read().expect("latest_pip lock").clone()
        } else {
            None
        };
        let pip = pip_cached
            .as_ref()
            .map(|f| (f, cfg.webcam.pip_position, cfg.webcam.pip_scale));
        let pos_snap = self.pos_osd.snapshot(&cfg.pos, now);

        let mut proc = FrameProcessor::new();
        proc.encode_jpeg_snapshot_with_pos(
            &raw,
            max_width,
            82,
            snap.as_ref(),
            pip,
            pos_snap.as_ref(),
        )
    }
}

struct WebcamSnapshotProvider {
    latest_pip: Arc<RwLock<Option<RawVideoFrame>>>,
    config: Arc<RwLock<NvrDeskConfig>>,
}

impl SnapshotProvider for WebcamSnapshotProvider {
    fn capture_jpeg(&self, profile_slug: &str) -> Result<Vec<u8>, NvrDeskError> {
        let cfg = self.config.read().expect("config lock").clone();
        let max_width = if profile_slug.eq_ignore_ascii_case("sub") {
            webcam_sub_max_width(cfg.webcam.max_width)
        } else {
            cfg.webcam.max_width
        };
        capture_webcam_jpeg_snapshot(&self.latest_pip, &cfg, max_width)
    }
}

#[derive(Clone)]
struct VirtualWebcamHandles {
    main_handle: RtspStreamHandle,
    sub_handle: RtspStreamHandle,
    registry: RtspRegistry,
}

#[derive(Clone, PartialEq, Eq)]
struct VirtualWebcamSignature {
    bind: String,
    rtsp_port: u16,
    rtsps_enabled: bool,
    rtsps_port: u16,
    onvif_enabled: bool,
    onvif_http_port: u16,
    https_enabled: bool,
    device_name: String,
    serial_number: String,
    rtsp_username: Option<String>,
    rtsp_password: Option<String>,
    onvif_username: Option<String>,
    onvif_password: Option<String>,
    audio_enabled: bool,
    audio_codec: nvrdesk_core::AudioCodecKind,
    max_width: u32,
    fps: u32,
    bitrate_bps: u32,
}

impl VirtualWebcamSignature {
    fn from_config(cfg: &NvrDeskConfig) -> Self {
        Self {
            bind: cfg.rtsp.bind.clone(),
            rtsp_port: cfg.webcam.rtsp_port,
            rtsps_enabled: cfg.tls.rtsps_enabled,
            rtsps_port: cfg.webcam.rtsps_port,
            onvif_enabled: cfg.onvif.enabled,
            onvif_http_port: cfg.webcam.onvif_http_port,
            https_enabled: cfg.tls.https_enabled,
            device_name: webcam_device_name(cfg),
            serial_number: format!("{}-CAM", cfg.onvif.serial_number),
            rtsp_username: cfg.rtsp.username.clone(),
            rtsp_password: cfg.rtsp.password.clone(),
            onvif_username: cfg.onvif.username.clone(),
            onvif_password: cfg.onvif.password.clone(),
            audio_enabled: cfg.webcam.audio_enabled || cfg.audio.enabled,
            audio_codec: cfg.audio.codec,
            max_width: cfg.webcam.max_width,
            fps: cfg.webcam.fps,
            bitrate_bps: cfg.webcam.bitrate_bps,
        }
    }
}

struct VirtualWebcamRuntime {
    signature: Option<VirtualWebcamSignature>,
    tasks: Vec<AsyncJoinHandle<()>>,
}

impl VirtualWebcamRuntime {
    fn new() -> Self {
        Self {
            signature: None,
            tasks: Vec::new(),
        }
    }

    fn stop(
        &mut self,
        slots: &Arc<RwLock<Option<VirtualWebcamHandles>>>,
        ports_slot: &Arc<RwLock<Option<VirtualWebcamPorts>>>,
    ) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        *slots.write().expect("virtual webcam slots lock") = None;
        *ports_slot.write().expect("virtual webcam ports lock") = None;
        self.signature = None;
    }
}

fn webcam_device_name(cfg: &NvrDeskConfig) -> String {
    cfg.webcam
        .onvif_device_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(
            || format!("{} (Webcam)", cfg.onvif.device_name),
            str::to_string,
        )
}

fn webcam_sub_max_width(main_max_width: u32) -> u32 {
    if main_max_width == 0 {
        320
    } else {
        (main_max_width / 2).clamp(160, 352) & !1
    }
}

fn webcam_main_stream_config(cfg: &NvrDeskConfig) -> StreamConfig {
    StreamConfig {
        codec: VideoCodecKind::H264,
        fps: cfg.webcam.fps.max(1),
        bitrate_bps: cfg.webcam.bitrate_bps.max(100_000),
        max_width: cfg.webcam.max_width,
        idr_interval_secs: 2,
        show_cursor: false,
    }
}

fn webcam_sub_stream_config(cfg: &NvrDeskConfig) -> StreamConfig {
    StreamConfig {
        codec: VideoCodecKind::H264,
        fps: cfg.webcam.fps.clamp(1, 6),
        bitrate_bps: (cfg.webcam.bitrate_bps / 2).max(150_000),
        max_width: webcam_sub_max_width(cfg.webcam.max_width),
        idr_interval_secs: 2,
        show_cursor: false,
    }
}

fn build_discovery_configs(
    cfg: &NvrDeskConfig,
    http_port: u16,
    webcam_http_port: Option<u16>,
) -> Vec<WsDiscoveryConfig> {
    if !cfg.onvif.enabled || !cfg.onvif.discovery_enabled {
        return Vec::new();
    }

    let desktop_uuid = cfg
        .onvif
        .device_uuid
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(|| Uuid::new_v5(&Uuid::NAMESPACE_DNS, cfg.onvif.serial_number.as_bytes()));

    let mut list = vec![WsDiscoveryConfig {
        device_uuid: desktop_uuid,
        http_port,
        device_name: cfg.onvif.device_name.clone(),
        model: cfg.onvif.model.clone(),
    }];

    if let Some(cam_http_port) = webcam_http_port {
        let cam_identity = format!("{}:{}:webcam", cfg.fleet.site, cfg.fleet.terminal_id);
        let cam_uuid = Uuid::new_v5(&Uuid::NAMESPACE_DNS, cam_identity.as_bytes());
        list.push(WsDiscoveryConfig {
            device_uuid: cam_uuid,
            http_port: cam_http_port,
            device_name: webcam_device_name(cfg),
            model: "VirtualWebcam".to_string(),
        });
    }

    list
}

fn build_tls_for_cfg(
    cfg: &NvrDeskConfig,
    bind_ip: IpAddr,
    extra_san_ip: bool,
) -> Result<Option<Arc<rustls::ServerConfig>>, NvrDeskError> {
    if !cfg.tls.https_enabled && !cfg.tls.rtsps_enabled {
        return Ok(None);
    }
    let mut extra_sans = Vec::new();
    if extra_san_ip && !bind_ip.is_unspecified() {
        extra_sans.push(bind_ip.to_string());
    }
    build_tls_server_config(
        cfg.tls.cert_path.as_deref(),
        cfg.tls.key_path.as_deref(),
        &extra_sans,
        vec![b"http/1.1".to_vec()],
    )
    .map(Some)
}

#[allow(clippy::too_many_arguments)]
async fn reconcile_virtual_webcam_servers(
    cfg: &NvrDeskConfig,
    primary_http_port: u16,
    runtime: &mut VirtualWebcamRuntime,
    slots: &Arc<RwLock<Option<VirtualWebcamHandles>>>,
    ports_slot: &Arc<RwLock<Option<VirtualWebcamPorts>>>,
    latest_pip: &Arc<RwLock<Option<RawVideoFrame>>>,
    shared_cfg: &Arc<RwLock<NvrDeskConfig>>,
    webcam_events: &EventBroker,
    disc_tx: &watch::Sender<Vec<WsDiscoveryConfig>>,
) -> Result<(), NvrDeskError> {
    let want_virtual = cfg.webcam.uses_virtual_onvif();
    if !want_virtual {
        if runtime.signature.is_some() {
            runtime.stop(slots, ports_slot);
            info!("stopped standalone Virtual ONVIF Webcam server");
        }
        let _ = disc_tx.send(build_discovery_configs(cfg, primary_http_port, None));
        return Ok(());
    }

    let next_sig = VirtualWebcamSignature::from_config(cfg);
    if runtime.signature.as_ref() == Some(&next_sig) {
        return Ok(());
    }

    runtime.stop(slots, ports_slot);

    let bind_ip: IpAddr = cfg
        .rtsp
        .bind
        .parse()
        .map_err(|e| NvrDeskError::Config(format!("invalid bind ip: {e}")))?;

    let rtsp_auth = match (&cfg.rtsp.username, &cfg.rtsp.password) {
        (Some(u), Some(p)) if !u.is_empty() => Some(RtspAuthConfig {
            username: u.clone(),
            password: p.clone(),
        }),
        _ => None,
    };

    let cam_audio_enabled = cfg.webcam.audio_enabled || cfg.audio.enabled;
    let audio_codec = cam_audio_enabled.then_some(cfg.audio.codec);
    let device_name = webcam_device_name(cfg);
    let specs = vec![
        RtspStreamSpec {
            path: "/stream1".to_string(),
            aliases: vec!["/stream".to_string()],
            name: format!("{device_name} (Main)"),
            video_codec: VideoCodecKind::H264,
            audio_codec,
            backchannel: false,
        },
        RtspStreamSpec {
            path: "/stream2".to_string(),
            aliases: Vec::new(),
            name: format!("{device_name} (Sub)"),
            video_codec: VideoCodecKind::H264,
            audio_codec,
            backchannel: false,
        },
    ];

    let (rtsp_server, rtsp_addr, mut handles) = RtspServer::bind_streams(
        SocketAddr::new(bind_ip, cfg.webcam.rtsp_port),
        rtsp_auth,
        specs,
    )
    .await?;
    let rtsp_registry = rtsp_server.registry();
    let cam_main_handle = handles.remove("/stream1").expect("webcam main handle");
    let cam_sub_handle = handles.remove("/stream2").expect("webcam sub handle");

    let mut tasks = Vec::new();
    let tls_server_cfg = build_tls_for_cfg(cfg, bind_ip, true)?;
    let mut cam_rtsps_port = None;
    if cfg.tls.rtsps_enabled
        && let Some(tls_cfg) = &tls_server_cfg
    {
        let rtsps_listener =
            TcpListener::bind(SocketAddr::new(bind_ip, cfg.webcam.rtsps_port)).await?;
        cam_rtsps_port = Some(rtsps_listener.local_addr()?.port());
        let rtsp_tls_cfg = build_tls_server_config(
            cfg.tls.cert_path.as_deref(),
            cfg.tls.key_path.as_deref(),
            &[],
            Vec::new(),
        )
        .unwrap_or_else(|_| Arc::clone(tls_cfg));
        rtsp_server.spawn_tls_listener(rtsps_listener, rtsp_tls_cfg);
    }

    let mut cam_http_port = None;
    if cfg.onvif.enabled {
        let listener =
            TcpListener::bind(SocketAddr::new(bind_ip, cfg.webcam.onvif_http_port)).await?;
        let bound_http_port = listener.local_addr()?.port();
        cam_http_port = Some(bound_http_port);

        let onvif_auth = match (&cfg.onvif.username, &cfg.onvif.password) {
            (Some(u), Some(p)) if !u.is_empty() => Some(OnvifAuthConfig {
                username: u.clone(),
                password: p.clone(),
            }),
            _ => None,
        };

        let main_cfg = webcam_main_stream_config(cfg);
        let sub_cfg = webcam_sub_stream_config(cfg);
        let (main_w, main_h) = compute_target_dimensions(1280, 720, main_cfg.max_width);
        let (sub_w, sub_h) = compute_target_dimensions(1280, 720, sub_cfg.max_width);
        let profiles = vec![
            OnvifMediaProfile {
                token: "MainProfile".into(),
                name: "MainStream".into(),
                encoder_token: "MainEncoder".into(),
                encoding: VideoCodecKind::H264,
                width: main_w,
                height: main_h,
                fps: main_cfg.fps,
                bitrate_kbps: main_cfg.bitrate_bps / 1000,
                gov_length: main_cfg.fps * main_cfg.idr_interval_secs,
                rtsp_path: "/stream1".into(),
                snapshot_slug: "main".into(),
            },
            OnvifMediaProfile {
                token: "SubProfile".into(),
                name: "SubStream".into(),
                encoder_token: "SubEncoder".into(),
                encoding: VideoCodecKind::H264,
                width: sub_w,
                height: sub_h,
                fps: sub_cfg.fps,
                bitrate_kbps: sub_cfg.bitrate_bps / 1000,
                gov_length: sub_cfg.fps * sub_cfg.idr_interval_secs,
                rtsp_path: "/stream2".into(),
                snapshot_slug: "sub".into(),
            },
        ];

        let meta = OnvifDeviceMeta {
            device_name: device_name.clone(),
            manufacturer: cfg.onvif.manufacturer.clone(),
            model: "VirtualWebcam".to_string(),
            firmware_version: env!("CARGO_PKG_VERSION").to_string(),
            serial_number: next_sig.serial_number.clone(),
            hardware_id: "NVRDESK-WEBCAM-V1".to_string(),
            rtsp_port: rtsp_addr.port(),
            rtsps_port: cam_rtsps_port,
            http_port: bound_http_port,
            https_enabled: cfg.tls.https_enabled,
            audio_enabled: cam_audio_enabled,
            backchannel_enabled: false,
        };

        let webcam_snapshots: Arc<dyn SnapshotProvider> = Arc::new(WebcamSnapshotProvider {
            latest_pip: Arc::clone(latest_pip),
            config: Arc::clone(shared_cfg),
        });
        let onvif_router = build_onvif_router_with_events(
            meta,
            profiles,
            onvif_auth,
            webcam_snapshots,
            webcam_events.clone(),
        );

        let http_tls = tls_server_cfg.filter(|_| cfg.tls.https_enabled);
        let http_task = tokio::spawn(async move {
            let addr = listener
                .local_addr()
                .unwrap_or(SocketAddr::new(bind_ip, bound_http_port));
            if let Some(tls_cfg) = http_tls {
                info!(%addr, "Virtual Webcam ONVIF HTTPS server listening");
                serve_https_router(listener, onvif_router, tls_cfg).await;
            } else {
                info!(%addr, "Virtual Webcam ONVIF HTTP server listening");
                if let Err(err) = axum::serve(
                    listener,
                    onvif_router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                {
                    error!(error = %err, "Virtual Webcam ONVIF HTTP server exited with error");
                }
            }
        });
        tasks.push(http_task);
    }

    let rtsp_task = tokio::spawn(async move {
        if let Err(err) = rtsp_server.serve().await {
            error!(error = %err, "Virtual Webcam RTSP server exited with error");
        }
    });
    tasks.push(rtsp_task);

    *slots.write().expect("virtual webcam slots lock") = Some(VirtualWebcamHandles {
        main_handle: cam_main_handle,
        sub_handle: cam_sub_handle,
        registry: rtsp_registry,
    });
    *ports_slot.write().expect("virtual webcam ports lock") = Some(VirtualWebcamPorts {
        rtsp_port: rtsp_addr.port(),
        rtsps_port: cam_rtsps_port,
        http_port: cam_http_port,
    });
    runtime.signature = Some(next_sig);
    runtime.tasks = tasks;

    let _ = disc_tx.send(build_discovery_configs(
        cfg,
        primary_http_port,
        cam_http_port,
    ));

    info!(
        rtsp_port = rtsp_addr.port(),
        onvif_http_port = ?cam_http_port,
        device_name = %device_name,
        "standalone Virtual ONVIF Webcam ready (/stream1 + /stream2)"
    );
    Ok(())
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
        video_codec: cfg.main_stream.codec,
        audio_codec,
        backchannel: cfg.audio.backchannel_enabled,
    }];

    if cfg.sub_stream.enabled {
        specs.push(RtspStreamSpec {
            path: sub_path.clone(),
            aliases: Vec::new(),
            name: format!("{} (Sub)", cfg.onvif.device_name),
            video_codec: cfg.sub_stream.codec,
            audio_codec,
            backchannel: false,
        });
    }

    if cfg.webcam.uses_channel() {
        let cam_audio = (cfg.webcam.audio_enabled || cfg.audio.enabled).then_some(cfg.audio.codec);
        specs.push(RtspStreamSpec {
            path: webcam_path.clone(),
            aliases: Vec::new(),
            name: format!("{} (Webcam)", cfg.onvif.device_name),
            video_codec: VideoCodecKind::H264,
            audio_codec: cam_audio,
            backchannel: false,
        });
    }

    let (rtsp_server, rtsp_addr, mut handles) =
        RtspServer::bind_streams(SocketAddr::new(bind_ip, cfg.rtsp.port), rtsp_auth, specs).await?;
    let rtsp_registry = rtsp_server.registry();

    let main_handle = handles.remove(&main_path).expect("main handle");
    let sub_handle = if cfg.sub_stream.enabled {
        handles.remove(&sub_path)
    } else {
        None
    };
    let webcam_handle = if cfg.webcam.uses_channel() {
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
    let pos_osd = Arc::new(PosOsdStore::new());
    let events = EventBroker::new();
    let shared_cfg = Arc::new(RwLock::new(cfg.clone()));
    let latest_frame = Arc::new(RwLock::new(None));
    let latest_bounds = Arc::new(RwLock::new(None));
    let latest_pip = Arc::new(RwLock::new(None));

    let snapshots: Arc<dyn SnapshotProvider> = Arc::new(LiveSnapshotProvider {
        latest_frame: Arc::clone(&latest_frame),
        latest_bounds: Arc::clone(&latest_bounds),
        latest_pip: Arc::clone(&latest_pip),
        tracker: Arc::clone(&tracker_arc),
        pos_osd: Arc::clone(&pos_osd),
        config: Arc::clone(&shared_cfg),
    });

    let tls_server_cfg = if cfg.tls.https_enabled || cfg.tls.rtsps_enabled {
        let mut extra_sans = Vec::new();
        if !bind_ip.is_unspecified() {
            extra_sans.push(bind_ip.to_string());
        }
        Some(build_tls_server_config(
            cfg.tls.cert_path.as_deref(),
            cfg.tls.key_path.as_deref(),
            &extra_sans,
            vec![b"http/1.1".to_vec()],
        )?)
    } else {
        None
    };

    let mut rtsps_port: Option<u16> = None;
    if cfg.tls.rtsps_enabled
        && let Some(tls_cfg) = &tls_server_cfg
    {
        let rtsps_listener =
            TcpListener::bind(SocketAddr::new(bind_ip, cfg.tls.rtsps_port)).await?;
        rtsps_port = Some(rtsps_listener.local_addr()?.port());
        let rtsp_tls_cfg = build_tls_server_config(
            cfg.tls.cert_path.as_deref(),
            cfg.tls.key_path.as_deref(),
            &[],
            Vec::new(),
        )
        .unwrap_or_else(|_| Arc::clone(tls_cfg));
        rtsp_server.spawn_tls_listener(rtsps_listener, rtsp_tls_cfg);
    }

    let backchannel_packets = Arc::new(AtomicU64::new(0));
    if cfg.audio.backchannel_enabled {
        let mut sink_out = AudioOutputSink::new();
        let output_sink = match sink_out.start() {
            Ok(()) => Some(Arc::new(sink_out)),
            Err(err) => {
                warn!(error = %err, "audio backchannel speaker unavailable; decoding packets silently");
                None
            }
        };
        let counter = Arc::clone(&backchannel_packets);
        let bc_sink: BackchannelSink = Arc::new(move |pt, payload| {
            counter.fetch_add(1, Ordering::Relaxed);
            if let Some(out) = &output_sink {
                let pcm = decode_g711(payload, pt);
                out.push_samples_8khz(&pcm);
            }
        });
        rtsp_registry.set_backchannel_sink(Some(bc_sink));
    }

    // Bind combined HTTP/HTTPS listener (Web UI + WS/REST POS OSD + MJPEG Preview + ONVIF Profile S/T).
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

    let https_enabled = cfg.tls.https_enabled;

    let main_frames = Arc::new(AtomicU64::new(0));
    let sub_frames = Arc::new(AtomicU64::new(0));
    let active_encoder = Arc::new(RwLock::new("idle".to_string()));
    let recording_stats = Arc::new(RwLock::new(RecordingStats::default()));
    let srt_stats = Arc::new(RwLock::new(SrtStats {
        enabled: cfg.srt.enabled,
        mode: cfg.srt.mode,
        target: cfg.srt.target.clone(),
        ..Default::default()
    }));
    let (srt_cfg_tx, srt_cfg_rx) = watch::channel(cfg.srt.clone());
    let initial_srt_handle = select_srt_handle(&cfg.srt.source_stream, &main_handle, &sub_handle);
    let (srt_stream_tx, srt_stream_rx) = watch::channel(Some(initial_srt_handle));
    let sub_handle_slot = Arc::new(RwLock::new(sub_handle.clone()));
    let paused = Arc::new(AtomicBool::new(false));
    let config_gen = Arc::new(AtomicU64::new(0));

    let virtual_webcam_slots: Arc<RwLock<Option<VirtualWebcamHandles>>> =
        Arc::new(RwLock::new(None));
    let virtual_webcam_ports: Arc<RwLock<Option<VirtualWebcamPorts>>> = Arc::new(RwLock::new(None));
    let webcam_events = EventBroker::new();
    let (disc_tx, disc_rx) = watch::channel(build_discovery_configs(&cfg, http_port, None));
    let (virtual_webcam_tx, mut virtual_webcam_rx) =
        mpsc::unbounded_channel::<(NvrDeskConfig, Option<oneshot::Sender<()>>)>();

    let mut vw_runtime = VirtualWebcamRuntime::new();
    reconcile_virtual_webcam_servers(
        &cfg,
        http_port,
        &mut vw_runtime,
        &virtual_webcam_slots,
        &virtual_webcam_ports,
        &latest_pip,
        &shared_cfg,
        &webcam_events,
        &disc_tx,
    )
    .await?;

    let endpoints = build_endpoints_with_virtual(
        &cfg,
        &display_ip,
        rtsp_addr.port(),
        rtsps_port,
        http_port,
        https_enabled,
        virtual_webcam_ports
            .read()
            .expect("virtual webcam ports lock")
            .as_ref(),
    );

    let registry_for_players = rtsp_registry.clone();
    let vw_slots_for_players = Arc::clone(&virtual_webcam_slots);
    let active_players_fn = Arc::new(move || {
        let primary = registry_for_players.active_players();
        let cam = vw_slots_for_players
            .read()
            .expect("virtual webcam slots lock")
            .as_ref()
            .map_or(0, |s| s.registry.active_players());
        primary + cam
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
        srt_stats: Arc::clone(&srt_stats),
        srt_cfg_tx,
        srt_stream_tx: srt_stream_tx.clone(),
        main_handle: main_handle.clone(),
        sub_handle_slot: Arc::clone(&sub_handle_slot),
        virtual_webcam_ports: Arc::clone(&virtual_webcam_ports),
        virtual_webcam_tx,
        active_players_fn,
        display_ip,
        rtsp_port: rtsp_addr.port(),
        rtsps_port,
        http_port,
        https_enabled,
        backchannel_packets,
        snapshots: Arc::clone(&snapshots),
        pos_osd: Arc::clone(&pos_osd),
        events: events.clone(),
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
                encoding: cfg.main_stream.codec,
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
                    encoding: cfg.sub_stream.codec,
                    width: sub_w,
                    height: sub_h,
                    fps: cfg.sub_stream.fps,
                    bitrate_kbps: cfg.sub_stream.bitrate_bps / 1000,
                    gov_length: cfg.sub_stream.fps * cfg.sub_stream.idr_interval_secs,
                    rtsp_path: sub_path.clone(),
                    snapshot_slug: "sub".into(),
                });
            }
            if cfg.webcam.uses_channel() {
                let (cam_w, cam_h) = compute_target_dimensions(1280, 720, cfg.webcam.max_width);
                profiles.push(OnvifMediaProfile {
                    token: "WebcamProfile".into(),
                    name: "Webcam".into(),
                    encoder_token: "WebcamEncoder".into(),
                    encoding: VideoCodecKind::H264,
                    width: cam_w,
                    height: cam_h,
                    fps: cfg.webcam.fps,
                    bitrate_kbps: cfg.webcam.bitrate_bps / 1000,
                    gov_length: cfg.webcam.fps * 2,
                    rtsp_path: webcam_path.clone(),
                    snapshot_slug: "webcam".into(),
                });
            }
            let meta = OnvifDeviceMeta {
                device_name: cfg.onvif.device_name.clone(),
                manufacturer: cfg.onvif.manufacturer.clone(),
                model: cfg.onvif.model.clone(),
                firmware_version: env!("CARGO_PKG_VERSION").to_string(),
                serial_number: cfg.onvif.serial_number.clone(),
                hardware_id: "NVRDESK-RUST-V3".to_string(),
                rtsp_port: rtsp_addr.port(),
                rtsps_port,
                http_port,
                https_enabled,
                audio_enabled: cfg.audio.enabled,
                backchannel_enabled: cfg.audio.backchannel_enabled,
            };
            router = router.merge(build_onvif_router_with_events(
                meta,
                profiles,
                onvif_auth,
                snapshots,
                events.clone(),
            ));

            if cfg.onvif.discovery_enabled {
                tokio::spawn(async move {
                    if let Err(err) = run_ws_discovery_watch(disc_rx).await {
                        warn!(error = %err, "WS-Discovery responder stopped");
                    }
                });
            }
        }

        let http_tls = tls_server_cfg.filter(|_| cfg.tls.https_enabled);
        tokio::spawn(async move {
            let addr = listener
                .local_addr()
                .unwrap_or(SocketAddr::new(bind_ip, http_port));
            if let Some(tls_cfg) = http_tls {
                info!(%addr, "HTTPS server (Web UI + ONVIF Profile T) listening");
                serve_https_router(listener, router, tls_cfg).await;
            } else {
                info!(%addr, "HTTP server (Web UI + ONVIF Profile S/T) listening");
                if let Err(err) = axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                {
                    error!(error = %err, "HTTP server exited with error");
                }
            }
        });
    }

    let vw_slots_mgr = Arc::clone(&virtual_webcam_slots);
    let vw_ports_mgr = Arc::clone(&virtual_webcam_ports);
    let vw_pip_mgr = Arc::clone(&latest_pip);
    let vw_cfg_mgr = Arc::clone(&shared_cfg);
    let vw_events_mgr = webcam_events.clone();
    tokio::spawn(async move {
        let mut runtime = vw_runtime;
        let _keep_disc_tx = &disc_tx;
        while let Some((next_cfg, ack)) = virtual_webcam_rx.recv().await {
            if let Err(err) = reconcile_virtual_webcam_servers(
                &next_cfg,
                http_port,
                &mut runtime,
                &vw_slots_mgr,
                &vw_ports_mgr,
                &vw_pip_mgr,
                &vw_cfg_mgr,
                &vw_events_mgr,
                &disc_tx,
            )
            .await
            {
                warn!(error = %err, "failed to reconfigure Virtual ONVIF Webcam server");
            }
            if let Some(tx) = ack {
                let _ = tx.send(());
            }
        }
        runtime.stop(&vw_slots_mgr, &vw_ports_mgr);
    });

    tokio::spawn(async move {
        if let Err(err) = rtsp_server.serve().await {
            error!(error = %err, "RTSP server exited with error");
        }
    });

    for ep in &endpoints {
        info!(endpoint = %ep.label, url = %ep.url, "endpoint ready");
    }

    let _srt_task = spawn_srt_manager(
        srt_cfg_rx,
        srt_stream_rx,
        Arc::clone(&srt_stats),
        Arc::clone(&paused),
        Arc::clone(&shutdown),
    );

    let worker_shutdown = Arc::clone(&shutdown);
    let worker_handle = thread::Builder::new()
        .name("nvrdesk-daemon-worker".into())
        .spawn(move || {
            run_worker(
                shared_cfg,
                config_gen,
                paused,
                rtsp_registry,
                main_handle,
                sub_handle,
                sub_handle_slot,
                srt_stream_tx,
                webcam_handle,
                virtual_webcam_slots,
                tracker_arc,
                pos_osd,
                events,
                webcam_events,
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

async fn serve_https_router(
    listener: TcpListener,
    router: Router,
    tls_config: Arc<rustls::ServerConfig>,
) {
    let acceptor = TlsAcceptor::from(tls_config);
    loop {
        match listener.accept().await {
            Ok((tcp_stream, peer_addr)) => {
                let acceptor = acceptor.clone();
                let router = router.clone();
                tokio::spawn(async move {
                    let Ok(tls_stream) = acceptor.accept(tcp_stream).await else {
                        return;
                    };
                    let service = tower::service_fn(
                        move |mut req: axum::http::Request<hyper::body::Incoming>| {
                            req.extensions_mut().insert(ConnectInfo(peer_addr));
                            router.clone().oneshot(req)
                        },
                    );
                    let io = TokioIo::new(tls_stream);
                    let _ = HttpAutoBuilder::new(TokioExecutor::new())
                        .serve_connection_with_upgrades(io, TowerToHyperService::new(service))
                        .await;
                });
            }
            Err(err) => {
                error!(error = %err, "HTTPS accept error");
                break;
            }
        }
    }
}

fn build_screen_source(
    source: &SourceConfig,
    ffmpeg_path: &str,
    fps: u32,
) -> Box<dyn CaptureSource> {
    match source.kind {
        CaptureSourceKind::Screen => Box::new(ScreenCaptureSource::new(
            source.display,
            fps,
            source.show_cursor,
        )),
        CaptureSourceKind::TestPattern => {
            let w = source.width.unwrap_or(1280);
            let h = source.height.unwrap_or(720);
            Box::new(TestPatternSource::new(w, h, fps))
        }
        CaptureSourceKind::Mp4
        | CaptureSourceKind::Mjpeg
        | CaptureSourceKind::Rtmp
        | CaptureSourceKind::RtspRelay
        | CaptureSourceKind::FfmpegUri => Box::new(FfmpegPullSource::new(
            source.kind,
            source.uri.clone().unwrap_or_default(),
            ffmpeg_path.to_string(),
            source.loop_input,
            source.width.unwrap_or(1280),
            source.height.unwrap_or(720),
            fps,
        )),
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
fn reconcile_streams(
    cfg: &NvrDeskConfig,
    registry: &RtspRegistry,
    main_pipe: &mut StreamPipeline,
    sub_pipe: &mut Option<StreamPipeline>,
    active_sub_path: &mut Option<String>,
    sub_handle_slot: &Arc<RwLock<Option<RtspStreamHandle>>>,
    srt_stream_tx: &watch::Sender<Option<RtspStreamHandle>>,
    webcam_pipe: &mut Option<StreamPipeline>,
    active_webcam_path: &mut Option<String>,
    virtual_webcam_slots: &Arc<RwLock<Option<VirtualWebcamHandles>>>,
    webcam_main_pipe: &mut Option<StreamPipeline>,
    webcam_sub_pipe: &mut Option<StreamPipeline>,
    sub_frames: &Arc<AtomicU64>,
) {
    let audio_codec = if cfg.audio.enabled {
        Some(cfg.audio.codec)
    } else {
        None
    };

    let main_path = normalize_stream_path(&cfg.main_stream.path);
    let _ = registry.add_or_update_stream(RtspStreamSpec {
        path: main_path.clone(),
        aliases: if main_path == "/stream" {
            Vec::new()
        } else {
            vec!["/stream".to_string()]
        },
        name: format!("{} (Main)", cfg.onvif.device_name),
        video_codec: cfg.main_stream.codec,
        audio_codec,
        backchannel: cfg.audio.backchannel_enabled,
    });
    main_pipe.reconfigure(
        cfg.main_stream.to_stream_config(cfg.source.show_cursor),
        cfg.encoder.clone(),
    );

    let norm_sub_path = normalize_stream_path(&cfg.sub_stream.path);
    if cfg.sub_stream.enabled {
        if active_sub_path.as_deref() != Some(&norm_sub_path)
            && let Some(old_path) = active_sub_path.take()
        {
            registry.remove_stream(&old_path);
            *sub_pipe = None;
        }
        let handle = registry.add_or_update_stream(RtspStreamSpec {
            path: norm_sub_path.clone(),
            aliases: Vec::new(),
            name: format!("{} (Sub)", cfg.onvif.device_name),
            video_codec: cfg.sub_stream.codec,
            audio_codec,
            backchannel: false,
        });
        match sub_pipe.as_mut() {
            Some(sub) => sub.reconfigure(
                cfg.sub_stream.to_stream_config(cfg.source.show_cursor),
                cfg.encoder.clone(),
            ),
            None => {
                *sub_pipe = Some(StreamPipeline::new(
                    handle,
                    cfg.sub_stream.to_stream_config(cfg.source.show_cursor),
                    cfg.encoder.clone(),
                    Arc::clone(sub_frames),
                ));
            }
        }
        *active_sub_path = Some(norm_sub_path);
        *sub_handle_slot.write().expect("sub handle lock") =
            sub_pipe.as_ref().map(|p| p.handle.clone());
    } else {
        if let Some(old_path) = active_sub_path.take() {
            registry.remove_stream(&old_path);
        }
        *sub_pipe = None;
        *sub_handle_slot.write().expect("sub handle lock") = None;
    }

    let next_srt_handle = select_srt_handle(
        &cfg.srt.source_stream,
        &main_pipe.handle,
        &sub_handle_slot.read().expect("sub handle lock"),
    );
    let _ = srt_stream_tx.send(Some(next_srt_handle));

    let norm_cam_path = normalize_stream_path(&cfg.webcam.channel_path);
    if cfg.webcam.uses_channel() {
        if active_webcam_path.as_deref() != Some(&norm_cam_path)
            && let Some(old_path) = active_webcam_path.take()
        {
            registry.remove_stream(&old_path);
            *webcam_pipe = None;
        }
        let cam_audio = (cfg.webcam.audio_enabled || cfg.audio.enabled).then_some(cfg.audio.codec);
        let handle = registry.add_or_update_stream(RtspStreamSpec {
            path: norm_cam_path.clone(),
            aliases: Vec::new(),
            name: format!("{} (Webcam)", cfg.onvif.device_name),
            video_codec: VideoCodecKind::H264,
            audio_codec: cam_audio,
            backchannel: false,
        });
        let cam_cfg = webcam_main_stream_config(cfg);
        match webcam_pipe.as_mut() {
            Some(cam) => cam.reconfigure(cam_cfg, cfg.encoder.clone()),
            None => {
                *webcam_pipe = Some(StreamPipeline::new(
                    handle,
                    cam_cfg,
                    cfg.encoder.clone(),
                    Arc::new(AtomicU64::new(0)),
                ));
                info!(path = %norm_cam_path, "registered dedicated webcam RTSP channel");
            }
        }
        *active_webcam_path = Some(norm_cam_path);
    } else {
        if let Some(old_path) = active_webcam_path.take() {
            registry.remove_stream(&old_path);
            info!(path = %old_path, "removed dedicated webcam RTSP channel");
        }
        *webcam_pipe = None;
    }

    if cfg.webcam.uses_virtual_onvif() {
        let slots = virtual_webcam_slots
            .read()
            .expect("virtual webcam slots lock")
            .clone();
        if let Some(s) = slots {
            let main_cfg = webcam_main_stream_config(cfg);
            let sub_cfg = webcam_sub_stream_config(cfg);
            *webcam_main_pipe = Some(StreamPipeline::new(
                s.main_handle,
                main_cfg,
                cfg.encoder.clone(),
                Arc::new(AtomicU64::new(0)),
            ));
            *webcam_sub_pipe = Some(StreamPipeline::new(
                s.sub_handle,
                sub_cfg,
                cfg.encoder.clone(),
                Arc::new(AtomicU64::new(0)),
            ));
        } else {
            *webcam_main_pipe = None;
            *webcam_sub_pipe = None;
        }
    } else {
        *webcam_main_pipe = None;
        *webcam_sub_pipe = None;
    }
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    shared_cfg: Arc<RwLock<NvrDeskConfig>>,
    config_gen: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
    rtsp_registry: RtspRegistry,
    main_handle: RtspStreamHandle,
    sub_handle: Option<RtspStreamHandle>,
    sub_handle_slot: Arc<RwLock<Option<RtspStreamHandle>>>,
    srt_stream_tx: watch::Sender<Option<RtspStreamHandle>>,
    webcam_handle: Option<RtspStreamHandle>,
    virtual_webcam_slots: Arc<RwLock<Option<VirtualWebcamHandles>>>,
    tracker_arc: Arc<RwLock<InputTracker>>,
    pos_osd: Arc<PosOsdStore>,
    events: EventBroker,
    webcam_events: EventBroker,
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
            Arc::clone(&sub_frames),
        )
    });
    let mut active_sub_path = init_cfg
        .sub_stream
        .enabled
        .then(|| normalize_stream_path(&init_cfg.sub_stream.path));

    let mut webcam_pipe = webcam_handle.map(|h| {
        StreamPipeline::new(
            h,
            webcam_main_stream_config(&init_cfg),
            init_cfg.encoder.clone(),
            Arc::new(AtomicU64::new(0)),
        )
    });
    let mut active_webcam_path = init_cfg
        .webcam
        .uses_channel()
        .then(|| normalize_stream_path(&init_cfg.webcam.channel_path));

    let (mut webcam_main_pipe, mut webcam_sub_pipe) = if init_cfg.webcam.uses_virtual_onvif() {
        let slots = virtual_webcam_slots
            .read()
            .expect("virtual webcam slots lock")
            .clone();
        if let Some(s) = slots {
            (
                Some(StreamPipeline::new(
                    s.main_handle,
                    webcam_main_stream_config(&init_cfg),
                    init_cfg.encoder.clone(),
                    Arc::new(AtomicU64::new(0)),
                )),
                Some(StreamPipeline::new(
                    s.sub_handle,
                    webcam_sub_stream_config(&init_cfg),
                    init_cfg.encoder.clone(),
                    Arc::new(AtomicU64::new(0)),
                )),
            )
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    let mut recorder = RingRecorder::new(init_cfg.recording.clone()).unwrap_or_else(|e| {
        warn!(error = %e, "failed to initialize ring recorder; disabling recording");
        RingRecorder::new(Default::default()).expect("default disabled recorder")
    });

    while !shutdown.load(Ordering::Acquire) {
        let cfg = shared_cfg.read().expect("config lock").clone();
        let cur_gen = config_gen.load(Ordering::Acquire);
        if cur_gen != last_gen {
            last_gen = cur_gen;
            reconcile_streams(
                &cfg,
                &rtsp_registry,
                &mut main_pipe,
                &mut sub_pipe,
                &mut active_sub_path,
                &sub_handle_slot,
                &srt_stream_tx,
                &mut webcam_pipe,
                &mut active_webcam_path,
                &virtual_webcam_slots,
                &mut webcam_main_pipe,
                &mut webcam_sub_pipe,
                &sub_frames,
            );
            if !cfg.webcam.enabled {
                *latest_pip.write().expect("latest_pip lock") = None;
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
            .is_some_and(|p| p.has_demand(cfg.always_on))
            || webcam_main_pipe
                .as_ref()
                .is_some_and(|p| p.has_demand(cfg.always_on))
            || webcam_sub_pipe
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
            .max(
                if cfg.webcam.uses_channel() || cfg.webcam.uses_virtual_onvif() {
                    cfg.webcam.fps
                } else {
                    1
                },
            )
            .max(1);

        let mut screen_src = build_screen_source(&cfg.source, &cfg.encoder.ffmpeg_path, base_fps);
        let screen_started = if need_desktop {
            if let Err(err) = screen_src.start() {
                error!(error = %err, source = %screen_src.name(), "failed to start screen source");
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            true
        } else {
            false
        };
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

        let cam_audio_active = (cfg.webcam.uses_channel() || cfg.webcam.uses_virtual_onvif())
            && (cfg.webcam.audio_enabled || cfg.audio.enabled);
        let effective_cam_mic = cfg
            .webcam
            .audio_device_name
            .clone()
            .or_else(|| cfg.audio.device_name.clone());
        let reuse_desktop_audio = audio_src.is_some() && effective_cam_mic == cfg.audio.device_name;
        let mut webcam_audio_src = if cam_audio_active && !reuse_desktop_audio {
            let mut mic = AudioCaptureSource::new(effective_cam_mic);
            if let Err(err) = mic.start() {
                warn!(error = %err, "failed to start webcam microphone capture");
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
            last_gen,
            &paused,
            screen_src.as_mut(),
            screen_started,
            webcam_src.as_mut(),
            audio_src.as_mut(),
            webcam_audio_src.as_mut(),
            reuse_desktop_audio,
            &mut main_pipe,
            sub_pipe.as_mut(),
            webcam_pipe.as_mut(),
            webcam_main_pipe.as_mut(),
            webcam_sub_pipe.as_mut(),
            &tracker_arc,
            &pos_osd,
            &events,
            &webcam_events,
            &latest_frame,
            &latest_pip,
            &active_encoder,
            &mut recorder,
            &recording_stats,
            &shutdown,
        );

        if screen_started {
            screen_src.stop();
            *latest_frame.write().expect("latest_frame lock") = None;
        }
        if let Some(mut cam) = webcam_src {
            cam.stop();
        }
        if let Some(mut mic) = audio_src {
            mic.stop();
        }
        if let Some(mut mic) = webcam_audio_src {
            mic.stop();
        }
        main_pipe.encoder = None;
        if let Some(sub) = sub_pipe.as_mut() {
            sub.encoder = None;
        }
        if let Some(cam) = webcam_pipe.as_mut() {
            cam.encoder = None;
        }
        if let Some(cam) = webcam_main_pipe.as_mut() {
            cam.encoder = None;
        }
        if let Some(cam) = webcam_sub_pipe.as_mut() {
            cam.encoder = None;
        }
        recorder.flush_and_close();
    }
}

fn publish_webcam_audio_packet(
    pkt: nvrdesk_core::EncodedAudioFrame,
    webcam_pipe: Option<&StreamPipeline>,
    webcam_main_pipe: Option<&StreamPipeline>,
    webcam_sub_pipe: Option<&StreamPipeline>,
) {
    if let Some(cam_p) = webcam_pipe {
        cam_p.handle.publish_audio(pkt.clone());
    }
    if let Some(cam_m) = webcam_main_pipe {
        cam_m.handle.publish_audio(pkt.clone());
    }
    if let Some(cam_s) = webcam_sub_pipe {
        cam_s.handle.publish_audio(pkt);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_active_loop(
    shared_cfg: &Arc<RwLock<NvrDeskConfig>>,
    config_gen: &Arc<AtomicU64>,
    last_gen: u64,
    paused: &Arc<AtomicBool>,
    screen_src: &mut dyn CaptureSource,
    screen_started: bool,
    mut webcam_src: Option<&mut WebcamCaptureSource>,
    mut audio_src: Option<&mut AudioCaptureSource>,
    mut webcam_audio_src: Option<&mut AudioCaptureSource>,
    reuse_desktop_audio: bool,
    main_pipe: &mut StreamPipeline,
    mut sub_pipe: Option<&mut StreamPipeline>,
    mut webcam_pipe: Option<&mut StreamPipeline>,
    mut webcam_main_pipe: Option<&mut StreamPipeline>,
    mut webcam_sub_pipe: Option<&mut StreamPipeline>,
    tracker_arc: &Arc<RwLock<InputTracker>>,
    pos_osd: &PosOsdStore,
    events: &EventBroker,
    webcam_events: &EventBroker,
    latest_frame: &Arc<RwLock<Option<RawVideoFrame>>>,
    latest_pip: &Arc<RwLock<Option<RawVideoFrame>>>,
    active_encoder: &Arc<RwLock<String>>,
    recorder: &mut RingRecorder,
    recording_stats: &Arc<RwLock<RecordingStats>>,
    shutdown: &AtomicBool,
) {
    let cfg = shared_cfg.read().expect("config lock").clone();
    let base_fps = cfg
        .main_stream
        .fps
        .max(if cfg.sub_stream.enabled {
            cfg.sub_stream.fps
        } else {
            1
        })
        .max(
            if cfg.webcam.uses_channel() || cfg.webcam.uses_virtual_onvif() {
                cfg.webcam.fps
            } else {
                1
            },
        )
        .max(1);
    let tick_interval = Duration::from_secs_f64(1.0 / f64::from(base_fps));
    let motion_interval = Duration::from_millis(cfg.motion.min_interval_ms.max(250));

    let session_start = Instant::now();
    let mut next_tick = Instant::now();
    let mut last_frame: Option<RawVideoFrame> = None;
    let mut last_cam_frame: Option<RawVideoFrame> = None;
    let mut last_demand_at = Instant::now();
    let mut audio_pts: u32 = 0;
    let mut webcam_audio_pts: u32 = 0;
    let bounds = screen_src.bounds();
    let mut screen_motion = MotionDetector::new();
    let mut webcam_motion = MotionDetector::new();

    while !shutdown.load(Ordering::Acquire) && !paused.load(Ordering::Acquire) {
        if config_gen.load(Ordering::Acquire) != last_gen {
            // Break out so run_worker reconciles RTSP streams, pipelines, webcam, audio, and display.
            break;
        }

        let now = Instant::now();
        let main_active = main_pipe.has_demand(cfg.always_on) || cfg.recording.enabled;
        let sub_active = sub_pipe
            .as_ref()
            .is_some_and(|p| p.has_demand(cfg.always_on));
        let cam_chan_active = webcam_pipe
            .as_ref()
            .is_some_and(|p| p.has_demand(cfg.always_on));
        let cam_virt_main_active = webcam_main_pipe
            .as_ref()
            .is_some_and(|p| p.has_demand(cfg.always_on));
        let cam_virt_sub_active = webcam_sub_pipe
            .as_ref()
            .is_some_and(|p| p.has_demand(cfg.always_on));
        let cam_active = cam_chan_active || cam_virt_main_active || cam_virt_sub_active;

        if main_active || sub_active || cam_active {
            last_demand_at = now;
        } else if now.saturating_duration_since(last_demand_at) >= IDLE_GRACE_PERIOD {
            info!("no active demand after grace period; pausing (zero-idle)");
            break;
        }

        if main_active || sub_active {
            if !screen_started {
                // Screen demand appeared while only webcam was running; restart loop with screen_src started.
                break;
            }
            match screen_src.try_next_frame(tick_interval) {
                Ok(Some(raw)) => {
                    if cfg.motion.enabled
                        && let Some(ev) = screen_motion.update(
                            &raw,
                            cfg.motion.sensitivity,
                            motion_interval,
                            Instant::now(),
                        )
                    {
                        events.publish_motion("VideoSource_1", ev.active);
                    }
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
            if cfg.motion.enabled
                && cfg.motion.webcam_enabled
                && let Some(ev) = webcam_motion.update(
                    &cam_raw,
                    cfg.motion.sensitivity,
                    motion_interval,
                    Instant::now(),
                )
            {
                events.publish_motion("WebcamSource_1", ev.active);
                webcam_events.publish_motion("VideoSource_1", ev.active);
            }
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
                if reuse_desktop_audio {
                    publish_webcam_audio_packet(
                        pkt,
                        webcam_pipe.as_deref(),
                        webcam_main_pipe.as_deref(),
                        webcam_sub_pipe.as_deref(),
                    );
                }
            }
        }

        if let Some(cam_mic) = webcam_audio_src.as_deref_mut() {
            while let Ok(Some(samples)) = cam_mic.try_next_chunk(Duration::ZERO) {
                let pkt = encode_g711(&samples, cfg.audio.codec, webcam_audio_pts);
                webcam_audio_pts = webcam_audio_pts.wrapping_add(samples.len() as u32);
                publish_webcam_audio_packet(
                    pkt,
                    webcam_pipe.as_deref(),
                    webcam_main_pipe.as_deref(),
                    webcam_sub_pipe.as_deref(),
                );
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

            let pip = if cfg.webcam.uses_pip() {
                last_cam_frame
                    .as_ref()
                    .map(|f| (f, cfg.webcam.pip_position, cfg.webcam.pip_scale))
            } else {
                None
            };
            let pos_snap = pos_osd.snapshot(&cfg.pos, now);

            if main_active {
                if let Some(encoded) = tick_stream_pipeline(
                    main_pipe,
                    raw,
                    overlay.as_ref(),
                    pip,
                    pos_snap.as_ref(),
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
                    if let Some(encoded) = tick_stream_pipeline(
                        sub,
                        raw,
                        overlay.as_ref(),
                        pip,
                        pos_snap.as_ref(),
                        now,
                        None,
                    ) {
                        sub.handle.publish_frame(encoded);
                    }
                } else {
                    sub.encoder = None;
                }
            }
        }

        if let Some(cam_pipe) = webcam_pipe.as_deref_mut() {
            if cam_chan_active {
                if let Some(cam_raw) = &last_cam_frame
                    && let Some(encoded) = tick_stream_pipeline(
                        cam_pipe,
                        cam_raw,
                        None,
                        None,
                        None,
                        now,
                        (!main_active).then_some(active_encoder),
                    )
                {
                    cam_pipe.handle.publish_frame(encoded);
                }
            } else {
                cam_pipe.encoder = None;
            }
        }

        if let Some(cam_main) = webcam_main_pipe.as_deref_mut() {
            if cam_virt_main_active {
                if let Some(cam_raw) = &last_cam_frame
                    && let Some(encoded) = tick_stream_pipeline(
                        cam_main,
                        cam_raw,
                        None,
                        None,
                        None,
                        now,
                        (!main_active && !cam_chan_active).then_some(active_encoder),
                    )
                {
                    cam_main.handle.publish_frame(encoded);
                }
            } else {
                cam_main.encoder = None;
            }
        }

        if let Some(cam_sub) = webcam_sub_pipe.as_deref_mut() {
            if cam_virt_sub_active {
                if let Some(cam_raw) = &last_cam_frame
                    && let Some(encoded) = tick_stream_pipeline(
                        cam_sub,
                        cam_raw,
                        None,
                        None,
                        None,
                        now,
                        (!main_active && !cam_chan_active && !cam_virt_main_active)
                            .then_some(active_encoder),
                    )
                {
                    cam_sub.handle.publish_frame(encoded);
                }
            } else {
                cam_sub.encoder = None;
            }
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
    pos_snap: Option<&PosOverlaySnapshot>,
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
        .process_full_with_pos(raw, pipe.stream_cfg.max_width, overlay, pip, pos_snap)
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
