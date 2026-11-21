//! Async RTSP 1.0 server supporting multi-stream, Digest auth, TCP Interleaved, and UDP Unicast.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use nvrdesk_core::{EncodedFrame, NvrDeskError};
use rtsp_types::{
    Message, Method, Request, Response, StatusCode, Version,
    headers::{self, RtpLowerTransport, Transports},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{Notify, broadcast, watch},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{auth::RtspAuthConfig, rtp::H264RtpPacketizer, sdp::build_h264_sdp};

const FRAME_BROADCAST_CAPACITY: usize = 64;

/// Configuration for a single-stream RTSP server (`RtspServer::bind`).
#[derive(Debug, Clone)]
pub struct RtspServerConfig {
    /// Socket address to bind on (e.g. `0.0.0.0:8554`).
    pub bind_addr: SocketAddr,
    /// RTSP stream path (e.g. `/stream`).
    pub stream_path: String,
    /// SDP session name (`s=` line).
    pub stream_name: String,
}

impl Default for RtspServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8554),
            stream_path: "/stream".to_string(),
            stream_name: "NVRDesk Screen Stream".to_string(),
        }
    }
}

/// Definition of a stream mount point on a multi-stream RTSP server.
#[derive(Debug, Clone)]
pub struct RtspStreamSpec {
    /// Primary RTSP path (e.g. `/stream1`).
    pub path: String,
    /// Optional alias paths pointing to the same stream (e.g. `/stream`).
    pub aliases: Vec<String>,
    /// SDP session name (`s=` line).
    pub name: String,
}

#[derive(Clone)]
struct StreamEntry {
    stream_path: String,
    stream_name: String,
    frame_tx: broadcast::Sender<Arc<EncodedFrame>>,
    param_sets: Arc<RwLock<Option<(Bytes, Bytes)>>>,
    param_notify: Arc<Notify>,
    demand_count: Arc<AtomicUsize>,
    demand_tx: watch::Sender<usize>,
    play_count: Arc<AtomicUsize>,
    keyframe_requested: Arc<AtomicBool>,
}

impl StreamEntry {
    fn new(stream_path: String, stream_name: String) -> Arc<Self> {
        let (frame_tx, _) = broadcast::channel(FRAME_BROADCAST_CAPACITY);
        let (demand_tx, _) = watch::channel(0usize);
        Arc::new(Self {
            stream_path,
            stream_name,
            frame_tx,
            param_sets: Arc::new(RwLock::new(None)),
            param_notify: Arc::new(Notify::new()),
            demand_count: Arc::new(AtomicUsize::new(0)),
            demand_tx,
            play_count: Arc::new(AtomicUsize::new(0)),
            keyframe_requested: Arc::new(AtomicBool::new(false)),
        })
    }

    fn add_demand(&self) -> DemandGuard {
        let prev = self.demand_count.fetch_add(1, Ordering::AcqRel);
        let _ = self.demand_tx.send(prev + 1);
        DemandGuard {
            count: Arc::clone(&self.demand_count),
            tx: self.demand_tx.clone(),
        }
    }
}

#[derive(Clone)]
struct ServerState {
    streams: Arc<HashMap<String, Arc<StreamEntry>>>,
    auth: Option<RtspAuthConfig>,
    next_session_id: Arc<AtomicU64>,
}

impl ServerState {
    fn resolve_stream(&self, uri_path: &str) -> Option<&Arc<StreamEntry>> {
        if let Some(exact) = self.streams.get(uri_path) {
            return Some(exact);
        }
        // Strip `/trackID=...` suffix for SETUP requests.
        if let Some((base, _)) = uri_path.rsplit_once("/trackID=") {
            return self.streams.get(base);
        }
        None
    }
}

struct DemandGuard {
    count: Arc<AtomicUsize>,
    tx: watch::Sender<usize>,
}

impl Drop for DemandGuard {
    fn drop(&mut self) {
        let prev = self.count.fetch_sub(1, Ordering::AcqRel);
        let _ = self.tx.send(prev.saturating_sub(1));
    }
}

struct PlayGuard {
    count: Arc<AtomicUsize>,
}

