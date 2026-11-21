//! RTSP 1.0 server, Digest auth, SDP builder, and RFC 6184/3551 RTP packetizers.

pub mod audio_rtp;
pub mod auth;
pub mod rtp;
pub mod sdp;
pub mod server;

pub use auth::RtspAuthConfig;
pub use rtp::VideoRtpPacketizer;
pub use sdp::{build_h264_sdp, build_stream_sdp, build_stream_sdp_full};
pub use server::{
    BackchannelSink, RtspRegistry, RtspServer, RtspServerConfig, RtspStreamHandle, RtspStreamSpec,
    normalize_stream_path,
};
