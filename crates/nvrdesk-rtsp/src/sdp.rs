//! SDP generation for H.264 / H.265 (+ optional G.711 audio & ONVIF backchannel) RTSP DESCRIBE responses.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use nvrdesk_core::{AudioCodecKind, VideoCodecKind};

/// Builds a standards-compliant SDP description for H.264 video only.
#[must_use]
pub fn build_h264_sdp(
    origin_ip: &str,
    stream_name: &str,
    sps: Option<&[u8]>,
    pps: Option<&[u8]>,
) -> String {
    build_stream_sdp_full(
        origin_ip,
        stream_name,
        VideoCodecKind::H264,
        None,
        sps,
        pps,
        None,
        false,
    )
}

/// Builds a standards-compliant SDP description for H.264 video and optional G.711 audio.
#[must_use]
pub fn build_stream_sdp(
    origin_ip: &str,
    stream_name: &str,
    sps: Option<&[u8]>,
    pps: Option<&[u8]>,
    audio_codec: Option<AudioCodecKind>,
) -> String {
    build_stream_sdp_full(
        origin_ip,
        stream_name,
        VideoCodecKind::H264,
        None,
        sps,
        pps,
        audio_codec,
        false,
    )
}

/// Builds a full SDP description supporting H.264/H.265 video, microphone audio (`trackID=1`), and ONVIF backchannel (`trackID=2`).
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn build_stream_sdp_full(
    origin_ip: &str,
    stream_name: &str,
    video_codec: VideoCodecKind,
    vps: Option<&[u8]>,
    sps: Option<&[u8]>,
    pps: Option<&[u8]>,
    audio_codec: Option<AudioCodecKind>,
    backchannel: bool,
) -> String {
    let (rtpmap_video, fmtp) = match video_codec {
        VideoCodecKind::H264 => {
            let fmtp = match (sps, pps) {
                (Some(s), Some(p)) if s.len() >= 4 => {
                    let profile_level_id = format!("{:02X}{:02X}{:02X}", s[1], s[2], s[3]);
                    let sps_b64 = BASE64.encode(s);
                    let pps_b64 = BASE64.encode(p);
                    format!(
                        "a=fmtp:96 packetization-mode=1;profile-level-id={profile_level_id};sprop-parameter-sets={sps_b64},{pps_b64}"
                    )
                }
                _ => "a=fmtp:96 packetization-mode=1".to_string(),
            };
            ("a=rtpmap:96 H264/90000", fmtp)
        }
        VideoCodecKind::H265 => {
            let mut parts = Vec::new();
            if let Some(v) = vps {
                parts.push(format!("sprop-vps={}", BASE64.encode(v)));
            }
            if let Some(s) = sps {
                parts.push(format!("sprop-sps={}", BASE64.encode(s)));
            }
            if let Some(p) = pps {
                parts.push(format!("sprop-pps={}", BASE64.encode(p)));
            }
            let fmtp = if parts.is_empty() {
                "a=fmtp:96".to_string()
            } else {
                format!("a=fmtp:96 {}", parts.join(";"))
            };
            ("a=rtpmap:96 H265/90000", fmtp)
        }
    };

    let audio_media = match audio_codec {
        Some(AudioCodecKind::Pcmu) => {
            "m=audio 0 RTP/AVP 0\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=sendonly\r\n\
             a=control:trackID=1\r\n"
        }
        Some(AudioCodecKind::Pcma) => {
            "m=audio 0 RTP/AVP 8\r\n\
             a=rtpmap:8 PCMA/8000\r\n\
             a=sendonly\r\n\
             a=control:trackID=1\r\n"
        }
        None => "",
    };

    let backchannel_media = if backchannel {
        "m=audio 0 RTP/AVP 0\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:8 PCMA/8000\r\n\
         a=recvonly\r\n\
         a=control:trackID=2\r\n"
    } else {
        ""
    };

    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {origin_ip}\r\n\
         s={stream_name}\r\n\
         c=IN IP4 {origin_ip}\r\n\
         t=0 0\r\n\
         a=control:*\r\n\
         m=video 0 RTP/AVP 96\r\n\
         {rtpmap_video}\r\n\
         {fmtp}\r\n\
         a=control:trackID=0\r\n\
         {audio_media}\
         {backchannel_media}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sdp_with_sprop_and_optional_pcmu_audio() {
        let sps = [0x67, 0x42, 0xC0, 0x1F, 0xDA];
        let pps = [0x68, 0xCE, 0x3C, 0x80];
        let sdp = build_stream_sdp(
            "127.0.0.1",
            "NVRDesk",
            Some(&sps),
            Some(&pps),
            Some(AudioCodecKind::Pcmu),
        );
        assert!(sdp.contains("profile-level-id=42C01F"));
        assert!(sdp.contains("a=control:trackID=0"));
        assert!(sdp.contains("m=audio 0 RTP/AVP 0"));
        assert!(sdp.contains("a=rtpmap:0 PCMU/8000"));
        assert!(sdp.contains("a=control:trackID=1"));
    }
}
