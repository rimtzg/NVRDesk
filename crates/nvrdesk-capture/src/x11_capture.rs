//! X11 screen capture with root-window clamping to avoid `GetImage` `BadMatch` errors
//! when RandR monitor geometry slightly exceeds the X11 virtual screen size (e.g. XFCE fractional scaling).

#[cfg(target_os = "linux")]
use std::env;

use nvrdesk_core::NvrDeskError;

#[cfg(target_os = "linux")]
use nvrdesk_core::{MonitorBounds, PixelFormat, RawVideoFrame};

#[cfg(target_os = "linux")]
use xcb::{
    Connection,
    randr::GetMonitors,
    x::{Drawable, GetImage, ImageFormat, ImageOrder, Window},
};

/// Returns `true` when the active Linux desktop session is Wayland rather than X11.
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn is_wayland_session() -> bool {
    let xdg_session_type = env::var_os("XDG_SESSION_TYPE")
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    let wayland_display = env::var_os("WAYLAND_DISPLAY")
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();

    xdg_session_type == "wayland" || wayland_display.contains("wayland")
}

/// Clamps `(mon_x, mon_y, mon_w, mon_h)` strictly within `(0, 0, root_w, root_h)` so X11
/// `GetImage` never fails with `BadMatch`.
#[must_use]
pub(crate) fn clamp_rect_to_root(
    mon_x: i32,
    mon_y: i32,
    mon_w: u32,
    mon_h: u32,
    root_w: u16,
    root_h: u16,
) -> Option<(i16, i16, u16, u16)> {
    let rw = i32::from(root_w);
    let rh = i32::from(root_h);
    if rw <= 0 || rh <= 0 || mon_w == 0 || mon_h == 0 {
        return None;
    }

    let x0 = mon_x.clamp(0, rw);
    let y0 = mon_y.clamp(0, rh);
    let x1 = mon_x.saturating_add_unsigned(mon_w).clamp(0, rw);
    let y1 = mon_y.saturating_add_unsigned(mon_h).clamp(0, rh);

    let clamped_w = x1 - x0;
    let clamped_h = y1 - y0;
    if clamped_w <= 0 || clamped_h <= 0 {
        return None;
    }

    Some((x0 as i16, y0 as i16, clamped_w as u16, clamped_h as u16))
}

/// Selects monitor `display_index` (`0` prefers primary) and clamps it to the X11 root window,
/// falling back to the full root window when no active RandR monitor rect overlaps root.
#[must_use]
pub(crate) fn select_monitor_rect(
    monitors: &[(i32, i32, u32, u32, bool)],
    display_index: usize,
    root_w: u16,
    root_h: u16,
) -> Option<(i16, i16, u16, u16)> {
    let chosen = if display_index == 0 {
        monitors
            .iter()
            .find(|(_, _, _, _, primary)| *primary)
            .or_else(|| monitors.first())
    } else {
        monitors.get(display_index).or_else(|| monitors.first())
    };

    if let Some(&(x, y, w, h, _)) = chosen
        && let Some(rect) = clamp_rect_to_root(x, y, w, h, root_w, root_h)
    {
        return Some(rect);
    }

    if root_w > 0 && root_h > 0 {
        Some((0, 0, root_w, root_h))
    } else {
        None
    }
}

