//! Anti-aliased visual overlay renderer for mouse clicks, cursor highlight, and keystroke OSD.

use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};
use nvrdesk_core::{InputOverlaySnapshot, MouseButton, PixelFormat};

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
    for (i, ch) in chars.into_iter().enumerate() {
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
                    blend_pixel(buf, stride, format, px, py, [245, 248, 255, intensity]);
                }
            }
        }
    }
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
}
