//! Core domain types, configuration schema, input overlay snapshots, and errors for NVRDesk.

use std::path::Path;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
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

/// Video stream encoding configuration parameters.
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

/// Virtual desktop bounds of a captured monitor in OS global coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorBounds {
    /// Left coordinate in OS global display coordinates.
    pub x: i32,
    /// Top coordinate in OS global display coordinates.
    pub y: i32,
    /// Logical width in OS display coordinates.
    pub width: u32,
    /// Logical height in OS display coordinates.
    pub height: u32,
}

/// Mouse button kind for click visualization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Active animated mouse-click ripple normalized to `[0.0, 1.0]` within the captured monitor.
#[derive(Debug, Clone, Copy)]
pub struct ClickRipple {
    /// Normalized X coordinate in `[0.0, 1.0]`.
    pub norm_x: f32,
    /// Normalized Y coordinate in `[0.0, 1.0]`.
    pub norm_y: f32,
    /// Button pressed.
    pub button: MouseButton,
    /// Animation progress in `[0.0, 1.0]` where `0.0` is just clicked and `1.0` is expired.
    pub progress: f32,
}

/// Point-in-time overlay state rendered onto MainStream and SubStream frames.
#[derive(Debug, Clone, Default)]
pub struct InputOverlaySnapshot {
    /// Normalized cursor position `(x, y)` in `[0.0, 1.0]` when inside the captured monitor.
    pub cursor_norm: Option<(f32, f32)>,
    /// Whether to draw a subtle cursor highlight ring around `cursor_norm`.
    pub show_cursor_highlight: bool,
    /// Active mouse click animations.
    pub clicks: Vec<ClickRipple>,
    /// Formatted recent keystrokes for the OSD bar (empty when none active).
    pub keystroke_text: String,
    /// Whether keystroke recording is currently paused due to a privacy-excluded window.
    pub privacy_paused: bool,
}

/// Keystroke display and POS privacy mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeystrokeMode {
    /// Show all typed keys and shortcuts.
    #[default]
    All,
    /// Show only shortcuts (`Ctrl+...`, `Alt+...`, `Cmd+...`) and special keys (`ENTER`, `F1..F12`, `ESC`).
    ShortcutsAndSpecial,
    /// Show keys but mask long consecutive digit sequences (e.g. card numbers / PINs).
    MaskedDigits,
    /// Disable keystroke OSD recording.
    Off,
}

/// Video capture source kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSourceKind {
    /// Live OS monitor capture.
    #[default]
    Screen,
    /// Synthetic animated SMPTE-style test pattern (for headless testing).
    TestPattern,
}

/// Source configuration in `nvrdesk.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceConfig {
    pub kind: CaptureSourceKind,
    pub display: usize,
    pub show_cursor: bool,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            kind: CaptureSourceKind::Screen,
            display: 0,
            show_cursor: true,
        }
    }
}

/// Profile configuration for MainStream or SubStream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamProfileConfig {
    pub enabled: bool,
    pub path: String,
    pub max_width: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub idr_interval_secs: u32,
}

impl Default for StreamProfileConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "/stream1".to_string(),
            max_width: 1920,
            fps: 15,
            bitrate_bps: 2_000_000,
            idr_interval_secs: 2,
        }
    }
}

impl StreamProfileConfig {
    /// Converts this profile into a codec `StreamConfig`.
    #[must_use]
    pub fn to_stream_config(&self, show_cursor: bool) -> StreamConfig {
        StreamConfig {
            fps: self.fps.max(1),
            bitrate_bps: self.bitrate_bps.max(100_000),
            max_width: self.max_width,
            idr_interval_secs: self.idr_interval_secs.max(1),
            show_cursor,
        }
    }
}

/// Input capture and visual overlay configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InputOverlayConfig {
    pub enabled: bool,
    pub show_clicks: bool,
    pub show_cursor_highlight: bool,
    pub show_keystrokes: bool,
    pub keystroke_mode: KeystrokeMode,
    pub keystroke_ttl_ms: u64,
    pub click_ttl_ms: u64,
    pub excluded_windows: Vec<String>,
}

impl Default for InputOverlayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            show_clicks: true,
            show_cursor_highlight: false,
            show_keystrokes: true,
            keystroke_mode: KeystrokeMode::MaskedDigits,
            keystroke_ttl_ms: 4000,
            click_ttl_ms: 400,
            excluded_windows: vec![
                "1Password".to_string(),
                "Bitwarden".to_string(),
                "KeePass".to_string(),
            ],
        }
    }
}

/// RTSP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RtspConfig {
    pub bind: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl Default for RtspConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".to_string(),
            port: 8554,
            username: None,
            password: None,
        }
    }
}

/// ONVIF Profile S server & WS-Discovery configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OnvifConfig {
    pub enabled: bool,
    pub http_port: u16,
    pub discovery_enabled: bool,
    pub device_name: String,
    pub manufacturer: String,
    pub model: String,
    pub serial_number: String,
    pub device_uuid: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl Default for OnvifConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            http_port: 8080,
            discovery_enabled: true,
            device_name: "NVRDesk POS Camera".to_string(),
            manufacturer: "NVRDesk".to_string(),
            model: "Virtual IP Camera".to_string(),
            serial_number: "NVRDESK-001".to_string(),
            device_uuid: None,
            username: None,
            password: None,
        }
    }
}

/// Top-level NVRDesk configuration loaded from TOML or defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NvrDeskConfig {
    pub always_on: bool,
    pub source: SourceConfig,
    pub main_stream: StreamProfileConfig,
    pub sub_stream: StreamProfileConfig,
    pub input_overlay: InputOverlayConfig,
    pub rtsp: RtspConfig,
    pub onvif: OnvifConfig,
}

impl Default for NvrDeskConfig {
    fn default() -> Self {
        Self {
            always_on: false,
            source: SourceConfig::default(),
            main_stream: StreamProfileConfig::default(),
            sub_stream: StreamProfileConfig {
                enabled: true,
                path: "/stream2".to_string(),
                max_width: 640,
                fps: 8,
                bitrate_bps: 450_000,
                idr_interval_secs: 2,
            },
            input_overlay: InputOverlayConfig::default(),
            rtsp: RtspConfig::default(),
            onvif: OnvifConfig::default(),
        }
    }
}

impl NvrDeskConfig {
    /// Parses a TOML configuration file from disk.
    pub fn from_toml_file(path: &Path) -> Result<Self, NvrDeskError> {
        let content = std::fs::read_to_string(path)?;
        toml::from_str(&content).map_err(|e| NvrDeskError::Config(e.to_string()))
    }
}

/// Shared error type across NVRDesk crates.
#[derive(Debug, Error)]
pub enum NvrDeskError {
    /// Configuration load or parse failure.
    #[error("config error: {0}")]
    Config(String),

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

    /// ONVIF SOAP or WS-Discovery failure.
    #[error("onvif error: {0}")]
    Onvif(String),

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

    #[test]
    fn default_config_roundtrips_toml() {
        let cfg = NvrDeskConfig::default();
        let serialized = toml::to_string_pretty(&cfg).expect("serialize toml");
        let parsed: NvrDeskConfig = toml::from_str(&serialized).expect("parse toml");
        assert_eq!(parsed.main_stream.path, "/stream1");
        assert_eq!(parsed.sub_stream.path, "/stream2");
        assert_eq!(parsed.onvif.http_port, 8080);
    }
}
