//! Anti-aliased visual overlay renderer for mouse clicks, cursor highlight, and keystroke OSD.

use fast_image_resize::{
    FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
    images::{Image, ImageRef},
};
use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};
use nvrdesk_core::{
    InputOverlaySnapshot, MouseButton, PipPosition, PixelFormat, PosOsdLevel, PosOsdLine,
    PosOverlaySnapshot, PrivacyMaskRect, RawVideoFrame,
};

/// Composites a webcam frame as Picture-in-Picture onto `buf` in the specified corner.
#[allow(clippy::too_many_arguments)]
pub fn composite_pip(
    resizer: &mut Resizer,
    buf: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    dst_stride: usize,
    dst_format: PixelFormat,
    pip: &RawVideoFrame,
    position: PipPosition,
    scale_ratio: f32,
) {
    if dst_w < 160 || dst_h < 120 || pip.width == 0 || pip.height == 0 {
        return;
    }

    let target_w = ((dst_w as f32) * scale_ratio.clamp(0.10, 0.45)).round() as u32;
    let target_w = target_w.max(64).min(dst_w.saturating_sub(16));
    let target_h = ((u64::from(pip.height) * u64::from(target_w)) / u64::from(pip.width)) as u32;
    let target_h = target_h.max(48).min(dst_h.saturating_sub(16));

    let pip_bpp = pip.format.bytes_per_pixel();
    let pip_row_bytes = (pip.width as usize) * pip_bpp;
    if pip.stride < pip_row_bytes || pip.data.len() < pip.stride * (pip.height as usize) {
        return;
    }

    let mut tight_pip = Vec::new();
    let src_slice = if pip.stride == pip_row_bytes {
        &pip.data[..pip_row_bytes * (pip.height as usize)]
    } else {
        tight_pip.resize(pip_row_bytes * (pip.height as usize), 0);
        for row in 0..(pip.height as usize) {
            let s = row * pip.stride;
            let d = row * pip_row_bytes;
            tight_pip[d..d + pip_row_bytes].copy_from_slice(&pip.data[s..s + pip_row_bytes]);
        }
        &tight_pip
    };

    let pixel_type = match pip.format {
        PixelFormat::Bgra8 | PixelFormat::Rgba8 => PixelType::U8x4,
        PixelFormat::Rgb8 => PixelType::U8x3,
    };

    let Ok(src_img) = ImageRef::new(pip.width, pip.height, src_slice, pixel_type) else {
        return;
    };
    let mut scaled = vec![0u8; (target_w as usize) * (target_h as usize) * pip_bpp];
    let Ok(mut dst_img) = Image::from_slice_u8(target_w, target_h, &mut scaled, pixel_type) else {
        return;
    };
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
    if resizer.resize(&src_img, &mut dst_img, &opts).is_err() {
        return;
    }

    let margin = ((dst_h as usize) / 40).max(8);
    let ox = match position {
        PipPosition::TopLeft | PipPosition::BottomLeft => margin,
        PipPosition::TopRight | PipPosition::BottomRight => {
            (dst_w as usize).saturating_sub(target_w as usize + margin)
        }
    };
    let oy = match position {
        PipPosition::TopLeft | PipPosition::TopRight => margin,
        PipPosition::BottomLeft | PipPosition::BottomRight => {
            (dst_h as usize).saturating_sub(target_h as usize + margin)
        }
    };

    let dst_bpp = dst_format.bytes_per_pixel();
    for y in 0..(target_h as usize) {
        let py = oy + y;
        if py >= dst_h as usize {
            break;
        }
        for x in 0..(target_w as usize) {
            let px = ox + x;
            if px >= dst_w as usize {
                break;
            }
            let s_idx = (y * (target_w as usize) + x) * pip_bpp;
            let d_idx = py * dst_stride + px * dst_bpp;
            let (r, g, b) =
                if x == 0 || y == 0 || x + 1 == target_w as usize || y + 1 == target_h as usize {
                    (30u8, 35u8, 45u8)
                } else {
                    match pip.format {
                        PixelFormat::Rgba8 | PixelFormat::Rgb8 => {
                            (scaled[s_idx], scaled[s_idx + 1], scaled[s_idx + 2])
                        }
                        PixelFormat::Bgra8 => (scaled[s_idx + 2], scaled[s_idx + 1], scaled[s_idx]),
                    }
                };
            match dst_format {
                PixelFormat::Rgba8 | PixelFormat::Rgb8 => {
                    buf[d_idx] = r;
                    buf[d_idx + 1] = g;
                    buf[d_idx + 2] = b;
                    if dst_bpp == 4 {
                        buf[d_idx + 3] = 255;
                    }
                }
                PixelFormat::Bgra8 => {
                    buf[d_idx] = b;
                    buf[d_idx + 1] = g;
                    buf[d_idx + 2] = r;
                    buf[d_idx + 3] = 255;
                }
            }
        }
    }
}

