//! FFmpeg CLI pull capture source for MP4 test media and legacy MJPEG/RTMP/RTSP restreams.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, RecvTimeoutError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nvrdesk_core::{CaptureSourceKind, MonitorBounds, NvrDeskError, PixelFormat, RawVideoFrame};
use tracing::{debug, warn};

use crate::CaptureSource;

/// Capture source pulling raw BGRA video frames from an external `ffmpeg` CLI process.
///
/// Supports local/HTTP MP4 files (`mp4`), legacy MJPEG streams (`mjpeg`), live RTMP
/// streams (`rtmp`), upstream RTSP relays (`rtsp_relay`), and arbitrary FFmpeg URIs (`ffmpeg_uri`).
pub struct FfmpegPullSource {
    name: String,
    kind: CaptureSourceKind,
    uri: String,
    ffmpeg_path: String,
    loop_input: bool,
    width: u32,
    height: u32,
    fps: u32,
    running: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    rx: Option<mpsc::Receiver<Result<RawVideoFrame, NvrDeskError>>>,
    worker: Option<JoinHandle<()>>,
}

impl FfmpegPullSource {
    /// Creates a new FFmpeg pull capture source with even target dimensions.
    #[must_use]
    pub fn new(
        kind: CaptureSourceKind,
        uri: String,
        ffmpeg_path: String,
        loop_input: bool,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Self {
        let even_w = (width.max(64)) & !1;
        let even_h = (height.max(64)) & !1;
        Self {
            name: format!("ffmpeg-pull:{kind:?}:{uri}"),
            kind,
            uri,
            ffmpeg_path: if ffmpeg_path.trim().is_empty() {
                "ffmpeg".to_string()
            } else {
                ffmpeg_path
            },
            loop_input,
            width: even_w,
            height: even_h,
            fps: fps.max(1),
            running: Arc::new(AtomicBool::new(false)),
            child: Arc::new(Mutex::new(None)),
            rx: None,
            worker: None,
        }
    }
}

/// Builds FFmpeg CLI arguments for decoding a file or network stream to raw BGRA frames.
pub fn build_ffmpeg_pull_args(
    kind: CaptureSourceKind,
    uri: &str,
    loop_input: bool,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Vec<String>, NvrDeskError> {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return Err(NvrDeskError::Capture(
            "source.uri must be set for MP4/MJPEG/RTMP/RTSP relay sources".into(),
        ));
    }

    let even_w = (width.max(64)) & !1;
    let even_h = (height.max(64)) & !1;
    let fps = fps.max(1);

    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
    ];

    match kind {
        CaptureSourceKind::Mp4 => {
            args.push("-re".to_string());
            if loop_input {
                args.push("-stream_loop".to_string());
                args.push("-1".to_string());
            }
        }
        CaptureSourceKind::Mjpeg | CaptureSourceKind::Rtmp | CaptureSourceKind::FfmpegUri => {
            args.push("-fflags".to_string());
            args.push("nobuffer".to_string());
            args.push("-flags".to_string());
            args.push("low_delay".to_string());
        }
        CaptureSourceKind::RtspRelay => {
            args.push("-rtsp_transport".to_string());
            args.push("tcp".to_string());
            args.push("-fflags".to_string());
            args.push("nobuffer".to_string());
            args.push("-flags".to_string());
            args.push("low_delay".to_string());
        }
        CaptureSourceKind::Screen | CaptureSourceKind::TestPattern => {
            return Err(NvrDeskError::Capture(format!(
                "source kind {kind:?} is not an FFmpeg pull source"
            )));
        }
    }

    args.extend([
        "-i".to_string(),
        trimmed.to_string(),
        "-an".to_string(),
        "-sn".to_string(),
        "-dn".to_string(),
        "-vf".to_string(),
        format!("scale={even_w}:{even_h}:flags=bilinear,fps={fps}"),
        "-pix_fmt".to_string(),
        "bgra".to_string(),
        "-f".to_string(),
        "rawvideo".to_string(),
        "pipe:1".to_string(),
    ]);
    Ok(args)
}

