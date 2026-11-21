//! Cross-platform webcam capture backend via `nokhwa`.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, RecvTimeoutError, SyncSender},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nokhwa::{
    Camera,
    pixel_format::RgbAFormat,
    utils::{ApiBackend, CameraIndex, RequestedFormat, RequestedFormatType},
};
use nvrdesk_core::{DeviceEntry, NvrDeskError, PixelFormat, RawVideoFrame};
use tracing::{debug, warn};

use crate::CaptureSource;

/// Lists available webcam devices on the host OS.
#[must_use]
pub fn list_webcams() -> Vec<DeviceEntry> {
    let Ok(cameras) = nokhwa::query(ApiBackend::Auto) else {
        return Vec::new();
    };
    cameras
        .into_iter()
        .enumerate()
        .map(|(i, info)| DeviceEntry {
            index: i,
            name: info.human_name(),
            detail: info.description().to_string(),
            is_default: i == 0,
        })
        .collect()
}

/// Cross-platform webcam capture source backed by `nokhwa`.
pub struct WebcamCaptureSource {
    name: String,
    device_index: usize,
    running: Arc<AtomicBool>,
    rx: Option<mpsc::Receiver<Result<RawVideoFrame, NvrDeskError>>>,
    worker: Option<JoinHandle<()>>,
}

impl WebcamCaptureSource {
    /// Creates a new webcam source targeting `device_index`.
    #[must_use]
    pub fn new(device_index: usize) -> Self {
        Self {
            name: format!("webcam:{device_index}"),
            device_index,
            running: Arc::new(AtomicBool::new(false)),
            rx: None,
            worker: None,
        }
    }
}

impl CaptureSource for WebcamCaptureSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn start(&mut self) -> Result<(), NvrDeskError> {
        if self.running.load(Ordering::Acquire) {
            return Ok(());
        }

        let (tx, rx) = mpsc::sync_channel(2);
        let (init_tx, init_rx) = mpsc::sync_channel(1);
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::Release);
        let device_index = self.device_index;

        let worker = thread::Builder::new()
            .name("nvrdesk-webcam-capture".into())
            .spawn(move || {
                let requested = RequestedFormat::new::<RgbAFormat>(
                    RequestedFormatType::AbsoluteHighestFrameRate,
                );
                let mut camera =
                    match Camera::new(CameraIndex::Index(device_index as u32), requested) {
                        Ok(cam) => cam,
                        Err(err) => {
                            let _ = init_tx.send(Err(NvrDeskError::Capture(format!(
                                "failed to open webcam #{device_index}: {err}"
                            ))));
                            return;
                        }
                    };

                if let Err(err) = camera.open_stream() {
                    let _ = init_tx.send(Err(NvrDeskError::Capture(format!(
                        "failed to start webcam stream: {err}"
                    ))));
                    return;
                }

                debug!(device_index, "webcam capture started");
                let _ = init_tx.send(Ok(()));

                let start_time = Instant::now();
                webcam_loop(&mut camera, &tx, &running, start_time);

                if let Err(err) = camera.stop_stream() {
                    warn!(error = %err, "failed to stop webcam stream cleanly");
                }
            })
            .map_err(|e| NvrDeskError::Capture(format!("failed to spawn webcam thread: {e}")))?;

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
                    "webcam worker terminated during initialization".into(),
                ))
            }
        }
    }

    fn try_next_frame(&mut self, timeout: Duration) -> Result<Option<RawVideoFrame>, NvrDeskError> {
        let Some(rx) = &self.rx else {
            return Err(NvrDeskError::Capture("webcam source not started".into()));
        };

        match rx.recv_timeout(timeout) {
            Ok(Ok(frame)) => Ok(Some(frame)),
            Ok(Err(err)) => Err(err),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(NvrDeskError::Capture(
                "webcam worker stopped unexpectedly".into(),
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

impl Drop for WebcamCaptureSource {
    fn drop(&mut self) {
        self.stop();
    }
}

fn webcam_loop(
    camera: &mut Camera,
    tx: &SyncSender<Result<RawVideoFrame, NvrDeskError>>,
    running: &AtomicBool,
    start_time: Instant,
) {
    while running.load(Ordering::Acquire) {
        match camera.frame() {
            Ok(buf) => match buf.decode_image::<RgbAFormat>() {
                Ok(rgba) => {
                    let width = rgba.width();
                    let height = rgba.height();
                    let raw = RawVideoFrame {
                        width,
                        height,
                        stride: (width as usize) * 4,
                        format: PixelFormat::Rgba8,
                        data: rgba.into_raw(),
                        timestamp_us: start_time.elapsed().as_micros() as u64,
                    };
                    if let Err(mpsc::TrySendError::Disconnected(_)) = tx.try_send(Ok(raw)) {
                        break;
                    }
                }
                Err(err) => {
                    warn!(error = %err, "webcam RGBA decode error");
                }
            },
            Err(err) => {
                warn!(error = %err, "webcam frame read error");
                let _ = tx.try_send(Err(NvrDeskError::Capture(err.to_string())));
                break;
            }
        }
    }
}
