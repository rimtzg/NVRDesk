//! ONVIF Profile S HTTP/SOAP server (Device, Media, and HTTP JPEG Snapshot endpoint).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock};

use axum::{
    Router,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use nvrdesk_core::NvrDeskError;
use tokio::net::TcpListener;
use tracing::{debug, info};

use crate::{
    soap::{self, OnvifDeviceMeta, OnvifMediaProfile},
    ws_security::OnvifAuthConfig,
};

/// Callback trait allowing the ONVIF HTTP server to fetch fresh JPEG snapshots on demand.
pub trait SnapshotProvider: Send + Sync {
    /// Captures and encodes a JPEG snapshot for the requested profile slug (`"main"` or `"sub"`).
    fn capture_jpeg(&self, profile_slug: &str) -> Result<Vec<u8>, NvrDeskError>;
}

#[derive(Clone)]
struct AppState {
    meta: OnvifDeviceMeta,
    profiles: Arc<RwLock<Vec<OnvifMediaProfile>>>,
    auth: Option<OnvifAuthConfig>,
    snapshots: Arc<dyn SnapshotProvider>,
}

/// ONVIF Profile S HTTP server (`device_service`, `media_service`, `/snapshot/{profile}`).
pub struct OnvifServer {
    listener: TcpListener,
    app: Router,
    local_addr: SocketAddr,
}

/// Builds an Axum `Router` exposing ONVIF SOAP (`/onvif/*`) and JPEG snapshots (`/snapshot/{profile}`).
pub fn build_onvif_router(
    meta: OnvifDeviceMeta,
    profiles: Vec<OnvifMediaProfile>,
    auth: Option<OnvifAuthConfig>,
    snapshots: Arc<dyn SnapshotProvider>,
) -> Router {
    let state = AppState {
        meta,
        profiles: Arc::new(RwLock::new(profiles)),
        auth,
        snapshots,
    };

    Router::new()
        .route("/onvif/device_service", post(handle_soap))
        .route("/onvif/media_service", post(handle_soap))
        .route("/onvif/Media", post(handle_soap))
        .route("/snapshot/{profile}", get(handle_snapshot))
        .with_state(state)
}

impl OnvifServer {
    /// Binds the ONVIF HTTP server on `bind_addr`.
    pub async fn bind(
        bind_addr: SocketAddr,
        meta: OnvifDeviceMeta,
        profiles: Vec<OnvifMediaProfile>,
        auth: Option<OnvifAuthConfig>,
        snapshots: Arc<dyn SnapshotProvider>,
    ) -> Result<Self, NvrDeskError> {
        let listener = TcpListener::bind(bind_addr).await?;
        let local_addr = listener.local_addr()?;

        let mut adjusted_meta = meta;
        adjusted_meta.http_port = local_addr.port();

        let app = build_onvif_router(adjusted_meta, profiles, auth, snapshots);

        Ok(Self {
            listener,
            app,
            local_addr,
        })
    }

    /// Returns the actual bound socket address.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serves ONVIF SOAP and snapshot HTTP requests until shutdown.
    pub async fn serve(self) -> Result<(), NvrDeskError> {
        info!(addr = %self.local_addr, "ONVIF HTTP/SOAP server listening");
        axum::serve(
            self.listener,
            self.app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .map_err(|e| NvrDeskError::Onvif(format!("ONVIF HTTP server failed: {e}")))
    }
}

async fn handle_soap(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let Some(req) = soap::parse_soap_request(&body) else {
        return soap_response(
            StatusCode::BAD_REQUEST,
            soap::soap_fault("ter:InvalidArgVal", "Malformed SOAP request body"),
        );
    };

    debug!(%peer_addr, action = %req.action, "ONVIF SOAP request");

    // Per ONVIF Core Spec, GetSystemDateAndTime must be accessible without authentication.
    if req.action != "GetSystemDateAndTime"
        && let Some(auth) = &state.auth
    {
        let soap_ok = auth.verify_soap_xml(&body);
        let basic_ok = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|hdr| auth.verify_http_basic(hdr));

        if !soap_ok && !basic_ok {
            return soap_response(
                StatusCode::UNAUTHORIZED,
                soap::soap_fault("ter:NotAuthorized", "Authentication required"),
            );
        }
    }

    let host = extract_host_ip(&headers, peer_addr);
    let profiles = state.profiles.read().expect("profiles lock").clone();
    let (src_w, src_h, src_fps) = profiles
        .first()
        .map_or((1920, 1080, 15), |p| (p.width, p.height, p.fps));

    let xml = match req.action.as_str() {
        "GetSystemDateAndTime" => soap::get_system_date_and_time_response(),
        "GetDeviceInformation" => soap::get_device_information_response(&state.meta),
        "GetCapabilities" => soap::get_capabilities_response(&host, state.meta.http_port),
        "GetServices" => soap::get_services_response(&host, state.meta.http_port),
        "GetServiceCapabilities" => soap::get_service_capabilities_response(),
        "GetScopes" => soap::get_scopes_response(&state.meta),
        "GetNetworkInterfaces" => soap::get_network_interfaces_response(&host),
        "GetHostname" => soap::get_hostname_response(),
        "GetDNS" => soap::get_dns_response(),
        "GetProfiles" => soap::get_profiles_response(&profiles),
        "GetProfile" => {
            let found = req
                .profile_token
                .as_deref()
                .and_then(|tok| profiles.iter().find(|p| p.token == tok))
                .or_else(|| profiles.first());
            match found {
                Some(p) => soap::get_profile_response(p, src_w, src_h),
                None => {
                    return soap_response(
                        StatusCode::BAD_REQUEST,
                        soap::soap_fault("ter:NoProfile", "Profile not found"),
                    );
                }
            }
        }
        "GetVideoSources" => soap::get_video_sources_response(src_w, src_h, src_fps),
        "GetVideoSourceConfigurations" | "GetVideoSourceConfiguration" => {
            soap::get_video_source_configurations_response(src_w, src_h)
        }
        "GetVideoEncoderConfigurations" | "GetVideoEncoderConfiguration" => {
            soap::get_video_encoder_configurations_response(&profiles)
        }
        "GetVideoEncoderConfigurationOptions" => {
            soap::get_video_encoder_configuration_options_response(&profiles)
        }
        "SetVideoEncoderConfiguration" => soap::set_video_encoder_configuration_response(),
        "GetStreamUri" => {
            let profile = req
                .profile_token
                .as_deref()
                .and_then(|tok| profiles.iter().find(|p| p.token == tok))
                .or_else(|| profiles.first());
            let rtsp_path = profile.map_or("/stream1", |p| p.rtsp_path.as_str());
            let uri = format!("rtsp://{host}:{}{rtsp_path}", state.meta.rtsp_port);
            soap::get_stream_uri_response(&uri)
        }
        "GetSnapshotUri" => {
            let profile = req
                .profile_token
                .as_deref()
                .and_then(|tok| profiles.iter().find(|p| p.token == tok))
                .or_else(|| profiles.first());
            let slug = profile.map_or("main", |p| p.snapshot_slug.as_str());
            let uri = format!("http://{host}:{}/snapshot/{slug}", state.meta.http_port);
            soap::get_snapshot_uri_response(&uri)
        }
        other => {
            debug!(action = %other, "unsupported ONVIF SOAP action");
            return soap_response(
                StatusCode::BAD_REQUEST,
                soap::soap_fault(
                    "ter:ActionNotSupported",
                    &format!("Unsupported action: {other}"),
                ),
            );
        }
    };

    soap_response(StatusCode::OK, xml)
}

async fn handle_snapshot(
    State(state): State<AppState>,
    Path(profile_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(auth) = &state.auth {
        let ok = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|hdr| auth.verify_http_basic(hdr));
        if !ok {
            let mut resp = (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
            resp.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static(r#"Basic realm="NVRDesk""#),
            );
            return resp;
        }
    }

    let snapshots = Arc::clone(&state.snapshots);
    let res = tokio::task::spawn_blocking(move || snapshots.capture_jpeg(&profile_slug)).await;
    match res {
        Ok(Ok(jpeg)) => {
            let mut resp = (StatusCode::OK, jpeg).into_response();
            resp.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
            resp
        }
        Ok(Err(err)) => (StatusCode::SERVICE_UNAVAILABLE, err.to_string()).into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

fn soap_response(status: StatusCode, body: String) -> Response {
    let mut resp = (status, body).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/soap+xml; charset=utf-8"),
    );
    resp
}

fn extract_host_ip(headers: &HeaderMap, peer_addr: SocketAddr) -> String {
    if let Some(host_val) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) {
        // Handle IPv6 `[::1]:8080` or IPv4 `192.168.1.5:8080`.
        if let Some(stripped) = host_val.strip_prefix('[')
            && let Some((ip, _)) = stripped.split_once(']')
        {
            return format!("[{ip}]");
        }
        if let Some((ip, _)) = host_val.rsplit_once(':') {
            return ip.to_string();
        }
        return host_val.to_string();
    }

    if let Ok(sock) =
        std::net::UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
        && sock.connect(peer_addr).is_ok()
        && let Ok(local) = sock.local_addr()
    {
        return local.ip().to_string();
    }

    "127.0.0.1".to_string()
}
