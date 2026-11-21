//! Cross-platform microphone audio capture and resampling to 8 kHz mono PCM via `cpal`.

use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, RecvTimeoutError, SyncSender},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::{
    SampleFormat,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use nvrdesk_core::{DeviceEntry, NvrDeskError};
use tracing::{debug, warn};

const TARGET_SAMPLE_RATE: u32 = 8000;
const CHUNK_SAMPLES: usize = 160; // 20 ms at 8000 Hz

/// Lists available microphone input devices on the host OS.
#[must_use]
pub fn list_microphones() -> Vec<DeviceEntry> {
    let host = cpal::default_host();
    let default_name = host
        .default_input_device()
        .map(|d| d.to_string())
        .unwrap_or_default();

    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };

    devices
        .enumerate()
        .map(|(idx, dev)| {
            let name = dev.to_string();
            let detail = dev
                .default_input_config()
                .map(|c| format!("{} Hz, {} ch", c.sample_rate(), c.channels()))
                .unwrap_or_else(|_| "unknown".to_string());
            let is_default = name == default_name;
            DeviceEntry {
                index: idx,
                name,
                detail,
                is_default,
            }
        })
        .collect()
}

/// Captures microphone audio and emits 20 ms chunks of mono 8 kHz `i16` PCM samples.
pub struct AudioCaptureSource {
    device_name: Option<String>,
    running: Arc<AtomicBool>,
    rx: Option<mpsc::Receiver<Vec<i16>>>,
    worker: Option<JoinHandle<()>>,
}

impl AudioCaptureSource {
    /// Creates a new microphone capture source (default device when `device_name` is `None`).
    #[must_use]
    pub fn new(device_name: Option<String>) -> Self {
        Self {
            device_name,
            running: Arc::new(AtomicBool::new(false)),
            rx: None,
            worker: None,
        }
    }

    /// Starts capturing audio on a dedicated thread.
    pub fn start(&mut self) -> Result<(), NvrDeskError> {
        if self.running.load(Ordering::Acquire) {
            return Ok(());
        }

        let (tx, rx) = mpsc::sync_channel(32);
        let (init_tx, init_rx) = mpsc::sync_channel(1);
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::Release);
        let device_name = self.device_name.clone();

        let worker = thread::Builder::new()
            .name("nvrdesk-audio-capture".into())
            .spawn(move || {
                run_audio_thread(device_name.as_deref(), tx, init_tx, &running);
            })
            .map_err(|e| NvrDeskError::Capture(format!("failed to spawn audio thread: {e}")))?;

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
                    "audio worker terminated during initialization".into(),
                ))
            }
        }
    }

    /// Waits up to `timeout` for the next 20 ms chunk (160 mono 8 kHz `i16` samples).
    pub fn try_next_chunk(&mut self, timeout: Duration) -> Result<Option<Vec<i16>>, NvrDeskError> {
        let Some(rx) = &self.rx else {
            return Err(NvrDeskError::Capture("audio source not started".into()));
        };
        match rx.recv_timeout(timeout) {
            Ok(chunk) => Ok(Some(chunk)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                Err(NvrDeskError::Capture("audio capture disconnected".into()))
            }
        }
    }

    /// Stops audio capture cleanly.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.rx.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for AudioCaptureSource {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_audio_thread(
    device_name: Option<&str>,
    tx: SyncSender<Vec<i16>>,
    init_tx: SyncSender<Result<(), NvrDeskError>>,
    running: &AtomicBool,
) {
    let host = cpal::default_host();
    let device = if let Some(target_name) = device_name {
        host.input_devices()
            .ok()
            .and_then(|mut devs| devs.find(|d| d.to_string().contains(target_name)))
            .or_else(|| host.default_input_device())
    } else {
        host.default_input_device()
    };

    let Some(device) = device else {
        let _ = init_tx.send(Err(NvrDeskError::Capture(
            "no microphone input device available".into(),
        )));
        return;
    };

    let supported = match device.default_input_config() {
        Ok(cfg) => cfg,
        Err(err) => {
            let _ = init_tx.send(Err(NvrDeskError::Capture(format!(
                "failed to query microphone config: {err}"
            ))));
            return;
        }
    };

    let sample_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let sample_format = supported.sample_format();
    let config = supported.into();

    let mut resampler = MonoResampler::new(sample_rate, TARGET_SAMPLE_RATE);

    let err_cb = |err| warn!(error = %err, "cpal audio input stream error");

    let stream_res = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _| {
                push_mono_samples(
                    data.chunks(channels).map(|frame| {
                        let sum: f32 = frame.iter().copied().sum();
                        ((sum / channels as f32).clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
                    }),
                    &mut resampler,
                    &tx,
                );
            },
            err_cb,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _| {
                push_mono_samples(
                    data.chunks(channels).map(|frame| {
                        let sum: i32 = frame.iter().map(|&s| i32::from(s)).sum();
                        (sum / channels as i32) as i16
                    }),
                    &mut resampler,
                    &tx,
                );
            },
            err_cb,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            config,
            move |data: &[u16], _| {
                push_mono_samples(
                    data.chunks(channels).map(|frame| {
                        let sum: i32 = frame.iter().map(|&s| i32::from(s) - 32768).sum();
                        (sum / channels as i32) as i16
                    }),
                    &mut resampler,
                    &tx,
                );
            },
            err_cb,
            None,
        ),
        other => {
            let _ = init_tx.send(Err(NvrDeskError::Capture(format!(
                "unsupported microphone sample format: {other:?}"
            ))));
            return;
        }
    };

    let stream = match stream_res {
        Ok(s) => s,
        Err(err) => {
            let _ = init_tx.send(Err(NvrDeskError::Capture(format!(
                "failed to open microphone stream: {err}"
            ))));
            return;
        }
    };

    if let Err(err) = stream.play() {
        let _ = init_tx.send(Err(NvrDeskError::Capture(format!(
            "failed to start microphone stream: {err}"
        ))));
        return;
    }

    debug!(
        sample_rate,
        channels,
        ?sample_format,
        "microphone capture started"
    );
    let _ = init_tx.send(Ok(()));

    while running.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(50));
    }
    drop(stream);
}

