//! Video and audio encoders for NVRDesk (MIT-compatible OpenH264, G.711, and optional FFmpeg).

pub mod audio;
pub mod ffmpeg;

use bytes::Bytes;
use nvrdesk_core::{
    EncodedFrame, EncoderBackendKind, EncoderConfig, I420Frame, NvrDeskError, StreamConfig,
};
use openh264::{
    OpenH264API,
    encoder::{
        BitRate, Complexity, Encoder, EncoderConfig as OpenH264Config, FrameRate, FrameType,
        IntraFramePeriod, RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
    },
    formats::YUVSource,
};
use tracing::info;

use crate::ffmpeg::{FfmpegCliEncoder, probe_hw_h264_codec};

/// Abstraction over a video encoder producing Annex-B stripped H.264/H.265 access units.
pub trait VideoEncoder: Send {
    /// Human-readable encoder name (e.g. `"openh264"` or `"h264_videotoolbox"`).
    fn name(&self) -> &str;

    /// Encodes one planar I420 frame.
    fn encode(
        &mut self,
        frame: &I420Frame,
        force_keyframe: bool,
    ) -> Result<EncodedFrame, NvrDeskError>;

    /// Returns the most recently observed SPS and PPS NAL units, if available.
    fn parameter_sets(&self) -> (Option<Bytes>, Option<Bytes>);
}

/// Creates a video encoder matching `enc_cfg` with automatic fallback to `OpenH264Encoder`.
pub fn create_video_encoder(
    stream_cfg: &StreamConfig,
    enc_cfg: &EncoderConfig,
) -> Result<Box<dyn VideoEncoder>, NvrDeskError> {
    match enc_cfg.backend {
        EncoderBackendKind::Openh264 => Ok(Box::new(OpenH264Encoder::new(stream_cfg)?)),
        EncoderBackendKind::Ffmpeg => {
            let codec = enc_cfg
                .ffmpeg_codec
                .clone()
                .or_else(|| probe_hw_h264_codec(&enc_cfg.ffmpeg_path).map(str::to_string))
                .unwrap_or_else(|| "libx264".to_string());
            info!(codec = %codec, "initializing external FFmpeg video encoder");
            Ok(Box::new(FfmpegCliEncoder::new(
                enc_cfg.ffmpeg_path.clone(),
                codec,
                stream_cfg,
            )))
        }
        EncoderBackendKind::Auto => {
            if let Some(hw_codec) = enc_cfg
                .ffmpeg_codec
                .as_deref()
                .or_else(|| probe_hw_h264_codec(&enc_cfg.ffmpeg_path))
            {
                info!(codec = %hw_codec, "using hardware-accelerated FFmpeg video encoder");
                return Ok(Box::new(FfmpegCliEncoder::new(
                    enc_cfg.ffmpeg_path.clone(),
                    hw_codec.to_string(),
                    stream_cfg,
                )));
            }
            Ok(Box::new(OpenH264Encoder::new(stream_cfg)?))
        }
    }
}

struct I420Adapter<'a>(&'a I420Frame);

impl YUVSource for I420Adapter<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.0.width as usize, self.0.height as usize)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.0.y_stride(), self.0.uv_stride(), self.0.uv_stride())
    }

    fn y(&self) -> &[u8] {
        self.0.y_plane()
    }

    fn u(&self) -> &[u8] {
        self.0.u_plane()
    }

    fn v(&self) -> &[u8] {
        self.0.v_plane()
    }
}

/// Software H.264 encoder powered by Cisco's OpenH264 (BSD-2-Clause).
pub struct OpenH264Encoder {
    encoder: Encoder,
    cached_sps: Option<Bytes>,
    cached_pps: Option<Bytes>,
}

