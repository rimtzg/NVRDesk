//! Async RTSP 1.0 server supporting multi-stream, Digest auth, audio track, TCP & UDP.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use nvrdesk_core::{AudioCodecKind, EncodedAudioFrame, EncodedFrame, NvrDeskError, VideoCodecKind};
use rtsp_types::{
    Message, Method, Request, Response, StatusCode, Version,
    headers::{self, HeaderName, RtpLowerTransport, Transports},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    sync::{Notify, broadcast, watch},
};
use tokio_rustls::{TlsAcceptor, rustls};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    audio_rtp::AudioRtpPacketizer, auth::RtspAuthConfig, rtp::H264RtpPacketizer,
    sdp::build_stream_sdp_full,
};

const FRAME_BROADCAST_CAPACITY: usize = 64;
const AUDIO_BROADCAST_CAPACITY: usize = 128;

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

/// Callback invoked for incoming VMS-to-POS ONVIF audio backchannel payloads `(rtp_payload_type, raw_audio_bytes)`.
pub type BackchannelSink = Arc<dyn Fn(u8, &[u8]) + Send + Sync>;

/// Definition of a stream mount point on a multi-stream RTSP server.
#[derive(Debug, Clone, Default)]
pub struct RtspStreamSpec {
    /// Primary RTSP path (e.g. `/stream1`).
    pub path: String,
    /// Optional alias paths pointing to the same stream (e.g. `/stream`).
    pub aliases: Vec<String>,
    /// SDP session name (`s=` line).
    pub name: String,
    /// Video codec advertised in SDP (`H264` or `H265`).
    pub video_codec: VideoCodecKind,
    /// Optional G.711 audio codec advertised on `trackID=1`.
    pub audio_codec: Option<AudioCodecKind>,
    /// Whether ONVIF audio backchannel (`trackID=2`, `a=recvonly`) is enabled.
    pub backchannel: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct VideoParamSets {
    vps: Option<Bytes>,
    sps: Bytes,
    pps: Bytes,
}

#[derive(Clone)]
struct StreamEntry {
    stream_path: String,
    stream_name: String,
    video_codec: Arc<RwLock<VideoCodecKind>>,
    audio_codec: Arc<RwLock<Option<AudioCodecKind>>>,
    backchannel: Arc<RwLock<bool>>,
    frame_tx: broadcast::Sender<Arc<EncodedFrame>>,
    audio_tx: broadcast::Sender<Arc<EncodedAudioFrame>>,
    param_sets: Arc<RwLock<Option<VideoParamSets>>>,
    param_notify: Arc<Notify>,
    demand_count: Arc<AtomicUsize>,
    demand_tx: watch::Sender<usize>,
    play_count: Arc<AtomicUsize>,
    keyframe_requested: Arc<AtomicBool>,
}

impl StreamEntry {
    fn new(
        stream_path: String,
        stream_name: String,
        video_codec: VideoCodecKind,
        audio_codec: Option<AudioCodecKind>,
        backchannel: bool,
    ) -> Arc<Self> {
        let (frame_tx, _) = broadcast::channel(FRAME_BROADCAST_CAPACITY);
        let (audio_tx, _) = broadcast::channel(AUDIO_BROADCAST_CAPACITY);
        let (demand_tx, _) = watch::channel(0usize);
        Arc::new(Self {
            stream_path,
            stream_name,
            video_codec: Arc::new(RwLock::new(video_codec)),
            audio_codec: Arc::new(RwLock::new(audio_codec)),
            backchannel: Arc::new(RwLock::new(backchannel)),
            frame_tx,
            audio_tx,
            param_sets: Arc::new(RwLock::new(None)),
            param_notify: Arc::new(Notify::new()),
            demand_count: Arc::new(AtomicUsize::new(0)),
            demand_tx,
            play_count: Arc::new(AtomicUsize::new(0)),
            keyframe_requested: Arc::new(AtomicBool::new(false)),
        })
    }

    fn current_video_codec(&self) -> VideoCodecKind {
        *self.video_codec.read().expect("video_codec lock")
    }

    fn set_video_codec(&self, codec: VideoCodecKind) {
        let mut guard = self.video_codec.write().expect("video_codec lock");
        if *guard != codec {
            *guard = codec;
            *self.param_sets.write().expect("param_sets lock") = None;
        }
    }

    fn current_audio_codec(&self) -> Option<AudioCodecKind> {
        *self.audio_codec.read().expect("audio_codec lock")
    }