impl Drop for PlayGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Handle used by an encoder loop to publish H.264 frames and monitor client demand.
#[derive(Clone)]
pub struct RtspStreamHandle {
    entry: Arc<StreamEntry>,
}

impl RtspStreamHandle {
    /// Publishes an encoded H.264 access unit to all playing RTSP clients on this stream.
    pub fn publish_frame(&self, frame: EncodedFrame) {
        if let (Some(sps), Some(pps)) = (&frame.sps, &frame.pps) {
            let mut guard = self.entry.param_sets.write().expect("param_sets lock");
            let changed = guard.as_ref().is_none_or(|(s, p)| s != sps || p != pps);
            if changed {
                *guard = Some((sps.clone(), pps.clone()));
                self.entry.param_notify.notify_waiters();
            }
        }
        let _ = self.entry.frame_tx.send(Arc::new(frame));
    }

    /// Returns a watch receiver carrying the number of clients demanding capture/encoding.
    #[must_use]
    pub fn subscribe_demand(&self) -> watch::Receiver<usize> {
        self.entry.demand_tx.subscribe()
    }

    /// Returns the number of RTSP sessions currently in `PLAY` state on this stream.
    #[must_use]
    pub fn active_players(&self) -> usize {
        self.entry.play_count.load(Ordering::Acquire)
    }

    /// Returns `true` (and clears the flag) if a newly joined client requested an immediate IDR.
    #[must_use]
    pub fn take_keyframe_request(&self) -> bool {
        self.entry.keyframe_requested.swap(false, Ordering::AcqRel)
    }
}

/// Async RTSP 1.0 server.
pub struct RtspServer {
    listener: TcpListener,
    state: ServerState,
}

impl RtspServer {
    /// Binds a single-stream RTSP server on `config.bind_addr`.
    pub async fn bind(
        config: RtspServerConfig,
    ) -> Result<(Self, SocketAddr, RtspStreamHandle), NvrDeskError> {
        let spec = RtspStreamSpec {
            path: config.stream_path.clone(),
            aliases: Vec::new(),
            name: config.stream_name,
        };
        let (server, addr, mut handles) =
            Self::bind_streams(config.bind_addr, None, vec![spec]).await?;
        let norm_path = normalize_stream_path(&config.stream_path);
        let handle = handles
            .remove(&norm_path)
            .ok_or_else(|| NvrDeskError::Rtsp("missing primary stream handle".into()))?;
        Ok((server, addr, handle))
    }

    /// Binds a multi-stream RTSP server on `bind_addr`, returning handles keyed by normalized path.
    pub async fn bind_streams(
        bind_addr: SocketAddr,
        auth: Option<RtspAuthConfig>,
        specs: Vec<RtspStreamSpec>,
    ) -> Result<(Self, SocketAddr, HashMap<String, RtspStreamHandle>), NvrDeskError> {
        if specs.is_empty() {
            return Err(NvrDeskError::Rtsp(
                "at least one RTSP stream is required".into(),
            ));
        }

        let listener = TcpListener::bind(bind_addr).await?;
        let local_addr = listener.local_addr()?;

        let mut streams = HashMap::new();
        let mut handles = HashMap::new();

        for spec in specs {
            let norm_path = normalize_stream_path(&spec.path);
            let entry = StreamEntry::new(norm_path.clone(), spec.name);
            handles.insert(
                norm_path.clone(),
                RtspStreamHandle {
                    entry: Arc::clone(&entry),
                },
            );
            streams.insert(norm_path, Arc::clone(&entry));
            for alias in spec.aliases {
                let norm_alias = normalize_stream_path(&alias);
                streams
                    .entry(norm_alias)
                    .or_insert_with(|| Arc::clone(&entry));
            }
        }

        let state = ServerState {
            streams: Arc::new(streams),
            auth,
            next_session_id: Arc::new(AtomicU64::new(1000)),
        };

        Ok((Self { listener, state }, local_addr, handles))
    }

