//! Video capture sources for NVRDesk (cross-platform screen + cursor and synthetic test pattern).

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, RecvTimeoutError, SyncSender},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nvrdesk_core::{NvrDeskError, PixelFormat, RawVideoFrame};
use tracing::{debug, warn};
use xcap::{Frame as XcapFrame, Monitor};

/// Abstraction over a video capture backend (display, window, webcam, or synthetic generator).
pub trait CaptureSource: Send {
    /// Human-readable source identifier.
    fn name(&self) -> &str;

    /// Starts frame capture.
    fn start(&mut self) -> Result<(), NvrDeskError>;

    /// Waits up to `timeout` for the next captured frame.
    ///
    /// Returns `Ok(None)` when no new frame was produced before `timeout` (e.g. static screen),
    /// allowing the pipeline to repeat the previous frame to maintain constant FPS.
    fn try_next_frame(&mut self, timeout: Duration) -> Result<Option<RawVideoFrame>, NvrDeskError>;

    /// Stops frame capture and releases OS resources.
    fn stop(&mut self);
}

/// Cross-platform screen capture source backed by `xcap`.
pub struct ScreenCaptureSource {
    name: String,
    display_index: usize,
    running: Arc<AtomicBool>,
    rx: Option<mpsc::Receiver<Result<RawVideoFrame, NvrDeskError>>>,
    worker: Option<JoinHandle<()>>,
}

impl ScreenCaptureSource {
    /// Creates a new screen capture source for `display_index` (0 = primary or first monitor).
    #[must_use]
    pub fn new(display_index: usize, _fps: u32, _show_cursor: bool) -> Self {
        Self {
            name: format!("screen:{display_index}"),
            display_index,
            running: Arc::new(AtomicBool::new(false)),
            rx: None,
            worker: None,
        }
    }
}

fn select_monitor(display_index: usize) -> Result<Monitor, NvrDeskError> {
    let monitors = Monitor::all()
        .map_err(|e| NvrDeskError::Capture(format!("failed to enumerate monitors: {e}")))?;
    if monitors.is_empty() {
        return Err(NvrDeskError::Capture("no displays found".into()));
    }

    if display_index == 0
        && let Some(primary) = monitors
            .iter()
            .find(|m| m.is_primary().unwrap_or(false))
            .cloned()
    {
        return Ok(primary);
    }

    monitors
        .get(display_index)
        .cloned()
        .or_else(|| monitors.into_iter().next())
        .ok_or_else(|| NvrDeskError::Capture("display index out of range".into()))
}

impl CaptureSource for ScreenCaptureSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn start(&mut self) -> Result<(), NvrDeskError> {
        if self.running.load(Ordering::Acquire) {
            return Ok(());
        }

