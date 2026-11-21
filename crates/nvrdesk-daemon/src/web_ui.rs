//! Embedded Web UI, REST API (`/api/v1/*`), and live MJPEG preview router.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    body::Body,
    extract::{
        Path, State,
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use bytes::Bytes;
use futures_core::Stream;
use nvrdesk_capture::enumerate_devices;
use nvrdesk_core::{NvrDeskConfig, PosOsdLevel, PosOsdLine, WebcamMode};
use nvrdesk_onvif::{EventBroker, OnvifEventRecord, SnapshotProvider};
use nvrdesk_rtsp::normalize_stream_path;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::{pos_osd::PosOsdStore, recorder::RecordingStats};

#[derive(RustEmbed)]
#[folder = "../../web-ui"]
struct WebAssets;

/// Labeled endpoint URL shown in the Web UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointInfo {
    pub label: String,
    pub url: String,
}

/// Live telemetry payload returned by `GET /api/v1/status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub paused: bool,
    pub active_players: usize,
    pub main_frames: u64,
    pub sub_frames: u64,
    pub active_encoder: String,
    pub onvif_enabled: bool,
    pub https_enabled: bool,
    pub rtsps_port: Option<u16>,
    pub backchannel_packets: u64,
    pub recording: RecordingStats,
    pub endpoints: Vec<EndpointInfo>,
    pub active_osd_lines: Vec<PosOsdLine>,
    pub recent_events: Vec<OnvifEventRecord>,
}

/// Shared runtime state backing the Web UI, REST API, and worker coordination.
#[derive(Clone)]
pub struct DaemonSharedState {
    pub config: Arc<RwLock<NvrDeskConfig>>,
    pub config_path: Option<PathBuf>,
    pub config_gen: Arc<AtomicU64>,
    pub paused: Arc<AtomicBool>,
    pub main_frames: Arc<AtomicU64>,
    pub sub_frames: Arc<AtomicU64>,
    pub active_encoder: Arc<RwLock<String>>,
    pub recording_stats: Arc<RwLock<RecordingStats>>,
    pub active_players_fn: Arc<dyn Fn() -> usize + Send + Sync>,
    pub display_ip: String,
    pub rtsp_port: u16,
    pub rtsps_port: Option<u16>,
    pub http_port: u16,
    pub https_enabled: bool,
    pub backchannel_packets: Arc<AtomicU64>,
    pub snapshots: Arc<dyn SnapshotProvider>,
    pub pos_osd: Arc<PosOsdStore>,
    pub events: EventBroker,
}

/// Builds the current list of advertised RTSP/RTSPS/ONVIF/Web UI endpoints from live configuration.
#[must_use]
pub fn build_endpoints(
    cfg: &NvrDeskConfig,
    display_ip: &str,
    rtsp_port: u16,
    rtsps_port: Option<u16>,
    http_port: u16,
    https_enabled: bool,
) -> Vec<EndpointInfo> {
    let main_path = normalize_stream_path(&cfg.main_stream.path);
    let sub_path = normalize_stream_path(&cfg.sub_stream.path);
    let webcam_path = normalize_stream_path(&cfg.webcam.channel_path);
    let http_scheme = if https_enabled { "https" } else { "http" };

    let mut endpoints = vec![EndpointInfo {
        label: "MainStream RTSP".into(),
        url: format!("rtsp://{display_ip}:{rtsp_port}{main_path}"),
    }];
    if let Some(tls_port) = rtsps_port {
        endpoints.push(EndpointInfo {
            label: "MainStream RTSPS (TLS)".into(),
            url: format!("rtsps://{display_ip}:{tls_port}{main_path}"),
        });
    }
    if cfg.sub_stream.enabled {
        endpoints.push(EndpointInfo {
            label: "SubStream RTSP".into(),
            url: format!("rtsp://{display_ip}:{rtsp_port}{sub_path}"),
        });
    }
    if cfg.webcam.enabled && cfg.webcam.mode == WebcamMode::Channel {
        endpoints.push(EndpointInfo {
            label: "Webcam RTSP".into(),
            url: format!("rtsp://{display_ip}:{rtsp_port}{webcam_path}"),
        });
    }
    if cfg.onvif.enabled {
        endpoints.push(EndpointInfo {
            label: "ONVIF Device Service".into(),
            url: format!("{http_scheme}://{display_ip}:{http_port}/onvif/device_service"),
        });
        endpoints.push(EndpointInfo {
            label: "ONVIF Media2 (Profile T)".into(),
            url: format!("{http_scheme}://{display_ip}:{http_port}/onvif/media2_service"),
        });
        endpoints.push(EndpointInfo {
            label: "ONVIF Events (PullPoint)".into(),
            url: format!("{http_scheme}://{display_ip}:{http_port}/onvif/events_service"),
        });
        endpoints.push(EndpointInfo {
            label: "Snapshot Main".into(),
            url: format!("{http_scheme}://{display_ip}:{http_port}/snapshot/main"),
        });
    }
    if cfg.daemon.web_ui_enabled {
        endpoints.push(EndpointInfo {
            label: "Web Control Panel".into(),
            url: format!("{http_scheme}://{display_ip}:{http_port}/"),
        });
    }
    endpoints
}