/// Renders cursor highlight, animated mouse click ripples, and keystroke OSD onto a packed buffer.
pub fn draw_overlays(
    buf: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    overlay: &InputOverlaySnapshot,
) {
    if width < 64 || height < 64 {
        return;
    }

    let scale = (height as f32 / 1080.0).clamp(0.45, 2.0);

    if overlay.show_cursor_highlight
        && let Some((nx, ny)) = overlay.cursor_norm
    {
        let cx = nx * (width as f32);
        let cy = ny * (height as f32);
        let radius = (11.0 * scale).max(5.0);
        let stroke = (2.0 * scale).max(1.5);
        draw_ring_aa(
            buf,
            width,
            height,
            stride,
            format,
            cx,
            cy,
            radius,
            stroke,
            [255, 230, 80, 150],
        );
    }

    for ripple in &overlay.clicks {
        let cx = ripple.norm_x * (width as f32);
        let cy = ripple.norm_y * (height as f32);
        let base_r = (8.0 * scale).max(4.0);
        let max_r = (30.0 * scale).max(14.0);
        let radius = base_r + (max_r - base_r) * ripple.progress;
        let stroke = (3.5 * scale).max(2.0);
        let alpha = ((1.0 - ripple.progress) * 235.0).clamp(0.0, 255.0) as u8;
        let [r, g, b] = match ripple.button {
            MouseButton::Left => [255, 185, 30],
            MouseButton::Right => [55, 175, 255],
            MouseButton::Middle => [75, 220, 120],
        };
        draw_ring_aa(
            buf,
            width,
            height,
            stride,
            format,
            cx,
            cy,
            radius,
            stroke,
            [r, g, b, alpha],
        );
    }

    let osd_text = if overlay.privacy_paused {
        "KEYS: [PRIVACY PAUSED]"
    } else if !overlay.keystroke_text.is_empty() {
        overlay.keystroke_text.as_str()
    } else {
        ""
    };

    if !osd_text.is_empty() {
        draw_keystroke_pill(buf, width, height, stride, format, osd_text);
    }
}