impl OpenH264Encoder {
    /// Creates a new OpenH264 encoder tuned for real-time screen/POS content.
    pub fn new(config: &StreamConfig) -> Result<Self, NvrDeskError> {
        let fps = config.fps.max(1);
        let idr_frames = fps.saturating_mul(config.idr_interval_secs.max(1));

        let enc_cfg = OpenH264Config::new()
            .bitrate(BitRate::from_bps(config.bitrate_bps.max(100_000)))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .complexity(Complexity::Low)
            .adaptive_quantization(false)
            .background_detection(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(idr_frames))
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .skip_frames(true)
            .vui(VuiConfig::bt709());

        let encoder = Encoder::with_api_config(OpenH264API::from_source(), enc_cfg)
            .map_err(|e| NvrDeskError::Codec(format!("failed to initialize OpenH264: {e}")))?;

        Ok(Self {
            encoder,
            cached_sps: None,
            cached_pps: None,
        })
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn name(&self) -> &str {
        "openh264"
    }

    fn encode(
        &mut self,
        frame: &I420Frame,
        force_keyframe: bool,
    ) -> Result<EncodedFrame, NvrDeskError> {
        if force_keyframe {
            self.encoder.force_intra_frame();
        }

        let bitstream = self
            .encoder
            .encode(&I420Adapter(frame))
            .map_err(|e| NvrDeskError::Codec(format!("OpenH264 encode error: {e}")))?;

        let frame_type = bitstream.frame_type();
        let mut is_keyframe = matches!(frame_type, FrameType::IDR | FrameType::I);
        let mut nals = Vec::new();

        for layer_idx in 0..bitstream.num_layers() {
            let Some(layer) = bitstream.layer(layer_idx) else {
                continue;
            };
            for nal_idx in 0..layer.nal_count() {
                let Some(raw_nal) = layer.nal_unit(nal_idx) else {
                    continue;
                };
                for stripped in split_annex_b_nals(raw_nal) {
                    if stripped.is_empty() {
                        continue;
                    }
                    let nal_type = stripped[0] & 0x1F;
                    let nal_bytes = Bytes::copy_from_slice(stripped);
                    match nal_type {
                        7 => self.cached_sps = Some(nal_bytes.clone()),
                        8 => self.cached_pps = Some(nal_bytes.clone()),
                        5 => is_keyframe = true,
                        _ => {}
                    }
                    nals.push(nal_bytes);
                }
            }
        }

        let pts_90khz = frame.timestamp_us.saturating_mul(90) / 1_000;

        Ok(EncodedFrame {
            nals,
            sps: self.cached_sps.clone(),
            pps: self.cached_pps.clone(),
            is_keyframe,
            pts_90khz,
        })
    }

    fn parameter_sets(&self) -> (Option<Bytes>, Option<Bytes>) {
        (self.cached_sps.clone(), self.cached_pps.clone())
    }
}

/// Splits an Annex-B byte slice into individual NAL slices without start codes.
#[must_use]
pub fn split_annex_b_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    let mut nal_start: Option<usize> = None;

    while i < data.len() {
        let sc_len = if i + 4 <= data.len() && data[i..i + 4] == [0, 0, 0, 1] {
            4
        } else if i + 3 <= data.len() && data[i..i + 3] == [0, 0, 1] {
            3
        } else {
            0
        };

        if sc_len > 0 {
            if let Some(start) = nal_start
                && start < i
            {
                nals.push(&data[start..i]);
            }
            i += sc_len;
            nal_start = Some(i);
        } else {
            i += 1;
        }
    }

    if let Some(start) = nal_start
        && start < data.len()
    {
        nals.push(&data[start..]);
    } else if !data.is_empty() {
        nals.push(data);
    }

    nals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_i420_frame_with_sps_pps_and_idr() {
        let cfg = StreamConfig {
            fps: 15,
            bitrate_bps: 800_000,
            max_width: 640,
            idr_interval_secs: 2,
            show_cursor: true,
        };
        let mut enc = OpenH264Encoder::new(&cfg).expect("encoder init");
        let frame = I420Frame::new(320, 240, 0).expect("frame");
        let encoded = enc.encode(&frame, true).expect("encode first frame");
        assert!(encoded.is_keyframe);
        assert!(encoded.sps.is_some(), "expected SPS on first IDR frame");
        assert!(encoded.pps.is_some(), "expected PPS on first IDR frame");
        assert!(!encoded.nals.is_empty());
    }

    #[test]
    fn splits_annex_b_start_codes() {
        let buf = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x65, 0x88,
        ];
        let nals = split_annex_b_nals(&buf);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67, 0x42]);
        assert_eq!(nals[1], &[0x68, 0xCE]);
        assert_eq!(nals[2], &[0x65, 0x88]);
    }
}