fn push_mono_samples<I: Iterator<Item = i16>>(
    samples: I,
    resampler: &mut MonoResampler,
    tx: &SyncSender<Vec<i16>>,
) {
    resampler.push_samples(samples, |chunk| {
        let _ = tx.try_send(chunk);
    });
}

struct MonoResampler {
    step: f64,
    phase: f64,
    prev: i16,
    out_buf: Vec<i16>,
}

impl MonoResampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            step: f64::from(src_rate.max(1)) / f64::from(dst_rate.max(1)),
            phase: 0.0,
            prev: 0,
            out_buf: Vec::with_capacity(CHUNK_SAMPLES),
        }
    }

    fn push_samples<I: Iterator<Item = i16>, F: FnMut(Vec<i16>)>(
        &mut self,
        samples: I,
        mut on_chunk: F,
    ) {
        for cur in samples {
            while self.phase < 1.0 {
                let interpolated =
                    f64::from(self.prev) * (1.0 - self.phase) + f64::from(cur) * self.phase;
                self.out_buf.push(interpolated.round() as i16);
                if self.out_buf.len() >= CHUNK_SAMPLES {
                    on_chunk(std::mem::replace(
                        &mut self.out_buf,
                        Vec::with_capacity(CHUNK_SAMPLES),
                    ));
                }
                self.phase += self.step;
            }
            self.phase -= 1.0;
            self.prev = cur;
        }
    }
}

/// Audio playback sink for ONVIF Profile T Backchannel (VMS -> POS speakers).
pub struct AudioOutputSink {
    queue: Arc<Mutex<VecDeque<i16>>>,
    dst_rate: Arc<Mutex<u32>>,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl AudioOutputSink {
    /// Creates an audio backchannel output sink targeting the default speaker device.
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::with_capacity(16_384))),
            dst_rate: Arc::new(Mutex::new(8_000)),
            running: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    /// Starts audio playback on the default output device.
    pub fn start(&mut self) -> Result<(), NvrDeskError> {
        if self.running.load(Ordering::Acquire) {
            return Ok(());
        }
        let (init_tx, init_rx) = mpsc::sync_channel(1);
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::Release);
        let queue = Arc::clone(&self.queue);
        let dst_rate = Arc::clone(&self.dst_rate);

        let worker = thread::Builder::new()
            .name("nvrdesk-audio-backchannel".into())
            .spawn(move || {
                run_audio_output_thread(queue, dst_rate, init_tx, &running);
            })
            .map_err(|e| {
                NvrDeskError::Capture(format!("failed to spawn audio output thread: {e}"))
            })?;

        match init_rx.recv() {
            Ok(Ok(())) => {
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
                    "audio output worker terminated during initialization".into(),
                ))
            }
        }
    }

    /// Pushes mono 8 kHz `i16` PCM samples received from the VMS backchannel.
    pub fn push_samples_8khz(&self, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        let target_rate = *self.dst_rate.lock().expect("dst_rate lock");
        let ratio = (target_rate as f64 / f64::from(TARGET_SAMPLE_RATE)).max(1.0);
        let mut q = self.queue.lock().expect("audio output queue lock");
        for &s in samples {
            let repeats = ratio.round() as usize;
            for _ in 0..repeats.max(1) {
                if q.len() >= 48_000 {
                    q.pop_front();
                }
                q.push_back(s);
            }
        }
    }

    /// Stops audio playback cleanly.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.queue.lock().expect("audio output queue lock").clear();
    }
}

