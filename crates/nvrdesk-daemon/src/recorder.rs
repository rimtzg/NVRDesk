//! Local ring-buffer Annex-B H.264 segment recorder with disk quota eviction.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use chrono::Local;
use nvrdesk_core::{EncodedFrame, NvrDeskError, RecordingConfig};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Current disk usage statistics of the local ring recorder.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordingStats {
    pub enabled: bool,
    pub segment_count: usize,
    pub total_bytes: u64,
}

/// Rotated Annex-B H.264 recorder that enforces a maximum directory quota.
pub struct RingRecorder {
    cfg: RecordingConfig,
    writer: Option<BufWriter<File>>,
    segment_started_at: Instant,
    awaiting_keyframe: bool,
    segment_seq: u64,
}

impl RingRecorder {
    /// Creates a new `RingRecorder` targeting `cfg.directory`.
    pub fn new(cfg: RecordingConfig) -> Result<Self, NvrDeskError> {
        if cfg.enabled {
            fs::create_dir_all(&cfg.directory)?;
        }
        Ok(Self {
            cfg,
            writer: None,
            segment_started_at: Instant::now(),
            awaiting_keyframe: true,
            segment_seq: 0,
        })
    }

    /// Updates recorder settings on live config reload.
    pub fn update_config(&mut self, cfg: RecordingConfig) {
        if !cfg.enabled {
            self.flush_and_close();
        } else if !self.cfg.enabled || self.cfg.directory != cfg.directory {
            self.flush_and_close();
            if let Err(err) = fs::create_dir_all(&cfg.directory) {
                warn!(error = %err, "failed to create recording directory");
            }
        }
        self.cfg = cfg;
    }

    /// Writes an encoded H.264 access unit, rotating cleanly on IDR keyframes.
    pub fn write_frame(&mut self, frame: &EncodedFrame) {
        if !self.cfg.enabled {
            return;
        }

        let seg_duration = Duration::from_secs(self.cfg.segment_secs.max(10));
        let should_rotate = self.writer.is_none()
            || (frame.is_keyframe && self.segment_started_at.elapsed() >= seg_duration);

        if should_rotate {
            if !frame.is_keyframe && self.writer.is_none() {
                return;
            }
            if frame.is_keyframe
                && let Err(err) = self.rotate_segment(frame)
            {
                warn!(error = %err, "failed to rotate recording segment");
                return;
            }
        }

        if self.awaiting_keyframe {
            if !frame.is_keyframe {
                return;
            }
            self.awaiting_keyframe = false;
        }

        let Some(writer) = self.writer.as_mut() else {
            return;
        };

        if frame.is_keyframe {
            let has_sps = frame
                .nals
                .iter()
                .any(|n| !n.is_empty() && (n[0] & 0x1F) == 7);
            let has_pps = frame
                .nals
                .iter()
                .any(|n| !n.is_empty() && (n[0] & 0x1F) == 8);
            if !has_sps && let Some(sps) = &frame.sps {
                let _ = write_annex_b_nal(writer, sps);
            }
            if !has_pps && let Some(pps) = &frame.pps {
                let _ = write_annex_b_nal(writer, pps);
            }
        }

        for nal in &frame.nals {
            if let Err(err) = write_annex_b_nal(writer, nal) {
                warn!(error = %err, "failed to write recording frame");
                self.flush_and_close();
                return;
            }
        }
    }

    /// Flushes and closes the active segment file.
    pub fn flush_and_close(&mut self) {
        if let Some(mut w) = self.writer.take() {
            let _ = w.flush();
        }
        self.awaiting_keyframe = true;
    }

    /// Computes current segment count and disk footprint.
    #[must_use]
    pub fn stats(&self) -> RecordingStats {
        if !self.cfg.enabled {
            return RecordingStats::default();
        }
        let entries = list_segments_oldest_first(&self.cfg.directory);
        let total_bytes = entries.iter().map(|(_, sz, _)| *sz).sum();
        RecordingStats {
            enabled: true,
            segment_count: entries.len(),
            total_bytes,
        }
    }

    fn rotate_segment(&mut self, _first_frame: &EncodedFrame) -> Result<(), NvrDeskError> {
        self.flush_and_close();
        fs::create_dir_all(&self.cfg.directory)?;
        self.prune_quota()?;

        self.segment_seq = self.segment_seq.wrapping_add(1);
        let stamp = Local::now().format("%Y%m%d_%H%M%S");
        let filename = format!("nvrdesk_{stamp}_{:04}.h264", self.segment_seq);
        let path = self.cfg.directory.join(filename);
        let file = File::create(&path)?;
        self.writer = Some(BufWriter::with_capacity(64 * 1024, file));
        self.segment_started_at = Instant::now();
        self.awaiting_keyframe = false;
        debug!(path = %path.display(), "opened new recording segment");
        Ok(())
    }

    fn prune_quota(&self) -> Result<(), NvrDeskError> {
        let max_bytes = self.cfg.max_total_mb.max(1) * 1024 * 1024;
        let mut files = list_segments_oldest_first(&self.cfg.directory);
        let mut total: u64 = files.iter().map(|(_, sz, _)| *sz).sum();

        while total > max_bytes && !files.is_empty() {
            let (oldest_path, sz, _) = files.remove(0);
            if let Err(err) = fs::remove_file(&oldest_path) {
                warn!(path = %oldest_path.display(), error = %err, "failed to prune old recording");
                break;
            }
            total = total.saturating_sub(sz);
            info!(path = %oldest_path.display(), remaining_bytes = total, "pruned oldest recording segment");
        }
        Ok(())
    }
}

impl Drop for RingRecorder {
    fn drop(&mut self) {
        self.flush_and_close();
    }
}

fn write_annex_b_nal(writer: &mut BufWriter<File>, nal: &[u8]) -> std::io::Result<()> {
    writer.write_all(&[0, 0, 0, 1])?;
    writer.write_all(nal)
}

fn list_segments_oldest_first(dir: &PathBuf) -> Vec<(PathBuf, u64, SystemTime)> {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("nvrdesk_") || !name.ends_with(".h264") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            items.push((path, meta.len(), mtime));
        }
    }
    items.sort_by_key(|(path, _, mtime)| (*mtime, path.clone()));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn writes_annex_b_segments_and_evicts_over_quota() {
        let dir = std::env::temp_dir().join(format!(
            "nvrdesk_rec_test_{}",
            std::time::SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cfg = RecordingConfig {
            enabled: true,
            directory: dir.clone(),
            segment_secs: 60,
            max_total_mb: 1,
        };
        let mut rec = RingRecorder::new(cfg).expect("init recorder");
        let idr = EncodedFrame {
            codec: nvrdesk_core::VideoCodecKind::H264,
            nals: vec![Bytes::from_static(&[0x65, 0x88, 0x84])],
            vps: None,
            sps: Some(Bytes::from_static(&[0x67, 0x42, 0x00, 0x1E])),
            pps: Some(Bytes::from_static(&[0x68, 0xCE, 0x3C, 0x80])),
            is_keyframe: true,
            pts_90khz: 0,
        };
        rec.write_frame(&idr);
        rec.flush_and_close();

        let st = rec.stats();
        assert_eq!(st.segment_count, 1);
        assert!(st.total_bytes >= 15);
        let _ = fs::remove_dir_all(&dir);
    }
}
