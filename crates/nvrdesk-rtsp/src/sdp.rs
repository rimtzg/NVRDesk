//! SDP generation for H.264 (+ optional G.711 audio) RTSP DESCRIBE responses.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use nvrdesk_core::AudioCodecKind;

/// Builds a standards-compliant SDP description for H.264 video only.
#[must_use]
pub fn build_h264_sdp(
    origin_ip: &str,
    stream_name: &str,
    sps: Option<&[u8]>,
    pps: Option<&[u8]>,
) -> String {
    build_stream_sdp(origin_ip, stream_name, sps, pps, None)
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

    let audio_media = match audio_codec {
        Some(AudioCodecKind::Pcmu) => {
            "m=audio 0 RTP/AVP 0\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=control:trackID=1\r\n"
        }
        Some(AudioCodecKind::Pcma) => {
            "m=audio 0 RTP/AVP 8\r\n\
             a=rtpmap:8 PCMA/8000\r\n\
             a=control:trackID=1\r\n"
        }
        None => "",
    };

    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {origin_ip}\r\n\
         s={stream_name}\r\n\
         c=IN IP4 {origin_ip}\r\n\
         t=0 0\r\n\
         a=control:*\r\n\
         m=video 0 RTP/AVP 96\r\n\
         a=rtpmap:96 H264/90000\r\n\
         {fmtp}\r\n\
         a=control:trackID=0\r\n\
         {audio_media}"
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