/// Converts an X11 `ZPixmap` buffer into tightly packed `Rgba8`.
pub(crate) fn decode_zpixmap_to_rgba(
    bytes: &[u8],
    width: u32,
    height: u32,
    depth: u8,
    bits_per_pixel: u8,
    msb_first: bool,
) -> Result<Vec<u8>, NvrDeskError> {
    if width == 0 || height == 0 {
        return Err(NvrDeskError::Capture(
            "x11 captured zero-sized frame".into(),
        ));
    }
    let w = width as usize;
    let h = height as usize;
    let bytes_per_pixel = usize::from(bits_per_pixel) / 8;
    if bytes_per_pixel == 0 {
        return Err(NvrDeskError::Capture(format!(
            "unsupported x11 bits_per_pixel: {bits_per_pixel}"
        )));
    }
    let min_row_bytes = w * bytes_per_pixel;
    if bytes.len() < min_row_bytes * h {
        return Err(NvrDeskError::Capture(format!(
            "truncated x11 GetImage reply: got {} bytes, need at least {}",
            bytes.len(),
            min_row_bytes * h
        )));
    }
    let row_stride = bytes.len() / h;
    if row_stride < min_row_bytes {
        return Err(NvrDeskError::Capture(format!(
            "invalid x11 GetImage row stride {row_stride} < {min_row_bytes}"
        )));
    }

    let mut rgba = vec![0u8; w * h * 4];
    match (depth, bits_per_pixel) {
        (24 | 32, 32) => {
            for y in 0..h {
                let src_row = &bytes[y * row_stride..y * row_stride + min_row_bytes];
                let dst_row = &mut rgba[y * w * 4..(y + 1) * w * 4];
                for (src, dst) in src_row
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(dst_row.as_chunks_mut::<4>().0.iter_mut())
                {
                    if msb_first {
                        dst[0] = src[1];
                        dst[1] = src[2];
                        dst[2] = src[3];
                    } else {
                        dst[0] = src[2];
                        dst[1] = src[1];
                        dst[2] = src[0];
                    }
                    dst[3] = 255;
                }
            }
        }
        (24, 24) => {
            for y in 0..h {
                let src_row = &bytes[y * row_stride..y * row_stride + min_row_bytes];
                let dst_row = &mut rgba[y * w * 4..(y + 1) * w * 4];
                for (src, dst) in src_row
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .zip(dst_row.as_chunks_mut::<4>().0.iter_mut())
                {
                    if msb_first {
                        dst[0] = src[0];
                        dst[1] = src[1];
                        dst[2] = src[2];
                    } else {
                        dst[0] = src[2];
                        dst[1] = src[1];
                        dst[2] = src[0];
                    }
                    dst[3] = 255;
                }
            }
        }
        (16, 16) => {
            for y in 0..h {
                let src_row = &bytes[y * row_stride..y * row_stride + min_row_bytes];
                let dst_row = &mut rgba[y * w * 4..(y + 1) * w * 4];
                for (src, dst) in src_row
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .zip(dst_row.as_chunks_mut::<4>().0.iter_mut())
                {
                    let pixel = if msb_first {
                        u16::from_be_bytes([src[0], src[1]])
                    } else {
                        u16::from_le_bytes([src[0], src[1]])
                    };
                    dst[0] = (((pixel >> 11) as u32 * 255) / 31) as u8;
                    dst[1] = ((((pixel >> 5) & 63) as u32 * 255) / 63) as u8;
                    dst[2] = (((pixel & 31) as u32 * 255) / 31) as u8;
                    dst[3] = 255;
                }
            }
        }
        (8, 8) => {
            for y in 0..h {
                let src_row = &bytes[y * row_stride..y * row_stride + min_row_bytes];
                let dst_row = &mut rgba[y * w * 4..(y + 1) * w * 4];
                for (&raw, dst) in src_row
                    .iter()
                    .zip(dst_row.as_chunks_mut::<4>().0.iter_mut())
                {
                    let pixel = if msb_first {
                        (raw & (7 << 4)) | (raw >> 4)
                    } else {
                        raw
                    };
                    dst[0] = (((pixel >> 6) as u32 * 255) / 3) as u8;
                    dst[1] = ((((pixel >> 2) & 7) as u32 * 255) / 7) as u8;
                    dst[2] = (((pixel & 3) as u32 * 255) / 3) as u8;
                    dst[3] = 255;
                }
            }
        }
        _ => {
            return Err(NvrDeskError::Capture(format!(
                "unsupported x11 depth/bpp combination: {depth}/{bits_per_pixel}"
            )));
        }
    }

    Ok(rgba)
}

