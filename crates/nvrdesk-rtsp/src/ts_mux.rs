//! Minimal, zero-dependency MIT MPEG-2 Transport Stream (MPEG-TS) muxer for SRT delivery.
//!
//! Converts Annex-B H.264 / H.265 [`EncodedFrame`] access units into standard 188-byte TS
//! packets (PAT + PMT + PES) batched into 1316-byte (`7 * 188`) datagrams optimal for SRT.

use bytes::{BufMut, Bytes, BytesMut};
use nvrdesk_core::{EncodedFrame, VideoCodecKind};

/// Fixed size of an ISO/IEC 13818-1 MPEG-TS packet in bytes.
pub const TS_PACKET_SIZE: usize = 188;

/// Maximum SRT payload size carrying integer TS packets (`7 * 188 = 1316` bytes).
pub const SRT_TS_CHUNK_SIZE: usize = 7 * TS_PACKET_SIZE;

const PAT_PID: u16 = 0x0000;
const PMT_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x0100;
const VIDEO_STREAM_ID: u8 = 0xE0;

/// Stateful MPEG-TS packetizer maintaining continuity counters per PID.
#[derive(Debug, Default)]
pub struct TsMuxer {
    pat_cc: u8,
    pmt_cc: u8,
    video_cc: u8,
    sent_psi: bool,
    last_codec: Option<VideoCodecKind>,
}

impl TsMuxer {
    /// Creates a fresh MPEG-TS muxer instance.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resets PSI cadence on stream reconnect.
    pub fn reset(&mut self) {
        self.sent_psi = false;
    }

    /// Muxes one encoded H.264/H.265 frame into `<= 1316`-byte aligned MPEG-TS datagrams.
    #[must_use]
    pub fn mux_video_frame(&mut self, frame: &EncodedFrame) -> Vec<Bytes> {
        let mut ts_bytes = BytesMut::with_capacity(SRT_TS_CHUNK_SIZE * 4);

        let codec_changed = self.last_codec != Some(frame.codec);
        if !self.sent_psi || frame.is_keyframe || codec_changed {
            ts_bytes.extend_from_slice(&self.build_pat());
            ts_bytes.extend_from_slice(&self.build_pmt(frame.codec));
            self.sent_psi = true;
            self.last_codec = Some(frame.codec);
        }

        let pes = build_video_pes(frame);
        let total = pes.len();
        let mut offset = 0usize;
        let mut first_packet = true;

        while offset < total {
            let mut pkt = [0xFFu8; TS_PACKET_SIZE];
            pkt[0] = 0x47;
            let pid_hi = ((VIDEO_PID >> 8) as u8) & 0x1F;
            pkt[1] = if first_packet { pid_hi | 0x40 } else { pid_hi };
            pkt[2] = (VIDEO_PID & 0xFF) as u8;

            let cc = self.video_cc & 0x0F;
            self.video_cc = self.video_cc.wrapping_add(1) & 0x0F;

            // Emit PCR on the first packet of keyframes; use adaptation stuffing on short tail packets.
            let include_pcr = first_packet && frame.is_keyframe;
            let remaining = total - offset;
            let max_payload_plain = TS_PACKET_SIZE - 4;

            if include_pcr {
                // Adaptation field: length=7 (flags + 6-byte PCR) + extra stuffing if remaining < 176.
                let min_af = 8; // 1 byte AF_len + 1 byte flags + 6 byte PCR
                let avail_with_pcr = TS_PACKET_SIZE - 4 - min_af;
                let (stuffing, payload_len) = if remaining < avail_with_pcr {
                    (avail_with_pcr - remaining, remaining)
                } else {
                    (0, avail_with_pcr)
                };
                pkt[3] = 0x30 | cc; // adaptation_field + payload
                pkt[4] = (7 + stuffing) as u8;
                pkt[5] = 0x50; // random_access_indicator + PCR_flag
                encode_pcr(&mut pkt[6..12], frame.pts_90khz);
                // Bytes [12..12+stuffing] stay 0xFF.
                let payload_start = 4 + min_af + stuffing;
                pkt[payload_start..TS_PACKET_SIZE]
                    .copy_from_slice(&pes[offset..offset + payload_len]);
                offset += payload_len;
            } else if remaining < max_payload_plain {
                let af_bytes = max_payload_plain - remaining;
                pkt[3] = 0x30 | cc;
                pkt[4] = (af_bytes - 1) as u8;
                if af_bytes >= 2 {
                    pkt[5] = 0x00;
                }
                let payload_start = 4 + af_bytes;
                pkt[payload_start..TS_PACKET_SIZE]
                    .copy_from_slice(&pes[offset..offset + remaining]);
                offset += remaining;
            } else {
                pkt[3] = 0x10 | cc; // payload only
                pkt[4..TS_PACKET_SIZE].copy_from_slice(&pes[offset..offset + max_payload_plain]);
                offset += max_payload_plain;
            }

            first_packet = false;
            ts_bytes.extend_from_slice(&pkt);
        }

        chunk_ts_packets(ts_bytes.freeze())
    }

