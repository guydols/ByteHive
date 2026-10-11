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
    dropped: Arc<AtomicU64>,
    _token: Arc<()>,
}

impl BusReceiver {
    /// Number of messages dropped for this subscriber because its queue was
    /// full at publish time. Monotonically increasing.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
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
    dropped: Arc<AtomicU64>,
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
        let dropped = Arc::new(AtomicU64::new(0));
        self.subs
            .write()
            .entry(pattern.into())
            .or_default()
            .push(Sub {
                tx,
                alive,
                dropped: Arc::clone(&dropped),
            });
        BusReceiver {
            rx,
            dropped,
            _token: token,
        }
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
                            sub.dropped.fetch_add(1, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drop_counter_counts_queue_full_drops() {
        let bus = MessageBus::new();
        let rx = bus.subscribe("t", 4);
        for i in 0..10 {
            bus.publish("s", "t", json!(i));
        }
        let mut received = 0u64;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 4);
        assert_eq!(rx.dropped(), 6);
        assert_eq!(received + rx.dropped(), 10);
    }

    #[test]
    fn large_queue_absorbs_burst_without_drops() {
        let bus = MessageBus::new();
        let rx = bus.subscribe("*", 16_384);
        for i in 0..10_000u64 {
            bus.publish("s", "burst.topic", json!({ "i": i }));
        }
        assert_eq!(rx.dropped(), 0);
        let mut received = 0u64;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 10_000);
    }

    #[test]
    fn drop_counter_is_per_subscriber() {
        let bus = MessageBus::new();
        let small = bus.subscribe("*", 1);
        let big = bus.subscribe("*", 100);
        for i in 0..5 {
            bus.publish("s", "t", json!(i));
        }
        assert_eq!(small.dropped(), 4);
        assert_eq!(big.dropped(), 0);
    }
}