    fn set_audio_codec(&self, codec: Option<AudioCodecKind>) {
        *self.audio_codec.write().expect("audio_codec lock") = codec;
    }

    fn current_backchannel(&self) -> bool {
        *self.backchannel.read().expect("backchannel lock")
    }

    fn set_backchannel(&self, enabled: bool) {
        *self.backchannel.write().expect("backchannel lock") = enabled;
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

/// Dynamic registry for adding, updating, or removing RTSP stream mount points at runtime.
#[derive(Clone)]
pub struct RtspRegistry {
    streams: Arc<RwLock<HashMap<String, Arc<StreamEntry>>>>,
    backchannel_sink: Arc<RwLock<Option<BackchannelSink>>>,
}

impl RtspRegistry {
    /// Registers a new RTSP stream (or updates the existing stream's codecs/backchannel/aliases) at runtime.
    #[must_use]
    pub fn add_or_update_stream(&self, spec: RtspStreamSpec) -> RtspStreamHandle {
        let norm_path = normalize_stream_path(&spec.path);
        let mut streams = self.streams.write().expect("streams write lock");

        let entry = if let Some(existing) = streams.get(&norm_path).cloned() {
            existing.set_video_codec(spec.video_codec);
            existing.set_audio_codec(spec.audio_codec);
            existing.set_backchannel(spec.backchannel);
            existing
        } else {
            let created = StreamEntry::new(
                norm_path.clone(),
                spec.name,
                spec.video_codec,
                spec.audio_codec,
                spec.backchannel,
            );
            streams.insert(norm_path, Arc::clone(&created));
            created
        };

        for alias in spec.aliases {
            let norm_alias = normalize_stream_path(&alias);
            streams
                .entry(norm_alias)
                .or_insert_with(|| Arc::clone(&entry));
        }

        RtspStreamHandle { entry }
    }

    /// Installs or clears the callback receiving decoded RTP backchannel audio bytes.
    pub fn set_backchannel_sink(&self, sink: Option<BackchannelSink>) {
        *self
            .backchannel_sink
            .write()
            .expect("backchannel sink lock") = sink;
    }

    /// Removes an RTSP stream and all aliases pointing to it.
    pub fn remove_stream(&self, path: &str) {
        let norm_path = normalize_stream_path(path);
        let mut streams = self.streams.write().expect("streams write lock");
        streams.retain(|_, entry| entry.stream_path != norm_path);
    }

    /// Returns total active `PLAY` sessions across all currently registered unique streams.
    #[must_use]
    pub fn active_players(&self) -> usize {
        let streams = self.streams.read().expect("streams read lock");
        let mut seen = HashSet::new();
        let mut total = 0usize;
        for entry in streams.values() {
            if seen.insert(entry.stream_path.clone()) {
                total += entry.play_count.load(Ordering::Acquire);
            }
        }
        total
    }
}

#[derive(Clone)]
struct ServerState {
    streams: Arc<RwLock<HashMap<String, Arc<StreamEntry>>>>,
    auth: Option<RtspAuthConfig>,
    next_session_id: Arc<AtomicU64>,
    backchannel_sink: Arc<RwLock<Option<BackchannelSink>>>,
}

impl ServerState {
    fn resolve_stream_and_track(&self, uri_path: &str) -> Option<(Arc<StreamEntry>, Option<u8>)> {
        let streams = self.streams.read().expect("streams read lock");
        if let Some(exact) = streams.get(uri_path) {
            return Some((Arc::clone(exact), None));
        }
        if let Some((base, track_str)) = uri_path.rsplit_once("/trackID=")
            && let Some(entry) = streams.get(base)
        {
            let track_id = track_str.parse::<u8>().ok();
            return Some((Arc::clone(entry), track_id));
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

/// Handle used by an encoder loop to publish H.264/audio frames and monitor client demand.
#[derive(Clone)]
pub struct RtspStreamHandle {
    entry: Arc<StreamEntry>,
}

impl RtspStreamHandle {
    /// Publishes an encoded H.264/H.265 access unit to all playing RTSP clients on this stream.
    pub fn publish_frame(&self, frame: EncodedFrame) {
        if let (Some(sps), Some(pps)) = (&frame.sps, &frame.pps) {
            let next_sets = VideoParamSets {
                vps: frame.vps.clone(),
                sps: sps.clone(),
                pps: pps.clone(),
            };
            let mut guard = self.entry.param_sets.write().expect("param_sets lock");
            if guard.as_ref() != Some(&next_sets) {
                *guard = Some(next_sets);
                self.entry.param_notify.notify_waiters();
            }
        }
        let _ = self.entry.frame_tx.send(Arc::new(frame));
    }

    /// Publishes an encoded G.711 audio packet to all playing RTSP clients subscribed to audio.
    pub fn publish_audio(&self, frame: EncodedAudioFrame) {
        let _ = self.entry.audio_tx.send(Arc::new(frame));
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
            name: config.stream_name,
            ..Default::default()
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
            let entry = StreamEntry::new(
                norm_path.clone(),
                spec.name,
                spec.video_codec,
                spec.audio_codec,
                spec.backchannel,
            );
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
            streams: Arc::new(RwLock::new(streams)),
            auth,
            next_session_id: Arc::new(AtomicU64::new(1000)),
            backchannel_sink: Arc::new(RwLock::new(None)),
        };

        Ok((Self { listener, state }, local_addr, handles))
    }

    /// Returns a dynamic registry handle allowing runtime registration/removal of RTSP streams.
    #[must_use]
    pub fn registry(&self) -> RtspRegistry {
        RtspRegistry {
            streams: Arc::clone(&self.state.streams),
            backchannel_sink: Arc::clone(&self.state.backchannel_sink),
        }
    }

    /// Spawns an RTSPS (TLS-encrypted RTSP) listener sharing all streams, auth, and backchannel state.
    pub fn spawn_tls_listener(
        &self,
        listener: TcpListener,
        tls_config: Arc<rustls::ServerConfig>,
    ) -> tokio::task::JoinHandle<()> {
        let state = self.state.clone();
        let acceptor = TlsAcceptor::from(tls_config);
        tokio::spawn(async move {
            let addr = listener.local_addr().ok();
            info!(?addr, "RTSPS (TLS) server listening");
            loop {
                match listener.accept().await {
                    Ok((tcp_stream, peer_addr)) => {
                        let local_ip = tcp_stream
                            .local_addr()
                            .map(|a| a.ip().to_string())
                            .unwrap_or_else(|_| "127.0.0.1".to_string());
                        let _ = tcp_stream.set_nodelay(true);
                        let acceptor = acceptor.clone();
                        let state = state.clone();
                        tokio::spawn(async move {
                            match acceptor.accept(tcp_stream).await {
                                Ok(tls_stream) => {
                                    if let Err(err) =
                                        handle_client(tls_stream, local_ip, peer_addr, state).await
                                    {
                                        debug!(%peer_addr, error = %err, "RTSPS client session ended");
                                    }
                                }
                                Err(err) => {
                                    debug!(%peer_addr, error = %err, "RTSPS TLS handshake failed");
                                }
                            }
                        });
                    }
                    Err(err) => {
                        warn!(error = %err, "RTSPS accept error");
                        break;
                    }
                }
            }
        })
    }

    /// Runs the RTSP accept loop until the task is cancelled or encounters a fatal accept error.
    pub async fn serve(self) -> Result<(), NvrDeskError> {
        let addr = self.listener.local_addr()?;
        let paths: Vec<String> = {
            let streams = self.state.streams.read().expect("streams read lock");
            let mut list: Vec<String> = streams.keys().cloned().collect();
            list.sort_unstable();
            list
        };
        info!(%addr, ?paths, auth_enabled = self.state.auth.is_some(), "RTSP server listening");
        loop {
            let (stream, peer_addr) = self.listener.accept().await?;
            let local_ip = stream
                .local_addr()
                .map(|a| a.ip().to_string())
                .unwrap_or_else(|_| "127.0.0.1".to_string());
            let _ = stream.set_nodelay(true);
            let state = self.state.clone();
            tokio::spawn(async move {
                if let Err(err) = handle_client(stream, local_ip, peer_addr, state).await {
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
    video_ssrc: u32,
    audio_ssrc: u32,
    nonce: String,
    stream: Option<Arc<StreamEntry>>,
    video_transport: Option<ClientTransport>,
    audio_transport: Option<ClientTransport>,
    backchannel_transport: Option<ClientTransport>,
    playing: bool,
    awaiting_keyframe: bool,
    _demand_guard: Option<DemandGuard>,
    _play_guard: Option<PlayGuard>,
}

async fn handle_client<S>(
    mut stream: S,
    local_ip: String,
    peer_addr: SocketAddr,
    state: ServerState,
) -> Result<(), NvrDeskError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let session_num = state.next_session_id.fetch_add(1, Ordering::Relaxed);
    let video_ssrc = (session_num as u32) ^ 0x4E56_5244;
    let audio_ssrc = video_ssrc ^ 0x00AA_5500;
    let mut session = SessionState {
        id: format!("{session_num:08X}"),
        video_ssrc,
        audio_ssrc,
        nonce: Uuid::new_v4().simple().to_string(),
        stream: None,
        video_transport: None,
        audio_transport: None,
        backchannel_transport: None,
        playing: false,
        awaiting_keyframe: true,
        _demand_guard: None,
        _play_guard: None,
    };

    let mut video_packetizer = H264RtpPacketizer::new(video_ssrc);
    let mut audio_packetizer = AudioRtpPacketizer::new(audio_ssrc);
    let mut frame_rx: Option<broadcast::Receiver<Arc<EncodedFrame>>> = None;
    let mut audio_rx: Option<broadcast::Receiver<Arc<EncodedAudioFrame>>> = None;
    let mut read_buf = BytesMut::with_capacity(8192);
    let mut udp_back_buf = [0u8; 2048];

    debug!(%peer_addr, session = %session.id, "RTSP client connected");

    loop {
        if session.playing {
            let has_video = session.video_transport.is_some();
            let has_audio = session.audio_transport.is_some();
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
                        &mut audio_rx,
                        &video_packetizer,
                    ).await? {
                        return Ok(());
                    }
                }
                recv_res = async {
                    if has_video && let Some(v_rx) = frame_rx.as_mut() {
                        v_rx.recv().await
                    } else {
                        std::future::pending().await
                    }
                } => {
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
                                &session.video_transport,
                                &mut video_packetizer,
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
                audio_res = async {
                    if has_audio && let Some(a_rx) = audio_rx.as_mut() {
                        a_rx.recv().await
                    } else {
                        std::future::pending().await
                    }
                } => {
                    if let Ok(audio_frame) = audio_res
                        && (!has_video || !session.awaiting_keyframe)
                    {
                        let pkt = audio_packetizer.packetize(&audio_frame);
                        send_single_rtp(&mut stream, &session.audio_transport, &pkt).await?;
                    }
                }
                udp_res = async {
                    if let Some(ClientTransport::Udp { socket, .. }) = &session.backchannel_transport {
                        socket.recv(&mut udp_back_buf).await
                    } else {
                        std::future::pending().await
                    }
                } => {
                    if let Ok(n) = udp_res {
                        dispatch_backchannel_rtp(&udp_back_buf[..n], &state.backchannel_sink);
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
                &mut audio_rx,
                &video_packetizer,
            )
            .await?
            {
                return Ok(());
            }
        }
    }
}

fn dispatch_backchannel_rtp(pkt: &[u8], sink_lock: &RwLock<Option<BackchannelSink>>) {
    if pkt.len() < 12 || (pkt[0] >> 6) != 2 {
        return;
    }
    let cc = usize::from(pkt[0] & 0x0F);
    let has_ext = (pkt[0] & 0x10) != 0;
    let mut hdr_len = 12 + cc * 4;
    if pkt.len() <= hdr_len {
        return;
    }
    if has_ext {
        if pkt.len() < hdr_len + 4 {
            return;
        }
        let ext_words = u16::from_be_bytes([pkt[hdr_len + 2], pkt[hdr_len + 3]]) as usize;
        hdr_len += 4 + ext_words * 4;
        if pkt.len() <= hdr_len {
            return;
        }
    }
    let pt = pkt[1] & 0x7F;
    if let Some(sink) = sink_lock.read().expect("backchannel sink lock").as_ref() {
        sink(pt, &pkt[hdr_len..]);
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_read_buffer<S>(
    read_buf: &mut BytesMut,
    stream: &mut S,
    peer_addr: SocketAddr,
    local_ip: &str,
    state: &ServerState,
    session: &mut SessionState,
    frame_rx: &mut Option<broadcast::Receiver<Arc<EncodedFrame>>>,
    audio_rx: &mut Option<broadcast::Receiver<Arc<EncodedAudioFrame>>>,
    packetizer: &H264RtpPacketizer,
) -> Result<bool, NvrDeskError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    loop {
        if read_buf.is_empty() {
            return Ok(true);
        }

        // Handle client-to-server interleaved RTP/RTCP packets (backchannel or RTCP RR).
        if read_buf[0] == b'$' {
            if read_buf.len() < 4 {
                return Ok(true);
            }
            let ch = read_buf[1];
            let payload_len = u16::from_be_bytes([read_buf[2], read_buf[3]]) as usize;
            if read_buf.len() < 4 + payload_len {
                return Ok(true);
            }
            if matches!(
                session.backchannel_transport,
                Some(ClientTransport::TcpInterleaved { rtp_channel }) if rtp_channel == ch
            ) {
                dispatch_backchannel_rtp(&read_buf[4..4 + payload_len], &state.backchannel_sink);
            }
            read_buf.advance(4 + payload_len);
            continue;
        }

        match Message::<Vec<u8>>::parse(read_buf.as_ref()) {
            Ok((msg, consumed)) => {
                read_buf.advance(consumed);
                if let Message::Request(req) = msg {
                    let keep_alive = handle_request(
                        req, stream, peer_addr, local_ip, state, session, frame_rx, audio_rx,
                        packetizer,
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
async fn handle_request<S>(
    req: Request<Vec<u8>>,
    stream: &mut S,
    peer_addr: SocketAddr,
    local_ip: &str,
    state: &ServerState,
    session: &mut SessionState,
    frame_rx: &mut Option<broadcast::Receiver<Arc<EncodedFrame>>>,
    audio_rx: &mut Option<broadcast::Receiver<Arc<EncodedAudioFrame>>>,
    packetizer: &H264RtpPacketizer,
) -> Result<bool, NvrDeskError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
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
            let Some((entry, _)) = state.resolve_stream_and_track(&uri_path) else {
                let resp = Response::builder(Version::V1_0, StatusCode::NotFound)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            };

            let req_backchannel = HeaderName::try_from("require")
                .ok()
                .and_then(|h| req.header(&h))
                .is_some_and(|v| v.as_str().contains("backchannel"));
            let backchannel = entry.current_backchannel() || req_backchannel;
            let (vps, sps, pps) = ensure_parameter_sets(&entry).await;
            let sdp = build_stream_sdp_full(
                local_ip,
                &entry.stream_name,
                entry.current_video_codec(),
                vps.as_deref(),
                sps.as_deref(),
                pps.as_deref(),
                entry.current_audio_codec(),
                backchannel,
            );
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
            let Some((entry, track_id)) = state.resolve_stream_and_track(&uri_path) else {
                let resp = Response::builder(Version::V1_0, StatusCode::NotFound)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            };

            let is_audio_track = track_id == Some(1);
            let is_backchannel_track = track_id == Some(2);
            if is_audio_track && entry.current_audio_codec().is_none() {
                let resp = Response::builder(Version::V1_0, StatusCode::NotFound)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            }
            if is_backchannel_track && !entry.current_backchannel() {
                let resp = Response::builder(Version::V1_0, StatusCode::NotFound)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            }

            let ssrc = if is_audio_track || is_backchannel_track {
                session.audio_ssrc
            } else {
                session.video_ssrc
            };
            let default_ch = if is_backchannel_track {
                (4, 5)
            } else if is_audio_track {
                (2, 3)
            } else {
                (0, 1)
            };
            let Some((client_transport, reply_header)) =
                parse_setup_transport(&req, peer_addr, ssrc, default_ch).await?
            else {
                let resp = Response::builder(Version::V1_0, StatusCode::UnsupportedTransport)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            };

            if is_backchannel_track {
                session.backchannel_transport = Some(client_transport);
            } else if is_audio_track {
                session.audio_transport = Some(client_transport);
            } else {
                session.video_transport = Some(client_transport);
            }
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
            if session.video_transport.is_none() && session.backchannel_transport.is_none() {
                let resp = Response::builder(Version::V1_0, StatusCode::MethodNotValidInThisState)
                    .header(headers::CSEQ, cseq)
                    .empty();
                write_rtsp_response(stream, &resp).await?;
                return Ok(true);
            }

            if !session.playing {
                session._demand_guard =
                    session.video_transport.as_ref().map(|_| entry.add_demand());
                let prev = entry.play_count.fetch_add(1, Ordering::AcqRel);
                session._play_guard = Some(PlayGuard {
                    count: Arc::clone(&entry.play_count),
                });
                session.playing = true;
                session.awaiting_keyframe = session.video_transport.is_some();
                if session.video_transport.is_some() {
                    entry.keyframe_requested.store(true, Ordering::Release);
                    *frame_rx = Some(entry.frame_tx.subscribe());
                }
                if session.audio_transport.is_some() {
                    *audio_rx = Some(entry.audio_tx.subscribe());
                }
                info!(
                    %peer_addr,
                    path = %entry.stream_path,
                    session = %session.id,
                    audio = session.audio_transport.is_some(),
                    backchannel = session.backchannel_transport.is_some(),
                    active_players = prev + 1,
                    "RTSP client started PLAY"
                );
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

async fn ensure_parameter_sets(
    entry: &StreamEntry,
) -> (Option<Bytes>, Option<Bytes>, Option<Bytes>) {
    if let Some(sets) = entry.param_sets.read().expect("param_sets lock").clone() {
        return (sets.vps, Some(sets.sps), Some(sets.pps));
    }

    let _guard = entry.add_demand();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    loop {
        if let Some(sets) = entry.param_sets.read().expect("param_sets lock").clone() {
            return (sets.vps, Some(sets.sps), Some(sets.pps));
        }
        if tokio::time::Instant::now() >= deadline {
            return (None, None, None);
        }
        let _ = tokio::time::timeout_at(deadline, entry.param_notify.notified()).await;
    }
}

async fn parse_setup_transport(
    req: &Request<Vec<u8>>,
    peer_addr: SocketAddr,
    ssrc: u32,
    default_ch: (u8, u8),
) -> Result<Option<(ClientTransport, String)>, NvrDeskError> {
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
                None => default_ch,
            };
            let reply =
                format!("RTP/AVP/TCP;unicast;interleaved={rtp_ch}-{rtcp_ch};ssrc={ssrc:08X}");
            return Ok(Some((
                ClientTransport::TcpInterleaved {
                    rtp_channel: rtp_ch,
                },
                reply,
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

            let reply = format!(
                "RTP/AVP;unicast;client_port={client_rtp_port}-{client_rtcp_port};server_port={server_rtp_port}-{server_rtcp_port};ssrc={ssrc:08X}"
            );
            return Ok(Some((
                ClientTransport::Udp {
                    socket: udp_socket,
                    rtp_dest,
                },
                reply,
            )));
        }
    }

    Ok(None)
}

async fn send_encoded_frame<S>(
    stream: &mut S,
    transport: &Option<ClientTransport>,
    packetizer: &mut H264RtpPacketizer,
    frame: &EncodedFrame,
) -> Result<(), NvrDeskError>
where
    S: AsyncWrite + Unpin,
{
    let packets = packetizer.packetize_frame(frame);
    if packets.is_empty() {
        return Ok(());
    }

    match transport {
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

async fn send_single_rtp<S>(
    stream: &mut S,
    transport: &Option<ClientTransport>,
    pkt: &[u8],
) -> Result<(), NvrDeskError>
where
    S: AsyncWrite + Unpin,
{
    match transport {
        Some(ClientTransport::TcpInterleaved { rtp_channel }) => {
            let mut frame = BytesMut::with_capacity(4 + pkt.len());
            frame.put_u8(b'$');
            frame.put_u8(*rtp_channel);
            frame.put_u16(pkt.len() as u16);
            frame.extend_from_slice(pkt);
            stream.write_all(&frame).await?;
        }
        Some(ClientTransport::Udp { socket, rtp_dest }) => {
            socket.send_to(pkt, *rtp_dest).await?;
        }
        None => {}
    }
    Ok(())
}

async fn write_rtsp_response<S, B>(stream: &mut S, resp: &Response<B>) -> Result<(), NvrDeskError>
where
    S: AsyncWrite + Unpin,
    B: AsRef<[u8]>,
{
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

    #[tokio::test]
    async fn registry_registers_and_removes_streams_at_runtime() {
        let (srv, _addr, _handles) = RtspServer::bind_streams(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            None,
            vec![RtspStreamSpec {
                path: "/stream1".into(),
                aliases: vec!["/stream".into()],
                name: "Main".into(),
                ..Default::default()
            }],
        )
        .await
        .expect("bind");

        let registry = srv.registry();
        assert!(srv.state.resolve_stream_and_track("/webcam").is_none());

        let cam = registry.add_or_update_stream(RtspStreamSpec {
            path: "/webcam".into(),
            name: "Webcam".into(),
            audio_codec: Some(AudioCodecKind::Pcmu),
            backchannel: true,
            ..Default::default()
        });
        assert!(srv.state.resolve_stream_and_track("/webcam").is_some());
        assert_eq!(cam.entry.current_audio_codec(), Some(AudioCodecKind::Pcmu));
        assert!(cam.entry.current_backchannel());

        registry.remove_stream("/webcam");
        assert!(srv.state.resolve_stream_and_track("/webcam").is_none());
    }
}
