//! Dynamic POS OSD text store with per-slot TTL and snapshot generation.

use std::collections::BTreeMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use nvrdesk_core::{PosConfig, PosOsdLevel, PosOsdLine, PosOverlaySnapshot};

struct StoredEntry {
    line: PosOsdLine,
    expires_at: Option<Instant>,
}

/// Thread-safe store of active POS transaction/alert OSD lines keyed by `slot`.
#[derive(Default)]
pub struct PosOsdStore {
    entries: RwLock<BTreeMap<String, StoredEntry>>,
}

impl PosOsdStore {
    /// Creates an empty POS OSD store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces `slot` with `text` and optional expiration `ttl_ms` (`0` = sticky).
    pub fn set_line(&self, slot: String, text: String, level: PosOsdLevel, ttl_ms: u64) {
        let slot_key = if slot.trim().is_empty() {
            "default".to_string()
        } else {
            slot.trim().to_string()
        };
        let trimmed = text.trim().to_string();
        let mut entries = self.entries.write().expect("pos osd lock");
        if trimmed.is_empty() {
            entries.remove(&slot_key);
            return;
        }
        let expires_at = (ttl_ms > 0).then(|| Instant::now() + Duration::from_millis(ttl_ms));
        entries.insert(
            slot_key.clone(),
            StoredEntry {
                line: PosOsdLine {
                    slot: slot_key,
                    text: trimmed,
                    level,
                },
                expires_at,
            },
        );
    }

    /// Removes one slot from the OSD store.
    pub fn clear_slot(&self, slot: &str) {
        self.entries.write().expect("pos osd lock").remove(slot);
    }

    /// Removes all dynamic OSD lines.
    pub fn clear_all(&self) {
        self.entries.write().expect("pos osd lock").clear();
    }

    /// Returns currently active (non-expired) OSD lines, pruning expired items.
    #[must_use]
    pub fn active_lines(&self, now: Instant) -> Vec<PosOsdLine> {
        let mut entries = self.entries.write().expect("pos osd lock");
        entries.retain(|_, v| v.expires_at.is_none_or(|exp| exp > now));
        entries.values().map(|v| v.line.clone()).collect()
    }

    /// Builds a [`PosOverlaySnapshot`] when either privacy masks or POS OSD lines are active.
    #[must_use]
    pub fn snapshot(&self, pos_cfg: &PosConfig, now: Instant) -> Option<PosOverlaySnapshot> {
        let lines = if pos_cfg.osd_enabled {
            self.active_lines(now)
        } else {
            Vec::new()
        };
        if lines.is_empty() && pos_cfg.privacy_masks.is_empty() {
            return None;
        }
        Some(PosOverlaySnapshot {
            lines,
            privacy_masks: pos_cfg.privacy_masks.clone(),
            osd_position: pos_cfg.osd_position,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_expires_pos_osd_lines() {
        let store = PosOsdStore::new();
        store.set_line(
            "ticket".into(),
            "Ticket #99 | $120.00".into(),
            PosOsdLevel::Info,
            5_000,
        );
        store.set_line(
            "drawer".into(),
            "CAJON ABIERTO".into(),
            PosOsdLevel::Alert,
            0,
        );
        let lines = store.active_lines(Instant::now());
        assert_eq!(lines.len(), 2);

        let future = Instant::now() + Duration::from_secs(10);
        let after = store.active_lines(future);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].slot, "drawer");
    }
}
