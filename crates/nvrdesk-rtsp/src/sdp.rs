//! SDP generation for H.264 RTSP DESCRIBE responses.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

/// Builds a standards-compliant SDP description for an H.264 video stream.
#[must_use]
pub fn build_h264_sdp(
    origin_ip: &str,
    stream_name: &str,
    sps: Option<&[u8]>,
    pps: Option<&[u8]>,
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
         a=control:trackID=0\r\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sdp_with_sprop_parameter_sets() {
        let sps = [0x67, 0x42, 0xC0, 0x1F, 0xDA];
        let pps = [0x68, 0xCE, 0x3C, 0x80];
        let sdp = build_h264_sdp("127.0.0.1", "NVRDesk", Some(&sps), Some(&pps));
        assert!(sdp.contains("profile-level-id=42C01F"));
        assert!(sdp.contains("sprop-parameter-sets="));
        assert!(sdp.contains("a=control:trackID=0"));
    }
}
