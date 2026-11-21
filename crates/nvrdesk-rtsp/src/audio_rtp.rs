//! RFC 3551 RTP packetizer for G.711 PCMU/PCMA audio tracks.

use bytes::{BufMut, Bytes, BytesMut};
use nvrdesk_core::EncodedAudioFrame;

const RTP_HEADER_LEN: usize = 12;

/// Packetizes encoded G.711 audio frames into RTP packets.
pub struct AudioRtpPacketizer {
    sequence_number: u16,
    ssrc: u32,
}

impl AudioRtpPacketizer {
    /// Creates a new audio packetizer with `ssrc`.
    #[must_use]
    pub const fn new(ssrc: u32) -> Self {
        Self {
            sequence_number: 1,
            ssrc,
        }
    }

    /// Builds a single RTP packet for `frame`.
    pub fn packetize(&mut self, frame: &EncodedAudioFrame) -> Bytes {
        let seq = self.sequence_number;
        self.sequence_number = self.sequence_number.wrapping_add(1);

        let mut pkt = BytesMut::with_capacity(RTP_HEADER_LEN + frame.data.len());
        pkt.put_u8(0x80); // V=2, P=0, X=0, CC=0
        pkt.put_u8(frame.payload_type & 0x7F);
        pkt.put_u16(seq);
        pkt.put_u32(frame.pts_clock);
        pkt.put_u32(self.ssrc);
        pkt.extend_from_slice(&frame.data);
        pkt.freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_valid_g711_rtp_header() {
        let mut p = AudioRtpPacketizer::new(0xDEAD_BEEF);
        let frame = EncodedAudioFrame {
            data: Bytes::from_static(&[0xFF; 160]),
            pts_clock: 160,
            payload_type: 0,
        };
        let pkt = p.packetize(&frame);
        assert_eq!(pkt.len(), 12 + 160);
        assert_eq!(pkt[0] >> 6, 2);
        assert_eq!(pkt[1] & 0x7F, 0);
    }
}
