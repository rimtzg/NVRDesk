//! Embedded Web UI, REST API (`/api/v1/*`), and live MJPEG preview router.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use futures_core::Stream;
use nvrdesk_capture::enumerate_devices;
use nvrdesk_core::NvrDeskConfig;
use nvrdesk_onvif::SnapshotProvider;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::recorder::RecordingStats;

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
    pub recording: RecordingStats,
    pub endpoints: Vec<EndpointInfo>,
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
    pub endpoints: Arc<Vec<EndpointInfo>>,
    pub snapshots: Arc<dyn SnapshotProvider>,
}

/// Builds the Axum router for the embedded Web UI, REST API, and MJPEG preview.
pub fn build_web_ui_router(state: DaemonSharedState) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route("/assets/{*path}", get(serve_asset))
        .route("/api/v1/status", get(get_status))
        .route("/api/v1/devices", get(get_devices))
        .route("/api/v1/config", get(get_config).put(put_config))
        .route("/api/v1/control/pause", post(post_pause))
        .route("/api/v1/control/resume", post(post_resume))
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
        recording: state.recording_stats.read().expect("rec lock").clone(),
        endpoints: (*state.endpoints).clone(),
    };
    Json(status)
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