        // Bounded queue of 2 frames: drop stale frames rather than building up latency.
        let (tx, rx) = mpsc::sync_channel(2);
        let (init_tx, init_rx) = mpsc::sync_channel(1);
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::Release);
        let display_index = self.display_index;

        let worker = thread::Builder::new()
            .name("nvrdesk-xcap-capture".into())
            .spawn(move || {
                // Keep Monitor/VideoRecorder on this thread since OS capture handles may be !Send.
                let monitor = match select_monitor(display_index) {
                    Ok(m) => m,
                    Err(err) => {
                        let _ = init_tx.send(Err(err));
                        return;
                    }
                };
                let monitor_name = monitor
                    .name()
                    .unwrap_or_else(|_| format!("screen:{display_index}"));
                let (recorder, raw_rx) = match monitor.video_recorder() {
                    Ok(pair) => pair,
                    Err(err) => {
                        let _ = init_tx.send(Err(map_xcap_error(&err.to_string())));
                        return;
                    }
                };
                if let Err(err) = recorder.start() {
                    let _ = init_tx.send(Err(map_xcap_error(&err.to_string())));
                    return;
                }

                debug!(monitor = %monitor_name, "screen capture started");
                let _ = init_tx.send(Ok(()));

                let start_time = Instant::now();
                capture_loop(raw_rx, &tx, &running, start_time);

                if let Err(err) = recorder.stop() {
                    warn!(error = %err, "failed to stop screen recorder cleanly");
                }
            })
            .map_err(|e| NvrDeskError::Capture(format!("failed to spawn capture thread: {e}")))?;

        match init_rx.recv() {
            Ok(Ok(())) => {
                self.rx = Some(rx);
                self.worker = Some(worker);
                Ok(())
            }
            Ok(Err(err)) => {
                self.running.store(false, Ordering::Release);
                let _ = worker.join();
                Err(err)
            }
            Err(_) => {
                self.running.store(false, Ordering::Release);
                let _ = worker.join();
                Err(NvrDeskError::Capture(
                    "capture worker terminated during initialization".into(),
                ))
            }
        }
    }

    fn try_next_frame(&mut self, timeout: Duration) -> Result<Option<RawVideoFrame>, NvrDeskError> {
        let Some(rx) = &self.rx else {
            return Err(NvrDeskError::Capture("capture source not started".into()));
        };

        match rx.recv_timeout(timeout) {
            Ok(Ok(frame)) => Ok(Some(frame)),
            Ok(Err(err)) => Err(err),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(NvrDeskError::Capture(
                "capture worker stopped unexpectedly".into(),
            )),
        }
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.rx.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ScreenCaptureSource {
    fn drop(&mut self) {
        self.stop();
    }
}

fn map_xcap_error(msg: &str) -> NvrDeskError {
    let lower = msg.to_lowercase();
    if lower.contains("permission") || lower.contains("denied") || lower.contains("not authorized")
    {
        NvrDeskError::PermissionDenied(format!(
            "{msg} — grant Screen Recording permission in OS settings and restart"
        ))
    } else {
        NvrDeskError::Capture(msg.to_string())
    }
}

fn capture_loop(
    raw_rx: mpsc::Receiver<XcapFrame>,
    tx: &SyncSender<Result<RawVideoFrame, NvrDeskError>>,
    running: &AtomicBool,
    start_time: Instant,
) {
    while running.load(Ordering::Acquire) {
        match raw_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => {
                if frame.width == 0 || frame.height == 0 {
                    continue;
                }
                let expected_min = (frame.width as usize) * (frame.height as usize) * 4;
                if frame.raw.len() < expected_min {
                    warn!(
                        width = frame.width,
                        height = frame.height,
                        len = frame.raw.len(),
                        "dropping truncated captured frame"
                    );
                    continue;
                }
                let stride = frame.raw.len() / (frame.height as usize);
                let raw_frame = RawVideoFrame {
                    width: frame.width,
                    height: frame.height,
                    stride,
                    format: PixelFormat::Rgba8,
                    data: frame.raw,
                    timestamp_us: start_time.elapsed().as_micros() as u64,
                };
                if let Err(mpsc::TrySendError::Disconnected(_)) = tx.try_send(Ok(raw_frame)) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                if running.load(Ordering::Acquire) {
                    let _ = tx.try_send(Err(NvrDeskError::Capture(
                        "screen recorder stream disconnected".into(),
                    )));
                }
                break;
            }
        }
    }
    debug!("xcap capture loop exited");
}

/// Synthetic animated test-pattern generator (color bars + moving bar + simulated cursor).
///
/// Used for headless CI integration tests and environments without OS display permissions.
pub struct TestPatternSource {
    name: String,
    width: u32,
    height: u32,
    fps: u32,
    frame_index: u64,
    start_time: Option<Instant>,
    last_emit: Option<Instant>,
    running: bool,
}

impl TestPatternSource {
    /// Creates a new synthetic pattern source with even dimensions.
    #[must_use]
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self {
            name: format!("test-pattern:{width}x{height}"),
            width: width.max(64),
            height: height.max(64),
            fps: fps.max(1),
            frame_index: 0,
            start_time: None,
            last_emit: None,
            running: false,
        }
    }
}

