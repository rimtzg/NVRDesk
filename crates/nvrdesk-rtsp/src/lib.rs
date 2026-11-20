//! RTSP 1.0 server, Digest auth, SDP builder, and RFC 6184 H.264 RTP packetizer.

pub mod auth;
pub mod rtp;
pub mod sdp;
pub mod server;

pub use auth::RtspAuthConfig;
pub use server::{
    RtspServer, RtspServerConfig, RtspStreamHandle, RtspStreamSpec, normalize_stream_path,
};