    fn build_pat(&mut self) -> [u8; TS_PACKET_SIZE] {
        let cc = self.pat_cc & 0x0F;
        self.pat_cc = self.pat_cc.wrapping_add(1) & 0x0F;

        // Pointer field (0x00) + table_id (0x00) + syntax/len + TSID + flags + section + program
        let mut section = [0u8; 12];
        section[0] = 0x00; // table_id = PAT
        section[1] = 0xB0; // section_syntax_indicator = 1
        section[2] = 13; // section_length = 9 + 4 CRC
        section[3] = 0x00;
        section[4] = 0x01; // transport_stream_id = 1
        section[5] = 0xC1; // version=0, current_next=1
        section[6] = 0x00; // section_number
        section[7] = 0x00; // last_section_number
        section[8] = 0x00;
        section[9] = 0x01; // program_number = 1
        section[10] = 0xE0 | ((PMT_PID >> 8) as u8 & 0x1F);
        section[11] = (PMT_PID & 0xFF) as u8;

        build_psi_packet(PAT_PID, cc, &section)
    }

    fn build_pmt(&mut self, codec: VideoCodecKind) -> [u8; TS_PACKET_SIZE] {
        let cc = self.pmt_cc & 0x0F;
        self.pmt_cc = self.pmt_cc.wrapping_add(1) & 0x0F;

        let stream_type = match codec {
            VideoCodecKind::H264 => 0x1B,
            VideoCodecKind::H265 => 0x24,
        };

        let mut section = [0u8; 17];
        section[0] = 0x02; // table_id = PMT
        section[1] = 0xB0;
        section[2] = 18; // section_length = 14 + 4 CRC
        section[3] = 0x00;
        section[4] = 0x01; // program_number = 1
        section[5] = 0xC1;
        section[6] = 0x00;
        section[7] = 0x00;
        section[8] = 0xE0 | ((VIDEO_PID >> 8) as u8 & 0x1F);
        section[9] = (VIDEO_PID & 0xFF) as u8; // PCR PID
        section[10] = 0xF0;
        section[11] = 0x00; // program_info_length = 0
        section[12] = stream_type;
        section[13] = 0xE0 | ((VIDEO_PID >> 8) as u8 & 0x1F);
        section[14] = (VIDEO_PID & 0xFF) as u8;
        section[15] = 0xF0;
        section[16] = 0x00; // ES_info_length = 0

        build_psi_packet(PMT_PID, cc, &section)
    }
}

fn chunk_ts_packets(buf: Bytes) -> Vec<Bytes> {
    let mut chunks = Vec::new();
    let mut off = 0usize;
    while off < buf.len() {
        let end = (off + SRT_TS_CHUNK_SIZE).min(buf.len());
        chunks.push(buf.slice(off..end));
        off = end;
    }
    chunks
}

fn build_psi_packet(pid: u16, cc: u8, section: &[u8]) -> [u8; TS_PACKET_SIZE] {
    let mut pkt = [0xFFu8; TS_PACKET_SIZE];
    pkt[0] = 0x47;
    pkt[1] = 0x40 | (((pid >> 8) as u8) & 0x1F);
    pkt[2] = (pid & 0xFF) as u8;
    pkt[3] = 0x10 | (cc & 0x0F);
    pkt[4] = 0x00; // pointer_field

    let end = 5 + section.len();
    pkt[5..end].copy_from_slice(section);
    let crc = mpeg2_crc32(section);
    pkt[end..end + 4].copy_from_slice(&crc.to_be_bytes());
    pkt
}

fn encode_pcr(dst: &mut [u8], pts_90khz: u64) {
    let base = pts_90khz & 0x1_FFFF_FFFF; // 33-bit 90 kHz base
    dst[0] = (base >> 25) as u8;
    dst[1] = (base >> 17) as u8;
    dst[2] = (base >> 9) as u8;
    dst[3] = (base >> 1) as u8;
    dst[4] = (((base & 1) as u8) << 7) | 0x7E;
    dst[5] = 0x00;
}

fn encode_pts(dst: &mut [u8], pts_90khz: u64) {
    let pts = pts_90khz & 0x1_FFFF_FFFF;
    dst[0] = 0x21 | ((((pts >> 30) & 0x07) as u8) << 1);
    dst[1] = ((pts >> 22) & 0xFF) as u8;
    dst[2] = 0x01 | ((((pts >> 15) & 0x7F) as u8) << 1);
    dst[3] = ((pts >> 7) & 0xFF) as u8;
    dst[4] = 0x01 | (((pts & 0x7F) as u8) << 1);
}

