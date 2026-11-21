//! Optional external FFmpeg CLI pipe encoder (keeps NVRDesk 100% MIT-licensed).

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use bytes::Bytes;
use nvrdesk_core::{EncodedFrame, I420Frame, NvrDeskError, StreamConfig};
use tracing::{debug, info, warn};

use crate::{VideoEncoder, split_annex_b_nals};

/// Probes `ffmpeg_path` for hardware H.264 encoders suitable for the current OS.
#[must_use]
pub fn probe_hw_h264_codec(ffmpeg_path: &str) -> Option<&'static str> {
    let output = Command::new(ffmpeg_path)
        .args(["-hide_banner", "-encoders"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);

    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["h264_videotoolbox"]
    } else if cfg!(target_os = "windows") {
        &["h264_nvenc", "h264_qsv", "h264_amf"]
    } else {
        &["h264_nvenc", "h264_vaapi", "h264_qsv"]
    };

    for &codec in candidates {
        if text.contains(codec) && validate_codec_smoke(ffmpeg_path, codec) {
            info!(codec, "detected working FFmpeg hardware H.264 encoder");
            return Some(codec);
        }
    }
    None
}

fn validate_codec_smoke(ffmpeg_path: &str, codec: &str) -> bool {
    let mut cmd = Command::new(ffmpeg_path);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "color=c=black:s=128x128:r=5",
        "-frames:v",
        "1",
        "-c:v",
        codec,
        "-f",
        "null",
        "-",
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::null());

    cmd.status().is_ok_and(|s| s.success())
}

/// External FFmpeg CLI pipe encoder supporting hardware and custom codecs.
pub struct FfmpegCliEncoder {
    ffmpeg_path: String,
    codec: String,
    cfg: StreamConfig,
    dimensions: Option<(u32, u32)>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    au_rx: Option<mpsc::Receiver<Vec<Bytes>>>,
    reader_thread: Option<JoinHandle<()>>,
    cached_sps: Option<Bytes>,
    cached_pps: Option<Bytes>,
    last_pts_90khz: u64,
}

impl FfmpegCliEncoder {
    /// Creates a new external FFmpeg pipe encoder with `codec` (e.g. `h264_videotoolbox`).
    #[must_use]
    pub fn new(ffmpeg_path: String, codec: String, cfg: &StreamConfig) -> Self {
        Self {
            ffmpeg_path,
            codec,
            cfg: cfg.clone(),
            dimensions: None,
            child: None,
            stdin: None,
            au_rx: None,
            reader_thread: None,
            cached_sps: None,
            cached_pps: None,
            last_pts_90khz: 0,
        }
    }