impl CaptureSource for FfmpegPullSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn bounds(&self) -> Option<MonitorBounds> {
        Some(MonitorBounds {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        })
    }

    fn start(&mut self) -> Result<(), NvrDeskError> {
        if self.running.load(Ordering::Acquire) {
            return Ok(());
        }

        let args = build_ffmpeg_pull_args(
            self.kind,
            &self.uri,
            self.loop_input,
            self.width,
            self.height,
            self.fps,
        )?;
        let mut child = Command::new(&self.ffmpeg_path)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                NvrDeskError::Capture(format!(
                    "failed to launch ffmpeg pull source ({}): {e}",
                    self.ffmpeg_path
                ))
            })?;

        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            return Err(NvrDeskError::Capture(
                "ffmpeg child stdout unavailable".into(),
            ));
        };

        *self.child.lock().expect("ffmpeg child lock") = Some(child);
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::Release);

        let (tx, rx) = mpsc::sync_channel(2);
        let width = self.width;
        let height = self.height;
        let child_arc = Arc::clone(&self.child);

        let worker = thread::Builder::new()
            .name("nvrdesk-ffmpeg-pull".into())
            .spawn(move || {
                let stride = (width as usize) * 4;
                let frame_len = stride * (height as usize);
                let start = Instant::now();

                while running.load(Ordering::Acquire) {
                    let mut buf = vec![0u8; frame_len];
                    match stdout.read_exact(&mut buf) {
                        Ok(()) => {
                            let frame = RawVideoFrame {
                                width,
                                height,
                                stride,
                                format: PixelFormat::Bgra8,
                                data: buf,
                                timestamp_us: start.elapsed().as_micros() as u64,
                            };
                            if let Err(mpsc::TrySendError::Disconnected(_)) = tx.try_send(Ok(frame))
                            {
                                break;
                            }
                        }
                        Err(err) => {
                            if running.load(Ordering::Acquire) {
                                warn!(error = %err, "ffmpeg pull stream ended");
                                let _ = tx.try_send(Err(NvrDeskError::Capture(format!(
                                    "ffmpeg pull stream ended: {err}"
                                ))));
                            }
                            break;
                        }
                    }
                }

                if let Some(mut c) = child_arc.lock().expect("ffmpeg child lock").take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                debug!("ffmpeg pull capture loop exited");
            })
            .map_err(|e| NvrDeskError::Capture(format!("failed to spawn ffmpeg thread: {e}")))?;

        self.rx = Some(rx);
        self.worker = Some(worker);
        Ok(())
    }

    fn try_next_frame(&mut self, timeout: Duration) -> Result<Option<RawVideoFrame>, NvrDeskError> {
        let Some(rx) = &self.rx else {
            return Err(NvrDeskError::Capture(
                "ffmpeg pull source not started".into(),
            ));
        };
        match rx.recv_timeout(timeout) {
            Ok(Ok(frame)) => Ok(Some(frame)),
            Ok(Err(err)) => Err(err),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(NvrDeskError::Capture(
                "ffmpeg pull worker disconnected".into(),
            )),
        }
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(mut child) = self.child.lock().expect("ffmpeg child lock").take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.rx.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for FfmpegPullSource {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_expected_args_for_mp4_mjpeg_rtmp_and_rtsp() {
        let mp4 = build_ffmpeg_pull_args(
            CaptureSourceKind::Mp4,
            "/tmp/pos-demo.mp4",
            true,
            1281,
            721,
            15,
        )
        .expect("mp4 args");
        assert!(mp4.contains(&"-re".to_string()));
        assert!(mp4.contains(&"-stream_loop".to_string()));
        assert!(mp4.contains(&"scale=1280:720:flags=bilinear,fps=15".to_string()));

        let rtsp = build_ffmpeg_pull_args(
            CaptureSourceKind::RtspRelay,
            "rtsp://cam/stream1",
            false,
            640,
            360,
            10,
        )
        .expect("rtsp args");
        assert!(rtsp.contains(&"-rtsp_transport".to_string()));
        assert!(rtsp.contains(&"tcp".to_string()));

        let empty = build_ffmpeg_pull_args(CaptureSourceKind::Mjpeg, "   ", false, 640, 480, 10);
        assert!(empty.is_err());
    }
}