impl Default for AudioOutputSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AudioOutputSink {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_audio_output_thread(
    queue: Arc<Mutex<VecDeque<i16>>>,
    dst_rate: Arc<Mutex<u32>>,
    init_tx: SyncSender<Result<(), NvrDeskError>>,
    running: &AtomicBool,
) {
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        let _ = init_tx.send(Err(NvrDeskError::Capture(
            "no audio output device available".into(),
        )));
        return;
    };
    let supported = match device.default_output_config() {
        Ok(cfg) => cfg,
        Err(err) => {
            let _ = init_tx.send(Err(NvrDeskError::Capture(format!(
                "failed to query audio output config: {err}"
            ))));
            return;
        }
    };
    let sample_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let sample_format = supported.sample_format();
    *dst_rate.lock().expect("dst_rate lock") = sample_rate;
    let config = supported.into();

    let err_cb = |err| warn!(error = %err, "cpal output stream error");
    let q_cb = Arc::clone(&queue);
    let stream_res = match sample_format {
        SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _| {
                let mut q = q_cb.lock().expect("audio out lock");
                for frame in data.chunks_mut(channels.max(1)) {
                    let v = f32::from(q.pop_front().unwrap_or(0)) / 32768.0;
                    frame.fill(v);
                }
            },
            err_cb,
            None,
        ),
        SampleFormat::I16 => device.build_output_stream(
            config,
            move |data: &mut [i16], _| {
                let mut q = q_cb.lock().expect("audio out lock");
                for frame in data.chunks_mut(channels.max(1)) {
                    let v = q.pop_front().unwrap_or(0);
                    frame.fill(v);
                }
            },
            err_cb,
            None,
        ),
        SampleFormat::U16 => device.build_output_stream(
            config,
            move |data: &mut [u16], _| {
                let mut q = q_cb.lock().expect("audio out lock");
                for frame in data.chunks_mut(channels.max(1)) {
                    let s = q.pop_front().unwrap_or(0);
                    let v = (i32::from(s) + 32768).clamp(0, 65535) as u16;
                    frame.fill(v);
                }
            },
            err_cb,
            None,
        ),
        other => {
            let _ = init_tx.send(Err(NvrDeskError::Capture(format!(
                "unsupported audio output format: {other:?}"
            ))));
            return;
        }
    };

    let stream = match stream_res {
        Ok(s) => s,
        Err(err) => {
            let _ = init_tx.send(Err(NvrDeskError::Capture(format!(
                "failed to open audio output stream: {err}"
            ))));
            return;
        }
    };

    if let Err(err) = stream.play() {
        let _ = init_tx.send(Err(NvrDeskError::Capture(format!(
            "failed to start audio output stream: {err}"
        ))));
        return;
    }

    debug!(
        sample_rate,
        channels,
        ?sample_format,
        "audio backchannel output started"
    );
    let _ = init_tx.send(Ok(()));
    while running.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(50));
    }
    drop(stream);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_48khz_to_8khz_in_160_sample_chunks() {
        let mut r = MonoResampler::new(48_000, 8_000);
        let input: Vec<i16> = (0..960).map(|i| (i % 200) as i16).collect(); // 20ms @ 48kHz
        let mut chunks = Vec::new();
        r.push_samples(input.into_iter(), |c| chunks.push(c));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 160);
    }
}
