use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServiceEvent {
    pub sequence: u64,
    pub event_type: String,
    pub occurred_at: DateTime<Utc>,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct EventHub {
    sequence: Arc<AtomicU64>,
    sender: broadcast::Sender<ServiceEvent>,
}

impl EventHub {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            sequence: Arc::new(AtomicU64::new(0)),
            sender,
        }
    }

    pub fn publish(&self, event_type: impl Into<String>, payload: serde_json::Value) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let event = ServiceEvent {
            sequence,
            event_type: event_type.into(),
            occurred_at: Utc::now(),
            payload,
        };
        // The event payload may contain book metadata or provider data. Diagnostics retain only
        // the internal event name and sequence so background workflows remain debuggable without
        // copying payload contents into logs.
        tracing::info!(
            diagnostic_code = "service.event.published",
            action = event.event_type.as_str(),
            count = sequence,
            "Service event published"
        );
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        self.sender.subscribe()
    }
}
