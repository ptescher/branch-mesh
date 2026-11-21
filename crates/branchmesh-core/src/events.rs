use serde::Serialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// A runtime event, broadcast to observers (the web UI's SSE stream, tests, ...).
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub seq: u64,
    pub ts: i64,
    pub kind: String,
    pub data: Value,
}

/// Fan-out bus with a small replay buffer of recent events.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
    seq: Arc<AtomicU64>,
    recent: Arc<Mutex<VecDeque<Event>>>,
    capacity: usize,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(2048)
    }
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        EventBus {
            tx,
            seq: Arc::new(AtomicU64::new(0)),
            recent: Arc::new(Mutex::new(VecDeque::with_capacity(512))),
            capacity: 512,
        }
    }

    pub fn emit(&self, kind: &str, data: Value) {
        let event = Event {
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            ts: crate::now_ms(),
            kind: kind.to_string(),
            data,
        };
        // Token deltas are high-volume and only interesting live.
        if kind != "llm_delta" {
            let mut recent = self.recent.lock().unwrap();
            if recent.len() >= self.capacity {
                recent.pop_front();
            }
            recent.push_back(event.clone());
        }
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn recent(&self, limit: usize) -> Vec<Event> {
        let recent = self.recent.lock().unwrap();
        let skip = recent.len().saturating_sub(limit);
        recent.iter().skip(skip).cloned().collect()
    }
}