    fn spawn_for_dimensions(&mut self, width: u32, height: u32) -> Result<(), NvrDeskError> {
        self.stop_child();

        let fps = self.cfg.fps.max(1).to_string();
        let size = format!("{width}x{height}");
        let bitrate = self.cfg.bitrate_bps.max(100_000).to_string();
        let gop = self
            .cfg
            .fps
            .max(1)
            .saturating_mul(self.cfg.idr_interval_secs.max(1))
            .to_string();

        let mut args = vec![
            "-hide_banner".to_string(),
            "-loglevel".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "rawvideo".to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-s".to_string(),
            size,
            "-r".to_string(),
            fps,
            "-i".to_string(),
            "pipe:0".to_string(),
            "-an".to_string(),
            "-c:v".to_string(),
            self.codec.clone(),
            "-b:v".to_string(),
            bitrate,
            "-g".to_string(),
            gop,
            "-bf".to_string(),
            "0".to_string(),
        ];

        if self.codec == "libx264" {
            args.extend([
                "-preset".into(),
                "ultrafast".into(),
                "-tune".into(),
                "zerolatency".into(),
            ]);
        } else if self.codec == "h264_videotoolbox" {
            args.extend([
                "-realtime".into(),
                "true".into(),
                "-profile:v".into(),
                "baseline".into(),
            ]);
        }

        args.extend([
            "-f".to_string(),
            "h264".to_string(),
            "-flush_packets".to_string(),
            "1".to_string(),
            "pipe:1".to_string(),
        ]);

        let mut child = Command::new(&self.ffmpeg_path)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                NvrDeskError::Codec(format!("failed to spawn ffmpeg ({}): {e}", self.codec))
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| NvrDeskError::Codec("ffmpeg stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| NvrDeskError::Codec("ffmpeg stdout unavailable".into()))?;

        let (tx, rx) = mpsc::channel();
        let reader_thread = thread::Builder::new()
            .name("nvrdesk-ffmpeg-reader".into())
            .spawn(move || read_annex_b_stream(stdout, tx))
            .map_err(|e| NvrDeskError::Codec(format!("failed to spawn ffmpeg reader: {e}")))?;

        debug!(codec = %self.codec, width, height, "FFmpeg pipe encoder started");
        self.dimensions = Some((width, height));
        self.child = Some(child);
        self.stdin = Some(stdin);
        self.au_rx = Some(rx);
        self.reader_thread = Some(reader_thread);
        Ok(())
    }

    fn stop_child(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.au_rx.take();
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for FfmpegCliEncoder {
    fn drop(&mut self) {
        self.stop_child();
    }
}

impl VideoEncoder for FfmpegCliEncoder {
    fn name(&self) -> &str {
        &self.codec
    }

    fn encode(
        &mut self,
        frame: &I420Frame,
        _force_keyframe: bool,
    ) -> Result<EncodedFrame, NvrDeskError> {
        if self.dimensions != Some((frame.width, frame.height)) {
            self.spawn_for_dimensions(frame.width, frame.height)?;
        }

        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| NvrDeskError::Codec("ffmpeg stdin closed".into()))?;
        stdin
            .write_all(&frame.data)
            .map_err(|e| NvrDeskError::Codec(format!("ffmpeg write failed: {e}")))?;
        let _ = stdin.flush();

        let pts_90khz = frame.timestamp_us.saturating_mul(90) / 1_000;
        self.last_pts_90khz = pts_90khz;

        let rx = self
            .au_rx
            .as_ref()
            .ok_or_else(|| NvrDeskError::Codec("ffmpeg output channel closed".into()))?;

        let mut collected_nals = Vec::new();
        if let Ok(first) = rx.recv_timeout(Duration::from_millis(60)) {
            collected_nals.extend(first);
            while let Ok(more) = rx.try_recv() {
                collected_nals.extend(more);
            }
        }

        let mut is_keyframe = false;
        for nal in &collected_nals {
            if nal.is_empty() {
                continue;
            }
            match nal[0] & 0x1F {
                7 => self.cached_sps = Some(nal.clone()),
                8 => self.cached_pps = Some(nal.clone()),
                5 => is_keyframe = true,
                _ => {}
            }
        }

        Ok(EncodedFrame {
            nals: collected_nals,
            sps: self.cached_sps.clone(),
            pps: self.cached_pps.clone(),
            is_keyframe,
            pts_90khz,
        })
    }

    fn parameter_sets(&self) -> (Option<Bytes>, Option<Bytes>) {
        (self.cached_sps.clone(), self.cached_pps.clone())
    }
}

fn read_annex_b_stream(mut stdout: impl Read, tx: mpsc::Sender<Vec<Bytes>>) {
    let mut buf = Vec::with_capacity(128 * 1024);
    let mut tmp = [0u8; 32 * 1024];

    loop {
        match stdout.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(last_sc) = find_last_start_code(&buf)
                    && last_sc > 0
                {
                    let complete = &buf[..last_sc];
                    let nals: Vec<Bytes> = split_annex_b_nals(complete)
                        .into_iter()
                        .filter(|nal| !nal.is_empty())
                        .map(Bytes::copy_from_slice)
                        .collect();
                    if !nals.is_empty() && tx.send(nals).is_err() {
                        return;
                    }
                    buf.drain(..last_sc);
                }
            }
            Err(err) => {
                warn!(error = %err, "ffmpeg stdout read error");
                break;
            }
        }
    }

    if !buf.is_empty() {
        let nals: Vec<Bytes> = split_annex_b_nals(&buf)
            .into_iter()
            .filter(|nal| !nal.is_empty())
            .map(Bytes::copy_from_slice)
            .collect();
        if !nals.is_empty() {
            let _ = tx.send(nals);
        }
    }
}

fn find_last_start_code(data: &[u8]) -> Option<usize> {
    if data.len() < 4 {
        return None;
    }
    let mut idx = data.len() - 4;
    loop {
        if data[idx..idx + 4] == [0, 0, 0, 1] {
            return Some(idx);
        }
        if data[idx..idx + 3] == [0, 0, 1] {
            return Some(idx);
        }
        if idx == 0 {
            return None;
        }
        idx -= 1;
    }
}