#[cfg(target_os = "linux")]
pub(crate) struct X11Capturer {
    conn: Connection,
    screen_idx: i32,
    display_index: usize,
    root: Window,
    rect: (i16, i16, u16, u16),
    msb_first: bool,
    bpp_by_depth: Vec<(u8, u8)>,
}

#[cfg(target_os = "linux")]
impl X11Capturer {
    pub(crate) fn open(display_index: usize) -> Result<Self, NvrDeskError> {
        let (conn, screen_idx) = connect_x11()?;
        let (root, root_w, root_h, msb_first, bpp_by_depth) = read_screen_info(&conn, screen_idx)?;
        let rect = query_clamped_rect(&conn, root, root_w, root_h, display_index)?;

        Ok(Self {
            conn,
            screen_idx,
            display_index,
            root,
            rect,
            msb_first,
            bpp_by_depth,
        })
    }

    #[must_use]
    pub(crate) fn bounds(&self) -> MonitorBounds {
        MonitorBounds {
            x: i32::from(self.rect.0),
            y: i32::from(self.rect.1),
            width: u32::from(self.rect.2),
            height: u32::from(self.rect.3),
        }
    }

    pub(crate) fn capture_frame(
        &mut self,
        timestamp_us: u64,
    ) -> Result<RawVideoFrame, NvrDeskError> {
        match self.try_get_image(timestamp_us) {
            Ok(frame) => Ok(frame),
            Err(first_err) => {
                // Re-query RandR + root bounds once in case the display resolution changed mid-run.
                if let Ok((root, root_w, root_h, msb_first, bpp_by_depth)) =
                    read_screen_info(&self.conn, self.screen_idx)
                    && let Ok(rect) =
                        query_clamped_rect(&self.conn, root, root_w, root_h, self.display_index)
                    && rect != self.rect
                {
                    self.root = root;
                    self.rect = rect;
                    self.msb_first = msb_first;
                    self.bpp_by_depth = bpp_by_depth;
                    return self.try_get_image(timestamp_us);
                }
                Err(first_err)
            }
        }
    }

