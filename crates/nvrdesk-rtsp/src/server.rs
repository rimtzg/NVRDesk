//! Async RTSP 1.0 server supporting TCP Interleaved and UDP Unicast RTP/H.264 delivery.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, AtomicUsize, Ordering},
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

use crate::{rtp::H264RtpPacketizer, sdp::build_h264_sdp};

const FRAME_BROADCAST_CAPACITY: usize = 64;

/// Configuration for the RTSP server.
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

#[derive(Clone)]
struct SharedStreamState {
    stream_path: String,
    stream_name: String,
    frame_tx: broadcast::Sender<Arc<EncodedFrame>>,
    param_sets: Arc<RwLock<Option<(Bytes, Bytes)>>>,
    param_notify: Arc<Notify>,
    demand_count: Arc<AtomicUsize>,
    demand_tx: watch::Sender<usize>,
    play_count: Arc<AtomicUsize>,
    next_session_id: Arc<AtomicU64>,
}

impl SharedStreamState {
    fn add_demand(&self) -> DemandGuard {
        let prev = self.demand_count.fetch_add(1, Ordering::AcqRel);
        let new_val = prev + 1;
        let _ = self.demand_tx.send(new_val);
        DemandGuard {
            count: Arc::clone(&self.demand_count),
            tx: self.demand_tx.clone(),
        }
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

/// Handle used by the encoder loop to publish H.264 frames and monitor client demand.
#[derive(Clone)]
pub struct RtspStreamHandle {
    state: SharedStreamState,
}

impl RtspStreamHandle {
    /// Publishes an encoded H.264 access unit to all playing RTSP clients.
    pub fn publish_frame(&self, frame: EncodedFrame) {
        if let (Some(sps), Some(pps)) = (&frame.sps, &frame.pps) {
            let mut guard = self.state.param_sets.write().expect("param_sets lock");
            let changed = guard.as_ref().is_none_or(|(s, p)| s != sps || p != pps);
            if changed {
                *guard = Some((sps.clone(), pps.clone()));
                self.state.param_notify.notify_waiters();
            }
        }
        let _ = self.state.frame_tx.send(Arc::new(frame));
    }

    /// Returns a watch receiver carrying the number of clients demanding capture/encoding.
    #[must_use]
    pub fn subscribe_demand(&self) -> watch::Receiver<usize> {
        self.state.demand_tx.subscribe()
    }

    /// Returns the number of RTSP sessions currently in `PLAY` state.
    #[must_use]
    pub fn active_players(&self) -> usize {
        self.state.play_count.load(Ordering::Acquire)
    }
}

/// Async RTSP 1.0 server.
pub struct RtspServer {
    listener: TcpListener,
    state: SharedStreamState,
}

impl RtspServer {
    /// Binds an RTSP server on `config.bind_addr` and returns `(server, local_addr, handle)`.
    pub async fn bind(
        config: RtspServerConfig,
    ) -> Result<(Self, SocketAddr, RtspStreamHandle), NvrDeskError> {
        let listener = TcpListener::bind(config.bind_addr).await?;
        let local_addr = listener.local_addr()?;
        let normalized_path = normalize_stream_path(&config.stream_path);
        let (frame_tx, _) = broadcast::channel(FRAME_BROADCAST_CAPACITY);
        let (demand_tx, _) = watch::channel(0usize);

        let state = SharedStreamState {
            stream_path: normalized_path,
            stream_name: config.stream_name,
            frame_tx,
            param_sets: Arc::new(RwLock::new(None)),
            param_notify: Arc::new(Notify::new()),
            demand_count: Arc::new(AtomicUsize::new(0)),
            demand_tx,
            play_count: Arc::new(AtomicUsize::new(0)),
            next_session_id: Arc::new(AtomicU64::new(1000)),
        };

        let handle = RtspStreamHandle {
            state: state.clone(),
        };

        Ok((Self { listener, state }, local_addr, handle))
    }

    /// Runs the RTSP accept loop until the task is cancelled or encounters a fatal accept error.
    pub async fn serve(self) -> Result<(), NvrDeskError> {
        info!(addr = %self.listener.local_addr()?, path = %self.state.stream_path, "RTSP server listening");
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
    transport: Option<ClientTransport>,
    playing: bool,
    awaiting_keyframe: bool,
    _demand_guard: Option<DemandGuard>,
    _play_guard: Option<PlayGuard>,
}

async fn handle_client(
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    state: SharedStreamState,
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
        transport: None,
        playing: false,
        awaiting_keyframe: true,
        _demand_guard: None,
        _play_guard: None,
    };

    let mut packetizer = H264RtpPacketizer::new(session.ssrc);
    let mut frame_rx = state.frame_tx.subscribe();
    let mut read_buf = BytesMut::with_capacity(8192);

    debug!(%peer_addr, session = %session.id, "RTSP client connected");

    loop {
        if session.playing {
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
                recv_res = frame_rx.recv() => {
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
                            warn!(%peer_addr, skipped, "RTSP client lagged behind video stream; waiting for next keyframe");
                            session.awaiting_keyframe = true;
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
    state: &SharedStreamState,
    session: &mut SessionState,
    frame_rx: &mut broadcast::Receiver<Arc<EncodedFrame>>,
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
    state: &SharedStreamState,
    session: &mut SessionState,
    frame_rx: &mut broadcast::Receiver<Arc<EncodedFrame>>,
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

    match req.method() {
        Method::Options => {
            let resp = Response::builder(Version::V1_0, StatusCode::Ok)
                .header(headers::CSEQ, cseq)
                .header(
                    headers::PUBLIC,
                    "OPTIONS, DESCRIBE, SETUP, PLAY, GET_PARAMETER, TEARDOWN",
                )
                .empty();
            write_rtsp_response(stream, &resp).await?;
        }
        Method::Describe => {
            if uri_path != state.stream_path {
                let resp = Response::builder(Version::V1_0, StatusCode::NotFound)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            }

            // Ensure SPS/PPS are available for SDP by momentarily requesting capture if needed.
            let (sps, pps) = ensure_parameter_sets(state).await;
            let sdp = build_h264_sdp(local_ip, &state.stream_name, sps.as_deref(), pps.as_deref());
            let content_base = req
                .request_uri()
                .map(|u| format!("{}/", u.as_str().trim_end_matches('/')))
                .unwrap_or_else(|| format!("rtsp://{local_ip}{}/", state.stream_path));

            let resp = Response::builder(Version::V1_0, StatusCode::Ok)
                .header(headers::CSEQ, cseq)
                .header(headers::CONTENT_TYPE, "application/sdp")
                .header(headers::CONTENT_BASE, content_base)
                .build(sdp.into_bytes());
            write_rtsp_response(stream, &resp).await?;
        }
        Method::Setup => {
            if !matches_stream_or_track(&uri_path, &state.stream_path) {
                let resp = Response::builder(Version::V1_0, StatusCode::NotFound)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            }

            let transport_reply = setup_transport(&req, peer_addr, session).await?;
            let Some(reply_header) = transport_reply else {
                let resp = Response::builder(Version::V1_0, StatusCode::UnsupportedTransport)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            };

            let resp = Response::builder(Version::V1_0, StatusCode::Ok)
                .header(headers::CSEQ, cseq)
                .header(headers::TRANSPORT, reply_header)
                .header(headers::SESSION, format!("{};timeout=60", session.id))
                .empty();
            write_rtsp_response(stream, &resp).await?;
        }
        Method::Play => {
            if session.transport.is_none() {
                let resp = Response::builder(Version::V1_0, StatusCode::MethodNotValidInThisState)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            }

            if !session.playing {
                session._demand_guard = Some(state.add_demand());
                let prev = state.play_count.fetch_add(1, Ordering::AcqRel);
                session._play_guard = Some(PlayGuard {
                    count: Arc::clone(&state.play_count),
                });
                session.playing = true;
                session.awaiting_keyframe = true;
                *frame_rx = state.frame_tx.subscribe();
                info!(%peer_addr, session = %session.id, active_players = prev + 1, "RTSP client started PLAY");
            }

            let base_uri = req
                .request_uri()
                .map(|u| u.as_str().trim_end_matches('/').to_string())
                .unwrap_or_else(|| format!("rtsp://{local_ip}{}", state.stream_path));

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
        Method::GetParameter => {
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

async fn ensure_parameter_sets(state: &SharedStreamState) -> (Option<Bytes>, Option<Bytes>) {
    if let Some((sps, pps)) = state.param_sets.read().expect("param_sets lock").clone() {
        return (Some(sps), Some(pps));
    }

    // Wake capture+encoder temporarily until the first keyframe produces SPS/PPS.
    let _guard = state.add_demand();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    loop {
        if let Some((sps, pps)) = state.param_sets.read().expect("param_sets lock").clone() {
            return (Some(sps), Some(pps));
        }
        if tokio::time::Instant::now() >= deadline {
            return (None, None);
        }
        let _ = tokio::time::timeout_at(deadline, state.param_notify.notified()).await;
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

fn normalize_stream_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn matches_stream_or_track(uri_path: &str, stream_path: &str) -> bool {
    if uri_path == stream_path {
        return true;
    }
    let prefix = format!("{stream_path}/");
    uri_path.starts_with(&prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths_and_track_ids() {
        assert_eq!(normalize_stream_path("/stream/"), "/stream");
        assert_eq!(normalize_stream_path("stream"), "/stream");
        assert!(matches_stream_or_track("/stream", "/stream"));
        assert!(matches_stream_or_track("/stream/trackID=0", "/stream"));
        assert!(!matches_stream_or_track("/other", "/stream"));
    }
}