/// Builds the Axum router for the embedded Web UI, REST/WebSocket POS OSD API, and MJPEG preview.
pub fn build_web_ui_router(state: DaemonSharedState) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route("/assets/{*path}", get(serve_asset))
        .route("/api/v1/status", get(get_status))
        .route("/api/v1/devices", get(get_devices))
        .route("/api/v1/config", get(get_config).put(put_config))
        .route("/api/v1/control/pause", post(post_pause))
        .route("/api/v1/control/resume", post(post_resume))
        .route(
            "/api/v1/osd/text",
            get(get_osd_text).post(post_osd_text).delete(delete_all_osd),
        )
        .route("/api/v1/osd/text/{slot}", delete(delete_slot_osd))
        .route("/api/v1/ws/osd", get(ws_osd_handler))
        .route("/api/v1/events", get(get_events))
        .route("/preview/{profile}", get(serve_mjpeg_preview))
        .with_state(state)
}

async fn serve_index() -> Response {
    serve_embedded("index.html")
}

async fn serve_asset(Path(path): Path<String>) -> Response {
    serve_embedded(&path)
}

fn serve_embedded(path: &str) -> Response {
    let clean = path.trim_start_matches('/');
    let Some(file) = WebAssets::get(clean) else {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    };
    let content_type = if clean.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if clean.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if clean.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else {
        "application/octet-stream"
    };

    let mut resp = (StatusCode::OK, file.data.into_owned()).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

async fn get_status(State(state): State<DaemonSharedState>) -> Json<DaemonStatus> {
    let cfg = state.config.read().expect("config lock");
    let status = DaemonStatus {
        paused: state.paused.load(Ordering::Acquire),
        active_players: (state.active_players_fn)(),
        main_frames: state.main_frames.load(Ordering::Relaxed),
        sub_frames: state.sub_frames.load(Ordering::Relaxed),
        active_encoder: state.active_encoder.read().expect("encoder lock").clone(),
        onvif_enabled: cfg.onvif.enabled,
        https_enabled: state.https_enabled,
        rtsps_port: state.rtsps_port,
        backchannel_packets: state.backchannel_packets.load(Ordering::Relaxed),
        recording: state.recording_stats.read().expect("rec lock").clone(),
        endpoints: build_endpoints(
            &cfg,
            &state.display_ip,
            state.rtsp_port,
            state.rtsps_port,
            state.http_port,
            state.https_enabled,
        ),
        active_osd_lines: state.pos_osd.active_lines(Instant::now()),
        recent_events: state.events.recent(10),
    };
    Json(status)
}

#[derive(Debug, Deserialize)]
struct PosOsdRequest {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    slot: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    ttl_ms: Option<u64>,
    #[serde(default)]
    level: PosOsdLevel,
    #[serde(default)]
    emit_event: bool,
}

fn apply_pos_osd_command(state: &DaemonSharedState, payload: PosOsdRequest) {
    if payload.action.as_deref() == Some("clear") {
        if let Some(slot) = payload.slot.as_deref() {
            state.pos_osd.clear_slot(slot);
        } else {
            state.pos_osd.clear_all();
        }
        return;
    }

    let slot = payload.slot.unwrap_or_else(|| "ticket".to_string());
    let default_ttl = state.config.read().expect("config lock").pos.default_ttl_ms;
    let ttl_ms = payload.ttl_ms.unwrap_or(default_ttl);
    let level_label = match payload.level {
        PosOsdLevel::Info => "info",
        PosOsdLevel::Warning => "warning",
        PosOsdLevel::Alert => "alert",
    };

    if payload.emit_event && !payload.text.trim().is_empty() {
        state
            .events
            .publish_pos_event(&slot, payload.text.trim(), level_label);
    }
    state
        .pos_osd
        .set_line(slot, payload.text, payload.level, ttl_ms);
}

async fn get_osd_text(State(state): State<DaemonSharedState>) -> Json<Vec<PosOsdLine>> {
    Json(state.pos_osd.active_lines(Instant::now()))
}

async fn post_osd_text(
    State(state): State<DaemonSharedState>,
    Json(payload): Json<PosOsdRequest>,
) -> Json<Vec<PosOsdLine>> {
    apply_pos_osd_command(&state, payload);
    Json(state.pos_osd.active_lines(Instant::now()))
}

async fn delete_all_osd(State(state): State<DaemonSharedState>) -> StatusCode {
    state.pos_osd.clear_all();
    StatusCode::NO_CONTENT
}

async fn delete_slot_osd(
    State(state): State<DaemonSharedState>,
    Path(slot): Path<String>,
) -> StatusCode {
    state.pos_osd.clear_slot(&slot);
    StatusCode::NO_CONTENT
}

async fn ws_osd_handler(
    ws: WebSocketUpgrade,
    State(state): State<DaemonSharedState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_osd(socket, state))
}

