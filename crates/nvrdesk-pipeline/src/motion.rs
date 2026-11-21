//! Lightweight frame-delta motion detector for screen and webcam ONVIF MotionAlarm events.

use std::time::{Duration, Instant};

use nvrdesk_core::{PixelFormat, RawVideoFrame};

/// Motion state transition emitted by [`MotionDetector`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionTransition {
    pub active: bool,
    pub score: f32,
}

/// Samples a uniform grid of pixels across consecutive frames to detect motion cheaply.
pub struct MotionDetector {
    prev_luma: Vec<u8>,
    prev_dims: (u32, u32),
    active: bool,
    last_emit: Option<Instant>,
    quiet_frames: u32,
}

impl Default for MotionDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl MotionDetector {
    /// Creates a new motion detector.
    #[must_use]
    pub fn new() -> Self {
        Self {
            prev_luma: Vec::new(),
            prev_dims: (0, 0),
            active: false,
            last_emit: None,
            quiet_frames: 0,
        }
    }

    /// Evaluates `frame` against the previous sample and returns an ONVIF transition when state changes.
    pub fn update(
        &mut self,
        frame: &RawVideoFrame,
        sensitivity: f32,
        min_interval: Duration,
        now: Instant,
    ) -> Option<MotionTransition> {
        let sample = sample_luma_grid(frame);
        if sample.is_empty() {
            return None;
        }

        if self.prev_dims != (frame.width, frame.height) || self.prev_luma.len() != sample.len() {
            self.prev_luma = sample;
            self.prev_dims = (frame.width, frame.height);
            return None;
        }

        let total_diff: u64 = sample
            .iter()
            .zip(self.prev_luma.iter())
            .map(|(&a, &b)| u64::from(a.abs_diff(b)))
            .sum();
        let score = (total_diff as f32) / (sample.len() as f32 * 255.0);
        self.prev_luma = sample;

        let threshold = sensitivity.clamp(0.005, 0.9);
        if score >= threshold {
            self.quiet_frames = 0;
            if !self.active {
                let cooldown_ok = self
                    .last_emit
                    .is_none_or(|prev| now.saturating_duration_since(prev) >= min_interval);
                if cooldown_ok {
                    self.active = true;
                    self.last_emit = Some(now);
                    return Some(MotionTransition {
                        active: true,
                        score,
                    });
                }
            }
        } else if self.active {
            self.quiet_frames = self.quiet_frames.saturating_add(1);
            if self.quiet_frames >= 5 {
                self.active = false;
                self.quiet_frames = 0;
                self.last_emit = Some(now);
                return Some(MotionTransition {
                    active: false,
                    score,
                });
            }
        }
        None
    }

    /// Resets cached frame state.
    pub fn reset(&mut self) {
        self.prev_luma.clear();
        self.prev_dims = (0, 0);
        self.active = false;
        self.quiet_frames = 0;
    }
}

fn sample_luma_grid(frame: &RawVideoFrame) -> Vec<u8> {
    if frame.width == 0 || frame.height == 0 {
        return Vec::new();
    }
    let bpp = frame.format.bytes_per_pixel();
    let min_row = (frame.width as usize) * bpp;
    if frame.stride < min_row || frame.data.len() < frame.stride * (frame.height as usize) {
        return Vec::new();
    }

    let step_x = (frame.width / 32).max(1) as usize;
    let step_y = (frame.height / 18).max(1) as usize;
    let mut out = Vec::with_capacity(32 * 18);

    for y in (0..frame.height as usize).step_by(step_y) {
        let row = &frame.data[y * frame.stride..y * frame.stride + min_row];
        for x in (0..frame.width as usize).step_by(step_x) {
            let p = x * bpp;
            let (r, g, b) = match frame.format {
                PixelFormat::Bgra8 => (row[p + 2], row[p + 1], row[p]),
                PixelFormat::Rgba8 | PixelFormat::Rgb8 => (row[p], row[p + 1], row[p + 2]),
            };
            let luma = ((u16::from(r) * 77 + u16::from(g) * 150 + u16::from(b) * 29) >> 8) as u8;
            out.push(luma);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_motion_trigger_and_clear_after_quiet_frames() {
        let mut det = MotionDetector::new();
        let t0 = Instant::now();
        let base = RawVideoFrame {
            width: 320,
            height: 180,
            stride: 320 * 4,
            format: PixelFormat::Rgba8,
            data: vec![16u8; 320 * 180 * 4],
            timestamp_us: 0,
        };
        assert!(
            det.update(&base, 0.05, Duration::from_secs(1), t0)
                .is_none()
        );

        let moved = RawVideoFrame {
            data: vec![220u8; 320 * 180 * 4],
            ..base.clone()
        };
        let ev = det
            .update(
                &moved,
                0.05,
                Duration::from_secs(1),
                t0 + Duration::from_secs(2),
            )
            .expect("expected motion active transition");
        assert!(ev.active);
        assert!(ev.score > 0.5);

        let mut cleared = None;
        for i in 1..=6 {
            if let Some(ev) = det.update(
                &moved,
                0.05,
                Duration::from_secs(1),
                t0 + Duration::from_secs(2 + i),
            ) {
                cleared = Some(ev);
            }
        }
        let clear_ev = cleared.expect("expected motion cleared after quiet frames");
        assert!(!clear_ev.active);
    }
}