fn build_video_pes(frame: &EncodedFrame) -> BytesMut {
    let mut body_len = 0usize;
    if frame.is_keyframe {
        if let Some(vps) = &frame.vps {
            body_len += 4 + vps.len();
        }
        if let Some(sps) = &frame.sps {
            body_len += 4 + sps.len();
        }
        if let Some(pps) = &frame.pps {
            body_len += 4 + pps.len();
        }
    }
    for nal in &frame.nals {
        body_len += 4 + nal.len();
    }

    let mut out = BytesMut::with_capacity(14 + body_len);
    // PES start code prefix (0x000001) + video stream_id (0xE0)
    out.put_u8(0x00);
    out.put_u8(0x00);
    out.put_u8(0x01);
    out.put_u8(VIDEO_STREAM_ID);
    // PES_packet_length = 0 (unbounded video elementary stream allowed in MPEG-TS)
    out.put_u16(0);
    out.put_u8(0x80); // '10' marker bits
    out.put_u8(0x80); // PTS_DTS_flags = 10 (PTS present)
    out.put_u8(5); // PES_header_data_length
    let mut pts_buf = [0u8; 5];
    encode_pts(&mut pts_buf, frame.pts_90khz);
    out.extend_from_slice(&pts_buf);

    if frame.is_keyframe {
        if let Some(vps) = &frame.vps {
            push_annex_b(&mut out, vps);
        }
        if let Some(sps) = &frame.sps {
            push_annex_b(&mut out, sps);
        }
        if let Some(pps) = &frame.pps {
            push_annex_b(&mut out, pps);
        }
    }
    for nal in &frame.nals {
        push_annex_b(&mut out, nal);
    }

    out
}

fn push_annex_b(out: &mut BytesMut, nal: &[u8]) {
    out.put_slice(&[0x00, 0x00, 0x00, 0x01]);
    out.put_slice(nal);
}

/// Computes ISO/IEC 13818-1 CRC-32/MPEG-2 over `data`.
#[must_use]
pub fn mpeg2_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            if (crc & 0x8000_0000) != 0 {
                crc = (crc << 1) ^ 0x04C1_1DB7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muxes_keyframe_and_delta_into_188_aligned_chunks() {
        let mut muxer = TsMuxer::new();
        let key = EncodedFrame {
            codec: VideoCodecKind::H264,
            nals: vec![Bytes::from_static(&[0x65, 0x88, 0x84, 0x21])],
            vps: None,
            sps: Some(Bytes::from_static(&[0x67, 0x42, 0xE0, 0x1E])),
            pps: Some(Bytes::from_static(&[0x68, 0xCE, 0x38, 0x80])),
            is_keyframe: true,
            pts_90khz: 90_000,
        };

        let chunks = muxer.mux_video_frame(&key);
        assert!(!chunks.is_empty());
        let total_bytes: usize = chunks.iter().map(Bytes::len).sum();
        assert_eq!(total_bytes % TS_PACKET_SIZE, 0);
        // Should contain at least PAT + PMT + 1 PES TS packet = 3 packets = 564 bytes.
        assert!(total_bytes >= 3 * TS_PACKET_SIZE);
        for chunk in &chunks {
            assert!(chunk.len() <= SRT_TS_CHUNK_SIZE);
            assert_eq!(chunk.len() % TS_PACKET_SIZE, 0);
            for pkt in chunk.as_chunks::<TS_PACKET_SIZE>().0 {
                assert_eq!(pkt[0], 0x47);
            }
        }

        // Verify PAT table CRC-32 covers section bytes and equals zero over (section + crc).
        let first_pkt = &chunks[0][0..TS_PACKET_SIZE];
        let pat_pid = u16::from_be_bytes([first_pkt[1] & 0x1F, first_pkt[2]]);
        assert_eq!(pat_pid, PAT_PID);
        let section_with_crc = &first_pkt[5..5 + 12 + 4];
        assert_eq!(mpeg2_crc32(section_with_crc), 0);

        // Large P-frame spanning >1500 bytes splits across <= 1316-byte SRT chunks.
        let p_frame = EncodedFrame {
            codec: VideoCodecKind::H265,
            nals: vec![Bytes::from(vec![0x02; 2200])],
            vps: None,
            sps: None,
            pps: None,
            is_keyframe: false,
            pts_90khz: 96_000,
        };
        let p_chunks = muxer.mux_video_frame(&p_frame);
        assert!(p_chunks.len() >= 2);
        for chunk in p_chunks {
            assert!(chunk.len() <= SRT_TS_CHUNK_SIZE);
            assert_eq!(chunk.len() % TS_PACKET_SIZE, 0);
        }
    }
}
