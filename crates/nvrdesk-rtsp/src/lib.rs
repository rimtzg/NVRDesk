//! RTSP 1.0 server, SDP builder, and RFC 6184 H.264 RTP packetizer.

pub mod rtp;
pub mod sdp;
pub mod server;

pub use server::{RtspServer, RtspServerConfig, RtspStreamHandle};
