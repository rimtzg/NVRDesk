//! Core domain types, video frame representations, and errors for NVRDesk.

use bytes::Bytes;
use thiserror::Error;

/// Supported raw pixel formats produced by capture sources and consumed by the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 32-bit BGRA (8 bits per channel, native on Windows/macOS capture APIs).
    Bgra8,
    /// 32-bit RGBA (8 bits per channel).
    Rgba8,
    /// 24-bit RGB (8 bits per channel).
    Rgb8,
}

impl PixelFormat {
    /// Returns the number of bytes per pixel for packed formats.
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Bgra8 | Self::Rgba8 => 4,
            Self::Rgb8 => 3,
        }
    }
}

/// Raw packed video frame produced by a capture source.
#[derive(Debug, Clone)]
pub struct RawVideoFrame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Row stride in bytes.
    pub stride: usize,
    /// Pixel format of `data`.
    pub format: PixelFormat,
    /// Raw pixel buffer.
    pub data: Vec<u8>,
    /// Capture timestamp in microseconds relative to stream start.
    pub timestamp_us: u64,
}

/// Planar YUV 4:2:0 (I420) frame with tightly packed planes ready for video encoding.
#[derive(Debug, Clone)]
pub struct I420Frame {
    /// Even frame width in pixels.
    pub width: u32,
    /// Even frame height in pixels.
    pub height: u32,
    /// Contiguous `[Y | U | V]` planar buffer.
    pub data: Vec<u8>,
    /// Capture timestamp in microseconds.
    pub timestamp_us: u64,
}

impl I420Frame {
    /// Allocates a zeroed I420 buffer for even dimensions `width` x `height`.
    pub fn new(width: u32, height: u32, timestamp_us: u64) -> Result<Self, NvrDeskError> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(NvrDeskError::InvalidDimensions { width, height });
        }
        let y_len = (width as usize) * (height as usize);
        let uv_len = y_len / 4;
        Ok(Self {
            width,
            height,
            data: vec![0; y_len + 2 * uv_len],
            timestamp_us,
        })
    }

    /// Returns the Y plane stride in bytes.
    #[must_use]
    pub const fn y_stride(&self) -> usize {
        self.width as usize
    }

    /// Returns the U/V plane stride in bytes.
    #[must_use]
    pub const fn uv_stride(&self) -> usize {
        (self.width as usize) / 2
    }

    /// Returns an immutable slice of the Y plane.
    #[must_use]
    pub fn y_plane(&self) -> &[u8] {
        let y_len = (self.width as usize) * (self.height as usize);
        &self.data[..y_len]
    }

    /// Returns an immutable slice of the U plane.
    #[must_use]
    pub fn u_plane(&self) -> &[u8] {
        let y_len = (self.width as usize) * (self.height as usize);
        let uv_len = y_len / 4;
        &self.data[y_len..y_len + uv_len]
    }

    /// Returns an immutable slice of the V plane.
    #[must_use]
    pub fn v_plane(&self) -> &[u8] {
        let y_len = (self.width as usize) * (self.height as usize);
        let uv_len = y_len / 4;
        &self.data[y_len + uv_len..]
    }

    /// Splits the underlying buffer into mutable `(Y, U, V)` plane slices.
    pub fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        let y_len = (self.width as usize) * (self.height as usize);
        let uv_len = y_len / 4;
        let (y, rest) = self.data.split_at_mut(y_len);
        let (u, v) = rest.split_at_mut(uv_len);
        (y, u, v)
    }
}

/// Encoded H.264 access unit consisting of Annex-B stripped NAL units.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// NAL units without Annex-B start codes (`0x000001` / `0x00000001`).
    pub nals: Vec<Bytes>,
    /// Extracted Sequence Parameter Set (NAL type 7), if present in this access unit.
    pub sps: Option<Bytes>,
    /// Extracted Picture Parameter Set (NAL type 8), if present in this access unit.
    pub pps: Option<Bytes>,
    /// Whether this access unit contains an IDR keyframe (NAL type 5).
    pub is_keyframe: bool,
    /// Presentation timestamp on a 90 kHz RTP clock.
    pub pts_90khz: u64,
}

/// Video stream configuration parameters.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Target frames per second.
    pub fps: u32,
    /// Target bitrate in bits per second.
    pub bitrate_bps: u32,
    /// Maximum output width (preserving aspect ratio).
    pub max_width: u32,
    /// Interval between forced IDR keyframes in seconds.
    pub idr_interval_secs: u32,
    /// Whether the OS cursor should be captured in screen sources.
    pub show_cursor: bool,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            fps: 15,
            bitrate_bps: 2_000_000,
            max_width: 1920,
            idr_interval_secs: 2,
            show_cursor: true,
        }
    }
}

/// Shared error type across NVRDesk crates.
#[derive(Debug, Error)]
pub enum NvrDeskError {
    /// Screen capture permission is missing on the host OS.
    #[error("capture permission denied: {0}")]
    PermissionDenied(String),

    /// Capture source is unsupported or failed to initialize.
    #[error("capture error: {0}")]
    Capture(String),

    /// Frame dimensions are invalid (zero or odd for I420).
    #[error("invalid frame dimensions: {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },

    /// Pipeline pixel conversion or resizing failed.
    #[error("pipeline error: {0}")]
    Pipeline(String),

    /// Video or audio encoder initialization or encoding failed.
    #[error("codec error: {0}")]
    Codec(String),

    /// RTSP/RTP transport or protocol failure.
    #[error("rtsp error: {0}")]
    Rtsp(String),

    /// Generic I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i420_frame_rejects_odd_dimensions() {
        assert!(I420Frame::new(1919, 1080, 0).is_err());
        assert!(I420Frame::new(0, 1080, 0).is_err());
        let ok = I420Frame::new(1280, 720, 42).expect("valid even dimensions");
        assert_eq!(ok.y_plane().len(), 1280 * 720);
        assert_eq!(ok.u_plane().len(), 1280 * 720 / 4);
        assert_eq!(ok.v_plane().len(), 1280 * 720 / 4);
    }
}
