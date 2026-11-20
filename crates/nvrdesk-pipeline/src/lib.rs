//! Video processing pipeline: scaling and SIMD RGB/BGRA -> I420 conversion.

use fast_image_resize::{
    FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
    images::{Image, ImageRef},
};
use nvrdesk_core::{I420Frame, NvrDeskError, PixelFormat, RawVideoFrame};
use yuvutils_rs::{
    BufferStoreMut, YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
    bgra_to_yuv420, rgb_to_yuv420, rgba_to_yuv420,
};

/// Computes even target dimensions preserving aspect ratio while respecting `max_width`.
#[must_use]
pub fn compute_target_dimensions(src_w: u32, src_h: u32, max_width: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (0, 0);
    }
    let (w, h) = if max_width > 0 && src_w > max_width {
        let scaled_h = (u64::from(src_h) * u64::from(max_width)) / u64::from(src_w);
        (max_width, scaled_h as u32)
    } else {
        (src_w, src_h)
    };
    let even_w = (w & !1).max(2);
    let even_h = (h & !1).max(2);
    (even_w, even_h)
}

/// Stateful frame scaler and color converter reusing scratch buffers across frames.
pub struct FrameProcessor {
    resizer: Resizer,
    tight_src_buf: Vec<u8>,
    resized_buf: Vec<u8>,
}

impl Default for FrameProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameProcessor {
    /// Creates a new `FrameProcessor`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            resizer: Resizer::new(),
            tight_src_buf: Vec::new(),
            resized_buf: Vec::new(),
        }
    }

    /// Scales (if larger than `max_width` or odd-sized) and converts `frame` into planar I420.
    pub fn process(
        &mut self,
        frame: &RawVideoFrame,
        max_width: u32,
    ) -> Result<I420Frame, NvrDeskError> {
        let (dst_w, dst_h) = compute_target_dimensions(frame.width, frame.height, max_width);
        if dst_w == 0 || dst_h == 0 {
            return Err(NvrDeskError::InvalidDimensions {
                width: frame.width,
                height: frame.height,
            });
        }

        let bpp = frame.format.bytes_per_pixel();
        let min_row_bytes = (frame.width as usize) * bpp;
        if frame.stride < min_row_bytes || frame.data.len() < frame.stride * (frame.height as usize)
        {
            return Err(NvrDeskError::Pipeline(format!(
                "invalid frame buffer: {}x{} stride={} len={}",
                frame.width,
                frame.height,
                frame.stride,
                frame.data.len()
            )));
        }

        if dst_w == frame.width && dst_h == frame.height {
            return convert_packed_to_i420(
                &frame.data,
                frame.stride as u32,
                frame.width,
                frame.height,
                frame.format,
                frame.timestamp_us,
            );
        }

        // fast_image_resize expects tightly packed rows.
        let src_slice = if frame.stride == min_row_bytes {
            &frame.data[..min_row_bytes * (frame.height as usize)]
        } else {
            self.tight_src_buf
                .resize(min_row_bytes * (frame.height as usize), 0);
            for row in 0..(frame.height as usize) {
                let src_start = row * frame.stride;
                let dst_start = row * min_row_bytes;
                self.tight_src_buf[dst_start..dst_start + min_row_bytes]
                    .copy_from_slice(&frame.data[src_start..src_start + min_row_bytes]);
            }
            &self.tight_src_buf
        };

        let pixel_type = match frame.format {
            PixelFormat::Bgra8 | PixelFormat::Rgba8 => PixelType::U8x4,
            PixelFormat::Rgb8 => PixelType::U8x3,
        };

        let src_img = ImageRef::new(frame.width, frame.height, src_slice, pixel_type)
            .map_err(|e| NvrDeskError::Pipeline(format!("resize source init: {e}")))?;

        let dst_stride = (dst_w as usize) * bpp;
        self.resized_buf.resize(dst_stride * (dst_h as usize), 0);
        let mut dst_img = Image::from_slice_u8(dst_w, dst_h, &mut self.resized_buf, pixel_type)
            .map_err(|e| NvrDeskError::Pipeline(format!("resize dest init: {e}")))?;

        let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
        self.resizer
            .resize(&src_img, &mut dst_img, &options)
            .map_err(|e| NvrDeskError::Pipeline(format!("resize failed: {e}")))?;

        convert_packed_to_i420(
            &self.resized_buf,
            dst_stride as u32,
            dst_w,
            dst_h,
            frame.format,
            frame.timestamp_us,
        )
    }
}

fn convert_packed_to_i420(
    src: &[u8],
    src_stride: u32,
    width: u32,
    height: u32,
    format: PixelFormat,
    timestamp_us: u64,
) -> Result<I420Frame, NvrDeskError> {
    let mut i420 = I420Frame::new(width, height, timestamp_us)?;
    let y_stride = i420.y_stride() as u32;
    let uv_stride = i420.uv_stride() as u32;
    let (y, u, v) = i420.planes_mut();

    let mut planar = YuvPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(y),
        y_stride,
        u_plane: BufferStoreMut::Borrowed(u),
        u_stride: uv_stride,
        v_plane: BufferStoreMut::Borrowed(v),
        v_stride: uv_stride,
        width,
        height,
    };

    let res = match format {
        PixelFormat::Bgra8 => bgra_to_yuv420(
            &mut planar,
            src,
            src_stride,
            YuvRange::Limited,
            YuvStandardMatrix::Bt709,
            YuvConversionMode::Balanced,
        ),
        PixelFormat::Rgba8 => rgba_to_yuv420(
            &mut planar,
            src,
            src_stride,
            YuvRange::Limited,
            YuvStandardMatrix::Bt709,
            YuvConversionMode::Balanced,
        ),
        PixelFormat::Rgb8 => rgb_to_yuv420(
            &mut planar,
            src,
            src_stride,
            YuvRange::Limited,
            YuvStandardMatrix::Bt709,
            YuvConversionMode::Balanced,
        ),
    };

    res.map_err(|e| NvrDeskError::Pipeline(format!("yuv420 conversion failed: {e:?}")))?;
    Ok(i420)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_even_scaled_dimensions() {
        assert_eq!(compute_target_dimensions(3840, 2160, 1920), (1920, 1080));
        assert_eq!(compute_target_dimensions(1920, 1080, 1920), (1920, 1080));
        assert_eq!(compute_target_dimensions(1921, 1081, 0), (1920, 1080));
        assert_eq!(compute_target_dimensions(1280, 720, 1920), (1280, 720));
    }

    #[test]
    fn processes_bgra_with_downscale() {
        let mut proc = FrameProcessor::new();
        let raw = RawVideoFrame {
            width: 640,
            height: 360,
            stride: 640 * 4,
            format: PixelFormat::Bgra8,
            data: vec![128; 640 * 360 * 4],
            timestamp_us: 1000,
        };
        let out = proc.process(&raw, 320).expect("process succeeds");
        assert_eq!(out.width, 320);
        assert_eq!(out.height, 180);
        assert_eq!(out.timestamp_us, 1000);
        assert_eq!(out.y_plane().len(), 320 * 180);
    }

    #[test]
    fn processes_rgba_with_padding_stride() {
        let mut proc = FrameProcessor::new();
        let stride = 320 * 4 + 16;
        let raw = RawVideoFrame {
            width: 320,
            height: 240,
            stride,
            format: PixelFormat::Rgba8,
            data: vec![200; stride * 240],
            timestamp_us: 2000,
        };
        let out = proc.process(&raw, 1920).expect("process succeeds");
        assert_eq!(out.width, 320);
        assert_eq!(out.height, 240);
    }
}
