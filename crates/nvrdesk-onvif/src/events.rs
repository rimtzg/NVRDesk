//! ONVIF Profile S/T event broker for MotionAlarm and POS transaction/alert PullPoint subscriptions.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use uuid::Uuid;

const MAX_RETAINED_EVENTS: usize = 64;

/// A single ONVIF notification event stored by [`EventBroker`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnvifEventRecord {
    pub id: u64,
    pub utc_time: String,
    pub topic: String,
    pub source_name: String,
    pub source_value: String,
    pub data_name: String,
    pub data_value: String,
}

#[derive(Default)]
struct BrokerInner {
    next_id: u64,
    events: VecDeque<OnvifEventRecord>,
    subscriptions: HashMap<String, u64>,
}

/// Thread-safe event hub powering ONVIF `Events` / `PullPointSubscription` and Web UI telemetry.
#[derive(Clone, Default)]
pub struct EventBroker {
    inner: Arc<Mutex<BrokerInner>>,
    notify: Arc<Notify>,
}

impl EventBroker {
    /// Creates a new empty `EventBroker`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes an arbitrary ONVIF event and wakes any waiting `PullMessages` calls.
    pub fn publish(
        &self,
        topic: &str,
        source_name: &str,
        source_value: &str,
        data_name: &str,
        data_value: &str,
    ) -> OnvifEventRecord {
        let record = {
            let mut inner = self.inner.lock().expect("event broker lock");
            inner.next_id += 1;
            let rec = OnvifEventRecord {
                id: inner.next_id,
                utc_time: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                topic: topic.to_string(),
                source_name: source_name.to_string(),
                source_value: source_value.to_string(),
                data_name: data_name.to_string(),
                data_value: data_value.to_string(),
            };
            inner.events.push_back(rec.clone());
            while inner.events.len() > MAX_RETAINED_EVENTS {
                inner.events.pop_front();
            }
            rec
        };
        self.notify.notify_waiters();
        record
    }

    /// Publishes an ONVIF `tns1:VideoSource/MotionAlarm` state change.
    pub fn publish_motion(&self, source_token: &str, active: bool) -> OnvifEventRecord {
        self.publish(
            "tns1:VideoSource/MotionAlarm",
            "Source",
            source_token,
            "State",
            if active { "true" } else { "false" },
        )
    }

    /// Publishes a POS dynamic OSD / drawer / alert notification under `tns1:RuleEngine/PosEvent/Alert`.
    pub fn publish_pos_event(&self, slot: &str, text: &str, level: &str) -> OnvifEventRecord {
        self.publish(
            "tns1:RuleEngine/PosEvent/Alert",
            "Slot",
            slot,
            "Message",
            &format!("{level}: {text}"),
        )
    }

    /// Creates a new `PullPointSubscription` token starting at the next event ID.
    #[must_use]
    pub fn create_subscription(&self) -> String {
        let token = Uuid::new_v4().simple().to_string();
        let mut inner = self.inner.lock().expect("event broker lock");
        let cursor = inner.next_id + 1;
        inner.subscriptions.insert(token.clone(), cursor);
        token
    }

    /// Waits up to `timeout` for unread events on `sub_id`, returning `None` if subscription does not exist.
    pub async fn pull_messages(
        &self,
        sub_id: &str,
        timeout: Duration,
        limit: usize,
    ) -> Option<Vec<OnvifEventRecord>> {
        let max_items = limit.max(1);
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.lock().expect("event broker lock");
                let cursor = *inner.subscriptions.get(sub_id)?;
                let batch: Vec<OnvifEventRecord> = inner
                    .events
                    .iter()
                    .filter(|ev| ev.id >= cursor)
                    .take(max_items)
                    .cloned()
                    .collect();
                if !batch.is_empty() {
                    if let Some(last) = batch.last() {
                        inner.subscriptions.insert(sub_id.to_string(), last.id + 1);
                    }
                    return Some(batch);
                }
            }

            if tokio::time::Instant::now() >= deadline {
                let inner = self.inner.lock().expect("event broker lock");
                return inner.subscriptions.contains_key(sub_id).then(Vec::new);
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }

    /// Removes a PullPoint subscription.
    pub fn unsubscribe(&self, sub_id: &str) -> bool {
        let mut inner = self.inner.lock().expect("event broker lock");
        inner.subscriptions.remove(sub_id).is_some()
    }

    /// Returns up to `limit` most recent events (newest first) for Web UI telemetry.
    #[must_use]
    pub fn recent(&self, limit: usize) -> Vec<OnvifEventRecord> {
        let inner = self.inner.lock().expect("event broker lock");
        inner.events.iter().rev().take(limit).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pull_point_subscription_delivers_motion_and_pos_events() {
        let broker = EventBroker::new();
        let sub = broker.create_subscription();

        broker.publish_motion("VideoSource_1", true);
        broker.publish_pos_event("drawer", "Cash Drawer Opened", "warning");

        let events = broker
            .pull_messages(&sub, Duration::from_millis(50), 10)
            .await
            .expect("valid subscription");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].topic, "tns1:VideoSource/MotionAlarm");
        assert_eq!(events[0].data_value, "true");
        assert_eq!(events[1].topic, "tns1:RuleEngine/PosEvent/Alert");

        let empty = broker
            .pull_messages(&sub, Duration::from_millis(10), 10)
            .await
            .expect("still subscribed");
        assert!(empty.is_empty());

        assert!(broker.unsubscribe(&sub));
        assert!(
            broker
                .pull_messages(&sub, Duration::from_millis(10), 10)
                .await
                .is_none()
        );
    }
}