async fn handle_ws_osd(mut socket: WebSocket, state: DaemonSharedState) {
    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            WsMessage::Text(txt) => {
                if let Ok(req) = serde_json::from_str::<PosOsdRequest>(&txt) {
                    apply_pos_osd_command(&state, req);
                    let active = state.pos_osd.active_lines(Instant::now());
                    if let Ok(reply) = serde_json::to_string(&active) {
                        let _ = socket.send(WsMessage::Text(reply.into())).await;
                    }
                }
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }
}

async fn get_events(State(state): State<DaemonSharedState>) -> Json<Vec<OnvifEventRecord>> {
    Json(state.events.recent(25))
}

async fn get_devices() -> Response {
    match tokio::task::spawn_blocking(enumerate_devices).await {
        Ok(inv) => Json(inv).into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn get_config(State(state): State<DaemonSharedState>) -> Json<NvrDeskConfig> {
    let cfg = state.config.read().expect("config lock").clone();
    Json(cfg)
}

async fn put_config(
    State(state): State<DaemonSharedState>,
    Json(new_cfg): Json<NvrDeskConfig>,
) -> Response {
    if let Some(path) = &state.config_path
        && let Err(err) = new_cfg.save_toml_file(path)
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response();
    }
    *state.config.write().expect("config lock") = new_cfg.clone();
    state.config_gen.fetch_add(1, Ordering::AcqRel);
    Json(new_cfg).into_response()
}

async fn post_pause(State(state): State<DaemonSharedState>) -> StatusCode {
    state.paused.store(true, Ordering::Release);
    StatusCode::OK
}

async fn post_resume(State(state): State<DaemonSharedState>) -> StatusCode {
    state.paused.store(false, Ordering::Release);
    StatusCode::OK
}

struct MjpegChannelStream {
    rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
}

impl Stream for MjpegChannelStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

async fn serve_mjpeg_preview(
    State(state): State<DaemonSharedState>,
    Path(profile_raw): Path<String>,
) -> Response {
    let profile = profile_raw
        .trim_end_matches(".mjpeg")
        .trim_end_matches(".jpg")
        .to_string();

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    let snapshots = Arc::clone(&state.snapshots);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(200));
        loop {
            interval.tick().await;
            let snaps = Arc::clone(&snapshots);
            let prof = profile.clone();
            let res = tokio::task::spawn_blocking(move || snaps.capture_jpeg(&prof)).await;
            let Ok(Ok(jpeg)) = res else {
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            };
            let header_part = format!(
                "--nvrdeskframe\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                jpeg.len()
            );
            let mut chunk = Vec::with_capacity(header_part.len() + jpeg.len() + 2);
            chunk.extend_from_slice(header_part.as_bytes());
            chunk.extend_from_slice(&jpeg);
            chunk.extend_from_slice(b"\r\n");

            if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                break;
            }
        }
    });

    let body = Body::from_stream(MjpegChannelStream { rx });
    let mut resp = Response::new(body);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("multipart/x-mixed-replace; boundary=nvrdeskframe"),
    );
    resp
}