impl CaptureSource for TestPatternSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn start(&mut self) -> Result<(), NvrDeskError> {
        self.running = true;
        self.frame_index = 0;
        self.start_time = Some(Instant::now());
        self.last_emit = None;
        Ok(())
    }

    fn try_next_frame(&mut self, timeout: Duration) -> Result<Option<RawVideoFrame>, NvrDeskError> {
        if !self.running {
            return Err(NvrDeskError::Capture(
                "test pattern source not started".into(),
            ));
        }

        let interval = Duration::from_secs_f64(1.0 / f64::from(self.fps));
        if let Some(last) = self.last_emit {
            let elapsed = last.elapsed();
            if elapsed < interval {
                let wait = (interval - elapsed).min(timeout);
                thread::sleep(wait);
                if last.elapsed() < interval {
                    return Ok(None);
                }
            }
        }

        let now = Instant::now();
        self.last_emit = Some(now);
        let ts_us = self
            .start_time
            .map_or(0, |s| s.elapsed().as_micros() as u64);

        let frame = render_test_pattern(self.width, self.height, self.frame_index, ts_us);
        self.frame_index = self.frame_index.wrapping_add(1);
        Ok(Some(frame))
    }

    fn stop(&mut self) {
        self.running = false;
    }
}

fn render_test_pattern(
    width: u32,
    height: u32,
    frame_idx: u64,
    timestamp_us: u64,
) -> RawVideoFrame {
    // Standard 7 SMPTE-style BGRA bars.
    const BARS_BGRA: [[u8; 4]; 7] = [
        [235, 235, 235, 255], // White
        [0, 235, 235, 255],   // Yellow
        [235, 235, 0, 255],   // Cyan
        [0, 235, 0, 255],     // Green
        [235, 0, 235, 255],   // Magenta
        [0, 0, 235, 255],     // Red
        [235, 0, 0, 255],     // Blue
    ];

    let w = width as usize;
    let h = height as usize;
    let stride = w * 4;
    let mut data = vec![0u8; stride * h];

    let bar_width = (w / BARS_BGRA.len()).max(1);
    let cursor_x = ((frame_idx * 12) as usize) % w.saturating_sub(24).max(1);
    let cursor_y = ((frame_idx * 7) as usize) % h.saturating_sub(24).max(1);
    let sweep_y = ((frame_idx * 8) as usize) % h;

    for y in 0..h {
        let row = &mut data[y * stride..(y + 1) * stride];
        let is_sweep = y >= sweep_y && y < sweep_y.saturating_add(6);
        for x in 0..w {
            let color = if x >= cursor_x
                && x < cursor_x + 20
                && y >= cursor_y
                && y < cursor_y + 20
                && (x - cursor_x <= y - cursor_y)
            {
                // Simulated white mouse pointer with dark border.
                if x == cursor_x || y == cursor_y || (x - cursor_x) == (y - cursor_y) {
                    [20, 20, 20, 255]
                } else {
                    [255, 255, 255, 255]
                }
            } else if is_sweep {
                [30, 30, 30, 255]
            } else {
                let idx = (x / bar_width).min(BARS_BGRA.len() - 1);
                BARS_BGRA[idx]
            };
            let offset = x * 4;
            row[offset..offset + 4].copy_from_slice(&color);
        }
    }

    RawVideoFrame {
        width,
        height,
        stride,
        format: PixelFormat::Bgra8,
        data,
        timestamp_us,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pattern_produces_valid_bgra_frames() {
        let mut src = TestPatternSource::new(320, 240, 30);
        src.start().expect("start succeeds");
        let frame = src
            .try_next_frame(Duration::from_millis(200))
            .expect("frame read succeeds")
            .expect("first frame emitted immediately");
        assert_eq!(frame.width, 320);
        assert_eq!(frame.height, 240);
        assert_eq!(frame.format, PixelFormat::Bgra8);
        assert_eq!(frame.data.len(), 320 * 240 * 4);
        src.stop();
    }
}