fn draw_keystroke_pill(
    buf: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    text: &str,
) {
    let raster_h = if height >= 720 {
        RasterHeight::Size24
    } else {
        RasterHeight::Size16
    };
    let char_w = get_raster_width(FontWeight::Bold, raster_h);
    let char_h = raster_h.val();

    let pad_x = (char_w / 2 + 6).max(8);
    let pad_y = (char_h / 4 + 3).max(5);
    let max_chars = ((width as usize).saturating_sub(pad_x * 2 + 24) / char_w).max(8);
    let chars: Vec<char> = {
        let count = text.chars().count();
        if count > max_chars {
            text.chars().skip(count - max_chars).collect()
        } else {
            text.chars().collect()
        }
    };

    let box_w = chars.len() * char_w + pad_x * 2;
    let box_h = char_h + pad_y * 2;
    let margin_bottom = ((height as usize) / 28).max(8);
    let box_x = ((width as usize).saturating_sub(box_w)) / 2;
    let box_y = (height as usize).saturating_sub(box_h + margin_bottom);

    // Semi-transparent dark pill background.
    fill_rect_blend(
        buf,
        width,
        height,
        stride,
        format,
        box_x,
        box_y,
        box_w,
        box_h,
        [14, 18, 26, 210],
    );

    let text_x = box_x + pad_x;
    let text_y = box_y + pad_y;
    draw_text_run(
        buf,
        width,
        height,
        stride,
        format,
        &chars,
        text_x,
        text_y,
        char_w,
        raster_h,
        [245, 248, 255],
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_text_run(
    buf: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    chars: &[char],
    text_x: usize,
    text_y: usize,
    char_w: usize,
    raster_h: RasterHeight,
    rgb: [u8; 3],
) {
    for (i, &ch) in chars.iter().enumerate() {
        let glyph = get_raster(ch, FontWeight::Bold, raster_h)
            .or_else(|| get_raster('?', FontWeight::Bold, raster_h));
        let Some(raster) = glyph else { continue };
        let gx = text_x + i * char_w;
        for (ry, row) in raster.rows().enumerate() {
            let py = text_y + ry;
            if py >= height as usize {
                break;
            }
            for (rx, &intensity) in row.iter().enumerate() {
                if intensity == 0 {
                    continue;
                }
                let px = gx + rx;
                if px < width as usize {
                    blend_pixel(
                        buf,
                        stride,
                        format,
                        px,
                        py,
                        [rgb[0], rgb[1], rgb[2], intensity],
                    );
                }
            }
        }
    }
}

/// Applies solid opaque privacy masks to normalized frame regions before overlays and encoding.
pub fn apply_privacy_masks(
    buf: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    masks: &[PrivacyMaskRect],
) {
    if width == 0 || height == 0 || masks.is_empty() {
        return;
    }
    let w_f = width as f32;
    let h_f = height as f32;

    for mask in masks {
        let x0 = (mask.x.clamp(0.0, 1.0) * w_f).floor() as usize;
        let y0 = (mask.y.clamp(0.0, 1.0) * h_f).floor() as usize;
        let mw = (mask.width.clamp(0.0, 1.0) * w_f).ceil() as usize;
        let mh = (mask.height.clamp(0.0, 1.0) * h_f).ceil() as usize;
        if mw == 0 || mh == 0 {
            continue;
        }
        fill_rect_blend(
            buf,
            width,
            height,
            stride,
            format,
            x0,
            y0,
            mw,
            mh,
            [15, 23, 42, 255],
        );

        if let Some(label) = mask
            .label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let raster_h = RasterHeight::Size16;
            let char_w = get_raster_width(FontWeight::Bold, raster_h);
            let max_chars = mw.saturating_sub(12) / char_w;
            if max_chars >= 2 && mh >= 22 {
                let chars: Vec<char> = label.chars().take(max_chars).collect();
                let lx = x0 + 6;
                let ly = y0 + (mh.saturating_sub(16)) / 2;
                draw_text_run(
                    buf,
                    width,
                    height,
                    stride,
                    format,
                    &chars,
                    lx,
                    ly,
                    char_w,
                    raster_h,
                    [148, 163, 184],
                );
            }
        }
    }
}

/// Renders active dynamic POS OSD lines onto `buf` at `position`.
pub fn draw_pos_osd(
    buf: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    lines: &[PosOsdLine],
    position: PipPosition,
) {
    if width < 160 || height < 120 || lines.is_empty() {
        return;
    }

    let raster_h = if width >= 1200 {
        RasterHeight::Size24
    } else {
        RasterHeight::Size16
    };
    let char_h = raster_h.val();
    let char_w = get_raster_width(FontWeight::Bold, raster_h);
    let pad_x = if width >= 1200 { 14 } else { 10 };
    let pad_y = if width >= 1200 { 8 } else { 6 };
    let line_gap = 6usize;
    let margin = ((width.min(height) as usize) / 40).max(10);
    let max_chars = ((width as usize).saturating_sub(margin * 2 + pad_x * 2 + 12) / char_w).max(8);

    let mut cur_y = match position {
        PipPosition::TopLeft | PipPosition::TopRight => margin,
        PipPosition::BottomLeft | PipPosition::BottomRight => {
            let total_h =
                lines.len() * (char_h + pad_y * 2) + lines.len().saturating_sub(1) * line_gap;
            (height as usize).saturating_sub(margin + total_h)
        }
    };

    for line in lines {
        let chars: Vec<char> = line.text.chars().take(max_chars).collect();
        if chars.is_empty() {
            continue;
        }
        let box_w = chars.len() * char_w + pad_x * 2 + 6;
        let box_h = char_h + pad_y * 2;
        let box_x = match position {
            PipPosition::TopLeft | PipPosition::BottomLeft => margin,
            PipPosition::TopRight | PipPosition::BottomRight => {
                (width as usize).saturating_sub(margin + box_w)
            }
        };

        fill_rect_blend(
            buf,
            width,
            height,
            stride,
            format,
            box_x,
            cur_y,
            box_w,
            box_h,
            [14, 18, 26, 215],
        );

        let accent = match line.level {
            PosOsdLevel::Info => [59, 130, 246, 255],
            PosOsdLevel::Warning => [245, 158, 11, 255],
            PosOsdLevel::Alert => [239, 68, 68, 255],
        };
        fill_rect_blend(
            buf, width, height, stride, format, box_x, cur_y, 4, box_h, accent,
        );

        draw_text_run(
            buf,
            width,
            height,
            stride,
            format,
            &chars,
            box_x + pad_x + 4,
            cur_y + pad_y,
            char_w,
            raster_h,
            [248, 250, 252],
        );
        cur_y += box_h + line_gap;
    }
}

/// Renders privacy masks and dynamic POS OSD lines onto `buf` from `pos_snap`.
pub fn draw_pos_snapshot(
    buf: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    pos_snap: &PosOverlaySnapshot,
) {
    draw_pos_osd(
        buf,
        width,
        height,
        stride,
        format,
        &pos_snap.lines,
        pos_snap.osd_position,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_ring_aa(
    buf: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    cx: f32,
    cy: f32,
    radius: f32,
    stroke: f32,
    rgba: [u8; 4],
) {
    let half_stroke = stroke * 0.5;
    let bound = (radius + half_stroke + 1.5).ceil() as i32;
    let min_x = ((cx as i32) - bound).max(0) as usize;
    let max_x = ((cx as i32) + bound).min(width as i32 - 1) as usize;
    let min_y = ((cy as i32) - bound).max(0) as usize;
    let max_y = ((cy as i32) + bound).min(height as i32 - 1) as usize;

    for py in min_y..=max_y {
        let dy = (py as f32) + 0.5 - cy;
        for px in min_x..=max_x {
            let dx = (px as f32) + 0.5 - cx;
            let dist = (dx * dx + dy * dy).sqrt();
            let edge_dist = (dist - radius).abs() - half_stroke;
            if edge_dist >= 1.0 {
                continue;
            }
            let coverage = (1.0 - edge_dist).clamp(0.0, 1.0);
            let alpha = ((f32::from(rgba[3])) * coverage).round() as u8;
            if alpha > 0 {
                blend_pixel(
                    buf,
                    stride,
                    format,
                    px,
                    py,
                    [rgba[0], rgba[1], rgba[2], alpha],
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_rect_blend(
    buf: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    rgba: [u8; 4],
) {
    let end_x = (x + w).min(width as usize);
    let end_y = (y + h).min(height as usize);
    for py in y..end_y {
        for px in x..end_x {
            blend_pixel(buf, stride, format, px, py, rgba);
        }
    }
}

#[inline]
fn blend_pixel(
    buf: &mut [u8],
    stride: usize,
    format: PixelFormat,
    x: usize,
    y: usize,
    src_rgba: [u8; 4],
) {
    let a = u16::from(src_rgba[3]);
    if a == 0 {
        return;
    }
    let inv_a = 255 - a;
    let bpp = format.bytes_per_pixel();
    let idx = y * stride + x * bpp;
    if idx + bpp > buf.len() {
        return;
    }

    match format {
        PixelFormat::Rgba8 | PixelFormat::Rgb8 => {
            buf[idx] = ((u16::from(src_rgba[0]) * a + u16::from(buf[idx]) * inv_a) / 255) as u8;
            buf[idx + 1] =
                ((u16::from(src_rgba[1]) * a + u16::from(buf[idx + 1]) * inv_a) / 255) as u8;
            buf[idx + 2] =
                ((u16::from(src_rgba[2]) * a + u16::from(buf[idx + 2]) * inv_a) / 255) as u8;
        }
        PixelFormat::Bgra8 => {
            buf[idx] = ((u16::from(src_rgba[2]) * a + u16::from(buf[idx]) * inv_a) / 255) as u8;
            buf[idx + 1] =
                ((u16::from(src_rgba[1]) * a + u16::from(buf[idx + 1]) * inv_a) / 255) as u8;
            buf[idx + 2] =
                ((u16::from(src_rgba[0]) * a + u16::from(buf[idx + 2]) * inv_a) / 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvrdesk_core::ClickRipple;

    #[test]
    fn draws_click_ripple_and_keystroke_pill_onto_buffer() {
        let mut buf = vec![0u8; 640 * 360 * 4];
        let snap = InputOverlaySnapshot {
            cursor_norm: Some((0.5, 0.5)),
            show_cursor_highlight: true,
            clicks: vec![ClickRipple {
                norm_x: 0.5,
                norm_y: 0.5,
                button: MouseButton::Left,
                progress: 0.3,
            }],
            keystroke_text: "[Ctrl+P] 123".to_string(),
            privacy_paused: false,
        };
        draw_overlays(&mut buf, 640, 360, 640 * 4, PixelFormat::Rgba8, &snap);
        assert!(buf.iter().any(|&b| b != 0), "expected overlay pixels drawn");
    }

    #[test]
    fn composites_pip_into_top_right_corner() {
        let mut resizer = Resizer::new();
        let mut dst = vec![0u8; 640 * 360 * 4];
        let pip = RawVideoFrame {
            width: 160,
            height: 120,
            stride: 160 * 4,
            format: PixelFormat::Rgba8,
            data: vec![200u8; 160 * 120 * 4],
            timestamp_us: 0,
        };
        composite_pip(
            &mut resizer,
            &mut dst,
            640,
            360,
            640 * 4,
            PixelFormat::Bgra8,
            &pip,
            PipPosition::TopRight,
            0.25,
        );
        // Top-right area must contain PiP pixels while top-left remains untouched (0).
        assert_eq!(dst[0], 0);
        let check_x = 580;
        let check_y = 40;
        let idx = (check_y * 640 + check_x) * 4;
        assert_eq!(dst[idx], 200);
    }

    #[test]
    fn applies_privacy_masks_and_draws_pos_osd() {
        let mut buf = vec![255u8; 640 * 360 * 4];
        let masks = [PrivacyMaskRect {
            x: 0.0,
            y: 0.0,
            width: 0.25,
            height: 0.2,
            label: Some("PIN".to_string()),
        }];
        apply_privacy_masks(&mut buf, 640, 360, 640 * 4, PixelFormat::Rgba8, &masks);
        // Pixel inside masked area must be overwritten from 255 -> 15 (R channel of #0F172A).
        assert_eq!(buf[0], 15);
        // Pixel outside masked area remains white (255).
        let unmasked_idx = (200 * 640 + 400) * 4;
        assert_eq!(buf[unmasked_idx], 255);

        let lines = [PosOsdLine {
            slot: "ticket".into(),
            text: "Ticket #1042 $95.00".into(),
            level: PosOsdLevel::Warning,
        }];
        draw_pos_osd(
            &mut buf,
            640,
            360,
            640 * 4,
            PixelFormat::Rgba8,
            &lines,
            PipPosition::BottomRight,
        );
        // Bottom-right region was modified by the POS OSD pill.
        let osd_idx = (335 * 640 + 550) * 4;
        assert_ne!(buf[osd_idx], 255);
    }
}
