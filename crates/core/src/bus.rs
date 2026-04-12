use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use parking_lot::RwLock;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

pub const BUS_QUEUE_DEPTH: usize = 256;

#[derive(Debug, Clone)]
pub struct BusMessage {
    pub id: u64,
    pub source: String,
    pub topic: String,
    pub payload: Value,
    pub timestamp_ms: u64,
}

pub struct BusReceiver {
    pub rx: Receiver<Arc<BusMessage>>,
    _token: Arc<()>,
}

impl std::ops::Deref for BusReceiver {
    type Target = Receiver<Arc<BusMessage>>;

    fn deref(&self) -> &Self::Target {
        &self.rx
    }
}

impl IntoIterator for BusReceiver {
    type Item = Arc<BusMessage>;
    type IntoIter = crossbeam_channel::IntoIter<Arc<BusMessage>>;

    fn into_iter(self) -> Self::IntoIter {
        self.rx.into_iter()
    }
}

struct Sub {
    tx: Sender<Arc<BusMessage>>,
    alive: Weak<()>,
}

pub struct MessageBus {
    subs: RwLock<HashMap<String, Vec<Sub>>>,
    next_id: AtomicU64,
}

impl MessageBus {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            subs: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn subscribe(&self, pattern: impl Into<String>, queue: usize) -> BusReceiver {
        let (tx, rx) = bounded(queue);
        let token = Arc::new(());
        let alive = Arc::downgrade(&token);
        self.subs
            .write()
            .entry(pattern.into())
            .or_default()
            .push(Sub { tx, alive });
        BusReceiver { rx, _token: token }
    }

    pub fn sub(&self, pattern: impl Into<String>) -> BusReceiver {
        self.subscribe(pattern, BUS_QUEUE_DEPTH)
    }

    pub fn publish(&self, source: impl Into<String>, topic: impl Into<String>, payload: Value) {
        let topic = topic.into();
        let msg = Arc::new(BusMessage {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            source: source.into(),
            topic: topic.clone(),
            payload,
            timestamp_ms: crate::timestamp_now(),
        });

        let subs = self.subs.read();
        for (pattern, txs) in subs.iter() {
            if topic_matches(pattern, &topic) {
                for sub in txs {
                    match sub.tx.try_send(Arc::clone(&msg)) {
                        Ok(_) => {}
                        Err(TrySendError::Full(_)) => {
                            log::warn!(
                                "bus: subscriber queue full for topic '{}' — message dropped",
                                topic
                            );
                        }
                        Err(TrySendError::Disconnected(_)) => {}
                    }
                }
            }
        }
    }

    pub fn patterns(&self) -> Vec<String> {
        self.subs.read().keys().cloned().collect()
    }

    pub fn gc(&self) {
        self.subs.write().retain(|_, txs| {
            txs.retain(|sub| sub.alive.upgrade().is_some());
            !txs.is_empty()
        });
    }
}

fn topic_matches(pattern: &str, topic: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern == topic {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        let expected = format!("{prefix}.");
        return topic.starts_with(&expected) || topic == prefix;
    }
    false
}
