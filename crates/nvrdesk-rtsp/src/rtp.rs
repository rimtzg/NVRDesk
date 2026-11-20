//! RFC 6184 H.264 RTP packetizer.

use bytes::{BufMut, Bytes, BytesMut};
use nvrdesk_core::EncodedFrame;

const RTP_HEADER_LEN: usize = 12;
const DEFAULT_MTU: usize = 1400;

/// Packetizes H.264 Annex-B-stripped NAL units into RFC 6184 RTP packets.
pub struct H264RtpPacketizer {
    payload_type: u8,
    sequence_number: u16,
    ssrc: u32,
    mtu: usize,
}

impl H264RtpPacketizer {
    /// Creates a new packetizer with the given SSRC and default 1400-byte MTU.
    #[must_use]
    pub const fn new(ssrc: u32) -> Self {
        Self {
            payload_type: 96,
            sequence_number: 1,
            ssrc,
            mtu: DEFAULT_MTU,
        }
    }

    /// Returns the next sequence number that will be assigned.
    #[must_use]
    pub const fn next_sequence_number(&self) -> u16 {
        self.sequence_number
    }

    /// Packetizes all NAL units of an `EncodedFrame` into one or more RTP packets.
    pub fn packetize_frame(&mut self, frame: &EncodedFrame) -> Vec<Bytes> {
        let mut ordered_nals: Vec<&[u8]> = Vec::with_capacity(frame.nals.len() + 2);

        if frame.is_keyframe {
            let has_sps = frame
                .nals
                .iter()
                .any(|n| !n.is_empty() && (n[0] & 0x1F) == 7);
            let has_pps = frame
                .nals
                .iter()
                .any(|n| !n.is_empty() && (n[0] & 0x1F) == 8);
            if !has_sps && let Some(sps) = &frame.sps {
                ordered_nals.push(sps.as_ref());
            }
            if !has_pps && let Some(pps) = &frame.pps {
                ordered_nals.push(pps.as_ref());
            }
        }

        for nal in &frame.nals {
            if !nal.is_empty() {
                ordered_nals.push(nal.as_ref());
            }
        }

        let total_nals = ordered_nals.len();
        let mut packets = Vec::new();
        let max_payload = self.mtu.saturating_sub(RTP_HEADER_LEN).max(64);
        let ts = frame.pts_90khz as u32;

        for (idx, nal) in ordered_nals.into_iter().enumerate() {
            let is_last_nal = idx + 1 == total_nals;
            if nal.len() <= max_payload {
                packets.push(self.build_single_nal_packet(nal, ts, is_last_nal));
            } else {
                self.push_fua_packets(nal, ts, is_last_nal, max_payload, &mut packets);
            }
        }

        packets
    }

    fn build_single_nal_packet(&mut self, nal: &[u8], ts: u32, marker: bool) -> Bytes {
        let mut pkt = BytesMut::with_capacity(RTP_HEADER_LEN + nal.len());
        self.write_rtp_header(&mut pkt, ts, marker);
        pkt.extend_from_slice(nal);
        pkt.freeze()
    }

    fn push_fua_packets(
        &mut self,
        nal: &[u8],
        ts: u32,
        is_last_nal: bool,
        max_payload: usize,
        out: &mut Vec<Bytes>,
    ) {
        let nri = nal[0] & 0x60;
        let nal_type = nal[0] & 0x1F;
        let fu_indicator = nri | 28; // FU-A type = 28
        let payload = &nal[1..];
        let max_frag = max_payload.saturating_sub(2).max(1);

        let total_frags = payload.len().div_ceil(max_frag);
        for (frag_idx, chunk) in payload.chunks(max_frag).enumerate() {
            let is_start = frag_idx == 0;
            let is_end = frag_idx + 1 == total_frags;
            let mut fu_header = nal_type;
            if is_start {
                fu_header |= 0x80; // S bit
            }
            if is_end {
                fu_header |= 0x40; // E bit
            }
            let marker = is_last_nal && is_end;

            let mut pkt = BytesMut::with_capacity(RTP_HEADER_LEN + 2 + chunk.len());
            self.write_rtp_header(&mut pkt, ts, marker);
            pkt.put_u8(fu_indicator);
            pkt.put_u8(fu_header);
            pkt.extend_from_slice(chunk);
            out.push(pkt.freeze());
        }
    }

    fn write_rtp_header(&mut self, buf: &mut BytesMut, ts: u32, marker: bool) {
        let seq = self.sequence_number;
        self.sequence_number = self.sequence_number.wrapping_add(1);

        buf.put_u8(0x80); // V=2, P=0, X=0, CC=0
        let pt_byte = if marker {
            0x80 | (self.payload_type & 0x7F)
        } else {
            self.payload_type & 0x7F
        };
        buf.put_u8(pt_byte);
        buf.put_u16(seq);
        buf.put_u32(ts);
        buf.put_u32(self.ssrc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packetizes_small_nal_as_single_rtp_packet() {
        let mut p = H264RtpPacketizer::new(0x1234_5678);
        let frame = EncodedFrame {
            nals: vec![Bytes::from_static(&[0x65, 0x01, 0x02, 0x03])],
            sps: Some(Bytes::from_static(&[0x67, 0x42, 0x00, 0x1E])),
            pps: Some(Bytes::from_static(&[0x68, 0xCE, 0x3C, 0x80])),
            is_keyframe: true,
            pts_90khz: 9000,
        };
        let pkts = p.packetize_frame(&frame);
        // SPS + PPS + IDR = 3 packets; only last has marker bit set.
        assert_eq!(pkts.len(), 3);
        assert_eq!(pkts[0][1] & 0x80, 0);
        assert_eq!(pkts[1][1] & 0x80, 0);
        assert_eq!(pkts[2][1] & 0x80, 0x80);
        assert_eq!(pkts[0][12], 0x67);
        assert_eq!(pkts[1][12], 0x68);
        assert_eq!(pkts[2][12], 0x65);
    }

    #[test]
    fn fragments_large_nal_with_fua() {
        let mut p = H264RtpPacketizer::new(0xCAFE_BABE);
        let mut big = vec![0x65];
        big.extend(std::iter::repeat_n(0xAB, 3500));
        let frame = EncodedFrame {
            nals: vec![Bytes::from(big)],
            sps: None,
            pps: None,
            is_keyframe: false,
            pts_90khz: 18000,
        };
        let pkts = p.packetize_frame(&frame);
        assert!(pkts.len() >= 3);
        // First fragment has FU-A S bit.
        assert_eq!(pkts[0][12] & 0x1F, 28);
        assert_eq!(pkts[0][13] & 0x80, 0x80);
        assert_eq!(pkts[0][13] & 0x40, 0);
        assert_eq!(pkts[0][1] & 0x80, 0);
        // Last fragment has FU-A E bit and RTP marker bit.
        let last = pkts.last().expect("non-empty");
        assert_eq!(last[13] & 0x40, 0x40);
        assert_eq!(last[1] & 0x80, 0x80);
    }
}