    /// Runs the RTSP accept loop until the task is cancelled or encounters a fatal accept error.
    pub async fn serve(self) -> Result<(), NvrDeskError> {
        let addr = self.listener.local_addr()?;
        let mut paths: Vec<&str> = self.state.streams.keys().map(String::as_str).collect();
        paths.sort_unstable();
        info!(%addr, ?paths, auth_enabled = self.state.auth.is_some(), "RTSP server listening");
        loop {
            let (stream, peer_addr) = self.listener.accept().await?;
            let state = self.state.clone();
            tokio::spawn(async move {
                if let Err(err) = handle_client(stream, peer_addr, state).await {
                    debug!(%peer_addr, error = %err, "RTSP client session ended");
                }
            });
        }
    }
}

enum ClientTransport {
    TcpInterleaved {
        rtp_channel: u8,
    },
    Udp {
        socket: UdpSocket,
        rtp_dest: SocketAddr,
    },
}

struct SessionState {
    id: String,
    ssrc: u32,
    nonce: String,
    stream: Option<Arc<StreamEntry>>,
    transport: Option<ClientTransport>,
    playing: bool,
    awaiting_keyframe: bool,
    _demand_guard: Option<DemandGuard>,
    _play_guard: Option<PlayGuard>,
}

async fn handle_client(
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    state: ServerState,
) -> Result<(), NvrDeskError> {
    let local_ip = stream
        .local_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let _ = stream.set_nodelay(true);

    let session_num = state.next_session_id.fetch_add(1, Ordering::Relaxed);
    let mut session = SessionState {
        id: format!("{session_num:08X}"),
        ssrc: (session_num as u32) ^ 0x4E56_5244,
        nonce: Uuid::new_v4().simple().to_string(),
        stream: None,
        transport: None,
        playing: false,
        awaiting_keyframe: true,
        _demand_guard: None,
        _play_guard: None,
    };

    let mut packetizer = H264RtpPacketizer::new(session.ssrc);
    let mut frame_rx: Option<broadcast::Receiver<Arc<EncodedFrame>>> = None;
    let mut read_buf = BytesMut::with_capacity(8192);

    debug!(%peer_addr, session = %session.id, "RTSP client connected");

    loop {
        if session.playing
            && let Some(rx) = frame_rx.as_mut()
        {
            tokio::select! {
                read_res = stream.read_buf(&mut read_buf) => {
                    let n = read_res?;
                    if n == 0 {
                        return Ok(());
                    }
                    if !process_read_buffer(
                        &mut read_buf,
                        &mut stream,
                        peer_addr,
                        &local_ip,
                        &state,
                        &mut session,
                        &mut frame_rx,
                        &packetizer,
                    ).await? {
                        return Ok(());
                    }
                }
                recv_res = rx.recv() => {
                    match recv_res {
                        Ok(frame) => {
                            if session.awaiting_keyframe {
                                if !frame.is_keyframe {
                                    continue;
                                }
                                session.awaiting_keyframe = false;
                            }
                            send_encoded_frame(
                                &mut stream,
                                &mut session,
                                &mut packetizer,
                                &frame,
                            ).await?;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(%peer_addr, skipped, "RTSP client lagged; waiting for next keyframe");
                            session.awaiting_keyframe = true;
                            if let Some(entry) = &session.stream {
                                entry.keyframe_requested.store(true, Ordering::Release);
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                }
            }
        } else {
            let n = stream.read_buf(&mut read_buf).await?;
            if n == 0 {
                return Ok(());
            }
            if !process_read_buffer(
                &mut read_buf,
                &mut stream,
                peer_addr,
                &local_ip,
                &state,
                &mut session,
                &mut frame_rx,
                &packetizer,
            )
            .await?
            {
                return Ok(());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_read_buffer(
    read_buf: &mut BytesMut,
    stream: &mut TcpStream,
    peer_addr: SocketAddr,
    local_ip: &str,
    state: &ServerState,
    session: &mut SessionState,
    frame_rx: &mut Option<broadcast::Receiver<Arc<EncodedFrame>>>,
    packetizer: &H264RtpPacketizer,
) -> Result<bool, NvrDeskError> {
    loop {
        if read_buf.is_empty() {
            return Ok(true);
        }

        // Skip client-to-server interleaved RTP/RTCP packets (e.g. RTCP Receiver Reports).
        if read_buf[0] == b'$' {
            if read_buf.len() < 4 {
                return Ok(true);
            }
            let payload_len = u16::from_be_bytes([read_buf[2], read_buf[3]]) as usize;
            if read_buf.len() < 4 + payload_len {
                return Ok(true);
            }
            read_buf.advance(4 + payload_len);
            continue;
        }

        match Message::<Vec<u8>>::parse(read_buf.as_ref()) {
            Ok((msg, consumed)) => {
                read_buf.advance(consumed);
                if let Message::Request(req) = msg {
                    let keep_alive = handle_request(
                        req, stream, peer_addr, local_ip, state, session, frame_rx, packetizer,
                    )
                    .await?;
                    if !keep_alive {
                        return Ok(false);
                    }
                }
            }
            Err(rtsp_types::ParseError::Incomplete(_)) => return Ok(true),
            Err(err) => {
                return Err(NvrDeskError::Rtsp(format!("RTSP parse error: {err}")));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    req: Request<Vec<u8>>,
    stream: &mut TcpStream,
    peer_addr: SocketAddr,
    local_ip: &str,
    state: &ServerState,
    session: &mut SessionState,
    frame_rx: &mut Option<broadcast::Receiver<Arc<EncodedFrame>>>,
    packetizer: &H264RtpPacketizer,
) -> Result<bool, NvrDeskError> {
    let cseq = req
        .header(&headers::CSEQ)
        .map(|v| v.as_str().to_string())
        .unwrap_or_else(|| "0".to_string());

    let uri_path = req
        .request_uri()
        .map(|u| normalize_stream_path(u.path()))
        .unwrap_or_default();

    debug!(%peer_addr, method = ?req.method(), path = %uri_path, %cseq, "RTSP request");

    if !matches!(req.method(), Method::Options)
        && let Some(auth) = &state.auth
    {
        let method_name = method_as_str(req.method());
        let authorized = req.header(&headers::AUTHORIZATION).is_some_and(|hdr| {
            auth.verify_authorization(hdr.as_str(), method_name, &session.nonce)
        });
        if !authorized {
            let resp = Response::builder(Version::V1_0, StatusCode::Unauthorized)
                .header(headers::CSEQ, cseq)
                .header(
                    headers::WWW_AUTHENTICATE,
                    RtspAuthConfig::challenge_header(&session.nonce),
                )
                .empty();
            write_rtsp_response(stream, &resp).await?;
            return Ok(true);
        }
    }

    match req.method() {
        Method::Options => {
            let resp = Response::builder(Version::V1_0, StatusCode::Ok)
                .header(headers::CSEQ, cseq)
                .header(
                    headers::PUBLIC,
                    "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, GET_PARAMETER, SET_PARAMETER, TEARDOWN",
                )
                .empty();
            write_rtsp_response(stream, &resp).await?;
        }
        Method::Describe => {
            let Some(entry) = state.resolve_stream(&uri_path).cloned() else {
                let resp = Response::builder(Version::V1_0, StatusCode::NotFound)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            };

            let (sps, pps) = ensure_parameter_sets(&entry).await;
            let sdp = build_h264_sdp(local_ip, &entry.stream_name, sps.as_deref(), pps.as_deref());
            let content_base = req
                .request_uri()
                .map(|u| format!("{}/", u.as_str().trim_end_matches('/')))
                .unwrap_or_else(|| format!("rtsp://{local_ip}{}/", entry.stream_path));

            let resp = Response::builder(Version::V1_0, StatusCode::Ok)
                .header(headers::CSEQ, cseq)
                .header(headers::CONTENT_TYPE, "application/sdp")
                .header(headers::CONTENT_BASE, content_base)
                .build(sdp.into_bytes());
            write_rtsp_response(stream, &resp).await?;
        }
        Method::Setup => {
            let Some(entry) = state.resolve_stream(&uri_path).cloned() else {
                let resp = Response::builder(Version::V1_0, StatusCode::NotFound)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            };

            let transport_reply = setup_transport(&req, peer_addr, session).await?;
            let Some(reply_header) = transport_reply else {
                let resp = Response::builder(Version::V1_0, StatusCode::UnsupportedTransport)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            };

            session.stream = Some(entry);

            let resp = Response::builder(Version::V1_0, StatusCode::Ok)
                .header(headers::CSEQ, cseq)
                .header(headers::TRANSPORT, reply_header)
                .header(headers::SESSION, format!("{};timeout=60", session.id))
                .empty();
            write_rtsp_response(stream, &resp).await?;
        }
        Method::Play => {
            let Some(entry) = session.stream.clone() else {
                let resp = Response::builder(Version::V1_0, StatusCode::MethodNotValidInThisState)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            };
            if session.transport.is_none() {
                let resp = Response::builder(Version::V1_0, StatusCode::MethodNotValidInThisState)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            }

            if !session.playing {
                session._demand_guard = Some(entry.add_demand());
                let prev = entry.play_count.fetch_add(1, Ordering::AcqRel);
                session._play_guard = Some(PlayGuard {
                    count: Arc::clone(&entry.play_count),
                });
                session.playing = true;
                session.awaiting_keyframe = true;
                entry.keyframe_requested.store(true, Ordering::Release);
                *frame_rx = Some(entry.frame_tx.subscribe());
                info!(%peer_addr, path = %entry.stream_path, session = %session.id, active_players = prev + 1, "RTSP client started PLAY");
            }

            let base_uri = req
                .request_uri()
                .map(|u| u.as_str().trim_end_matches('/').to_string())
                .unwrap_or_else(|| format!("rtsp://{local_ip}{}", entry.stream_path));

            let rtp_info = format!(
                "url={base_uri}/trackID=0;seq={}",
                packetizer.next_sequence_number()
            );

            let resp = Response::builder(Version::V1_0, StatusCode::Ok)
                .header(headers::CSEQ, cseq)
                .header(headers::SESSION, session.id.as_str())
                .header(headers::RANGE, "npt=0.000-")
                .header(headers::RTP_INFO, rtp_info)
                .empty();
            write_rtsp_response(stream, &resp).await?;
        }
        Method::Pause => {
            session.playing = false;
            session._play_guard.take();
            session._demand_guard.take();
            let resp = Response::builder(Version::V1_0, StatusCode::Ok)
                .header(headers::CSEQ, cseq)
                .header(headers::SESSION, session.id.as_str())
                .empty();
            write_rtsp_response(stream, &resp).await?;
        }
        Method::GetParameter | Method::SetParameter => {
            let resp = Response::builder(Version::V1_0, StatusCode::Ok)
                .header(headers::CSEQ, cseq)
                .header(headers::SESSION, session.id.as_str())
                .empty();
            write_rtsp_response(stream, &resp).await?;
        }
        Method::Teardown => {
            let resp = Response::builder(Version::V1_0, StatusCode::Ok)
                .header(headers::CSEQ, cseq)
                .header(headers::SESSION, session.id.as_str())
                .empty();
            write_rtsp_response(stream, &resp).await?;
            session.playing = false;
            session._play_guard.take();
            session._demand_guard.take();
            return Ok(false);
        }
        _ => {
            let resp = Response::builder(Version::V1_0, StatusCode::MethodNotAllowed)
                .header(headers::CSEQ, cseq)
                .empty();
            write_rtsp_response(stream, &resp).await?;
        }
    }

    Ok(true)
}

async fn ensure_parameter_sets(entry: &StreamEntry) -> (Option<Bytes>, Option<Bytes>) {
    if let Some((sps, pps)) = entry.param_sets.read().expect("param_sets lock").clone() {
        return (Some(sps), Some(pps));
    }

    // Wake capture+encoder temporarily until the first keyframe produces SPS/PPS.
    let _guard = entry.add_demand();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    loop {
        if let Some((sps, pps)) = entry.param_sets.read().expect("param_sets lock").clone() {
            return (Some(sps), Some(pps));
        }
        if tokio::time::Instant::now() >= deadline {
            return (None, None);
        }
        let _ = tokio::time::timeout_at(deadline, entry.param_notify.notified()).await;
    }
}

async fn setup_transport(
    req: &Request<Vec<u8>>,
    peer_addr: SocketAddr,
    session: &mut SessionState,
) -> Result<Option<String>, NvrDeskError> {
    let Ok(Some(transports)) = req.typed_header::<Transports>() else {
        return Ok(None);
    };

    for transport in transports.iter() {
        let rtsp_types::headers::Transport::Rtp(rtp) = transport else {
            continue;
        };

        if matches!(rtp.lower_transport, Some(RtpLowerTransport::Tcp))
            || rtp.params.interleaved.is_some()
        {
            let (rtp_ch, rtcp_ch) = match rtp.params.interleaved {
                Some((a, Some(b))) => (a, b),
                Some((a, None)) => (a, a.saturating_add(1)),
                None => (0, 1),
            };
            session.transport = Some(ClientTransport::TcpInterleaved {
                rtp_channel: rtp_ch,
            });
            return Ok(Some(format!(
                "RTP/AVP/TCP;unicast;interleaved={rtp_ch}-{rtcp_ch};ssrc={:08X}",
                session.ssrc
            )));
        }

        if rtp.params.unicast
            && let Some((client_rtp_port, client_rtcp_opt)) = rtp.params.client_port
        {
            let client_rtcp_port =
                client_rtcp_opt.unwrap_or_else(|| client_rtp_port.saturating_add(1));
            let udp_socket =
                UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)).await?;
            let server_rtp_port = udp_socket.local_addr()?.port();
            let server_rtcp_port = server_rtp_port.saturating_add(1);
            let rtp_dest = SocketAddr::new(peer_addr.ip(), client_rtp_port);

            session.transport = Some(ClientTransport::Udp {
                socket: udp_socket,
                rtp_dest,
            });

            return Ok(Some(format!(
                "RTP/AVP;unicast;client_port={client_rtp_port}-{client_rtcp_port};server_port={server_rtp_port}-{server_rtcp_port};ssrc={:08X}",
                session.ssrc
            )));
        }
    }

    Ok(None)
}

async fn send_encoded_frame(
    stream: &mut TcpStream,
    session: &mut SessionState,
    packetizer: &mut H264RtpPacketizer,
    frame: &EncodedFrame,
) -> Result<(), NvrDeskError> {
    let packets = packetizer.packetize_frame(frame);
    if packets.is_empty() {
        return Ok(());
    }

    match &session.transport {
        Some(ClientTransport::TcpInterleaved { rtp_channel }) => {
            let mut batch = BytesMut::new();
            for pkt in packets {
                batch.put_u8(b'$');
                batch.put_u8(*rtp_channel);
                batch.put_u16(pkt.len() as u16);
                batch.extend_from_slice(&pkt);
            }
            stream.write_all(&batch).await?;
        }
        Some(ClientTransport::Udp { socket, rtp_dest }) => {
            for pkt in packets {
                socket.send_to(&pkt, *rtp_dest).await?;
            }
        }
        None => {}
    }

    Ok(())
}

async fn write_rtsp_response<B: AsRef<[u8]>>(
    stream: &mut TcpStream,
    resp: &Response<B>,
) -> Result<(), NvrDeskError> {
    let mut out = Vec::with_capacity(512);
    resp.write(&mut out)
        .map_err(|e| NvrDeskError::Rtsp(format!("failed to serialize RTSP response: {e}")))?;
    stream.write_all(&out).await?;
    Ok(())
}

fn method_as_str(method: &Method) -> &str {
    match method {
        Method::Options => "OPTIONS",
        Method::Describe => "DESCRIBE",
        Method::Setup => "SETUP",
        Method::Play => "PLAY",
        Method::Pause => "PAUSE",
        Method::GetParameter => "GET_PARAMETER",
        Method::SetParameter => "SET_PARAMETER",
        Method::Teardown => "TEARDOWN",
        _ => "",
    }
}

/// Normalizes an RTSP stream path to always start with `/` and omit trailing slashes.
#[must_use]
pub fn normalize_stream_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize_stream_path("/stream1/"), "/stream1");
        assert_eq!(normalize_stream_path("stream2"), "/stream2");
    }
}