    fn try_get_image(&self, timestamp_us: u64) -> Result<RawVideoFrame, NvrDeskError> {
        let (rx, ry, rw, rh) = self.rect;
        let cookie = self.conn.send_request(&GetImage {
            format: ImageFormat::ZPixmap,
            drawable: Drawable::Window(self.root),
            x: rx,
            y: ry,
            width: rw,
            height: rh,
            plane_mask: u32::MAX,
        });

        let reply = self.conn.wait_for_reply(cookie).map_err(|e| {
            NvrDeskError::Capture(format!("x11 GetImage({rx},{ry},{rw}x{rh}) failed: {e:?}"))
        })?;

        let depth = reply.depth();
        let bpp = self
            .bpp_by_depth
            .iter()
            .find_map(|&(d, b)| (d == depth).then_some(b))
            .ok_or_else(|| {
                NvrDeskError::Capture(format!("x11 pixmap format not found for depth {depth}"))
            })?;

        let width = u32::from(rw);
        let height = u32::from(rh);
        let data = decode_zpixmap_to_rgba(reply.data(), width, height, depth, bpp, self.msb_first)?;

        Ok(RawVideoFrame {
            width,
            height,
            stride: (width as usize) * 4,
            format: PixelFormat::Rgba8,
            timestamp_us,
            data,
        })
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn capture_single_frame(
    display_index: usize,
) -> Result<(RawVideoFrame, Option<MonitorBounds>), NvrDeskError> {
    let mut capturer = X11Capturer::open(display_index)?;
    let bounds = Some(capturer.bounds());
    let frame = capturer.capture_frame(0)?;
    Ok((frame, bounds))
}

#[cfg(target_os = "linux")]
fn connect_x11() -> Result<(Connection, i32), NvrDeskError> {
    if let Some(display) = env::var("DISPLAY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        && let Ok(pair) = Connection::connect(Some(display.as_str()))
    {
        return Ok(pair);
    }

    Connection::connect(None)
        .or_else(|_| Connection::connect(Some(":0")))
        .map_err(|e| {
            NvrDeskError::Capture(format!(
                "failed to connect to X11 display (set DISPLAY/XAUTHORITY): {e:?}"
            ))
        })
}

#[cfg(target_os = "linux")]
type ScreenSetup = (Window, u16, u16, bool, Vec<(u8, u8)>);

#[cfg(target_os = "linux")]
fn read_screen_info(conn: &Connection, screen_idx: i32) -> Result<ScreenSetup, NvrDeskError> {
    let setup = conn.get_setup();
    let idx = usize::try_from(screen_idx).unwrap_or(0);
    let screen = setup
        .roots()
        .nth(idx)
        .or_else(|| setup.roots().next())
        .ok_or_else(|| NvrDeskError::Capture("no X11 screens found".into()))?;

    let root = screen.root();
    let root_w = screen.width_in_pixels();
    let root_h = screen.height_in_pixels();
    let msb_first = setup.bitmap_format_bit_order() == ImageOrder::MsbFirst;
    let bpp_by_depth = setup
        .pixmap_formats()
        .iter()
        .map(|fmt| (fmt.depth(), fmt.bits_per_pixel()))
        .collect();

    Ok((root, root_w, root_h, msb_first, bpp_by_depth))
}

#[cfg(target_os = "linux")]
fn query_clamped_rect(
    conn: &Connection,
    root: Window,
    root_w: u16,
    root_h: u16,
    display_index: usize,
) -> Result<(i16, i16, u16, u16), NvrDeskError> {
    let mut monitors = Vec::new();
    let cookie = conn.send_request(&GetMonitors {
        window: root,
        get_active: true,
    });
    if let Ok(reply) = conn.wait_for_reply(cookie) {
        for mon in reply.monitors() {
            monitors.push((
                i32::from(mon.x()),
                i32::from(mon.y()),
                u32::from(mon.width()),
                u32::from(mon.height()),
                mon.primary(),
            ));
        }
    }

    select_monitor_rect(&monitors, display_index, root_w, root_h).ok_or_else(|| {
        NvrDeskError::Capture(format!(
            "X11 root screen has invalid dimensions ({root_w}x{root_h})"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_monitor_overshoot_from_xfce_fractional_scaling() {
        // Reproduces physical Xubuntu 24.04 report: Screen 0 = 1343x895 while eDP-1 = 1344x896+0+0.
        let rect = clamp_rect_to_root(0, 0, 1344, 896, 1343, 895);
        assert_eq!(rect, Some((0, 0, 1343, 895)));
    }

    #[test]
    fn clamps_secondary_monitor_offset_and_partial_overshoot() {
        let monitors = vec![(0, 0, 1920, 1080, true), (1920, 0, 1344, 896, false)];
        let rect = select_monitor_rect(&monitors, 1, 3260, 1080);
        assert_eq!(rect, Some((1920, 0, 1340, 896)));
    }

    #[test]
    fn falls_back_to_root_when_no_monitors_listed() {
        let rect = select_monitor_rect(&[], 0, 1343, 895);
        assert_eq!(rect, Some((0, 0, 1343, 895)));
    }

    #[test]
    fn decodes_padded_32bpp_lsb_zpixmap_to_rgba() {
        // 2x2 BGRX pixels with 4 bytes of padding per row (stride = 12 instead of 8).
        let bytes = [
            10, 20, 30, 0, 40, 50, 60, 0, 99, 99, 99, 99, // row 0
            70, 80, 90, 0, 100, 110, 120, 0, 88, 88, 88, 88, // row 1
        ];
        let rgba = decode_zpixmap_to_rgba(&bytes, 2, 2, 24, 32, false).expect("decode");
        assert_eq!(
            rgba,
            vec![
                30, 20, 10, 255, 60, 50, 40, 255, // row 0
                90, 80, 70, 255, 120, 110, 100, 255, // row 1
            ]
        );
    }
}
