use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    InitialSync,
    Idle,
    Paused,
    Error(String),
}

impl ConnectionStatus {
    pub fn label(&self) -> &str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting…",
            Self::InitialSync => "Initial sync…",
            Self::Idle => "Connected",
            Self::Paused => "Paused",
            Self::Error(_) => "Error",
        }
    }

    pub fn colour(&self) -> [u8; 4] {
        match self {
            Self::Idle => [34, 197, 94, 255],
            Self::InitialSync => [59, 130, 246, 255],
            Self::Connecting => [245, 158, 11, 255],
            Self::Paused => [168, 85, 247, 255],
            Self::Error(_) => [239, 68, 68, 255],
            Self::Disconnected => [148, 163, 184, 255],
        }
    }
}

const LOG_CAPACITY: usize = 60;

#[derive(Debug, Clone)]
pub struct EventLog(Vec<String>);

impl EventLog {
    pub fn new() -> Self {
        Self(Vec::with_capacity(LOG_CAPACITY))
    }

    pub fn push(&mut self, msg: impl Into<String>) {
        if self.0.len() >= LOG_CAPACITY {
            self.0.remove(0);
        }
        self.0.push(msg.into());
    }

    pub fn entries(&self) -> &[String] {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct SyncSnapshot {
    pub status: ConnectionStatus,

    pub file_count: usize,
    pub dir_count: usize,
    pub total_bytes: u64,

    pub files_sent: u64,
    pub bytes_sent: u64,
    pub files_received: u64,
    pub bytes_received: u64,

    pub transfer_total: u64,

    pub last_connected: Option<Instant>,
    pub log: EventLog,
}

impl Default for SyncSnapshot {
    fn default() -> Self {
        Self {
            status: ConnectionStatus::Disconnected,
            file_count: 0,
            dir_count: 0,
            total_bytes: 0,
            files_sent: 0,
            bytes_sent: 0,
            files_received: 0,
            bytes_received: 0,
            transfer_total: 0,
            last_connected: None,
            log: EventLog::new(),
        }
    }
}

impl SyncSnapshot {
    pub fn log_event(&mut self, msg: impl Into<String>) {
        self.log.push(msg);
    }
}

pub type SharedState = Arc<RwLock<SyncSnapshot>>;

pub fn new_shared_state() -> SharedState {
    Arc::new(RwLock::new(SyncSnapshot::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_push_and_retrieve() {
        let mut log = EventLog::new();
        log.push("first");
        log.push("second");
        let entries = log.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], "first");
        assert_eq!(entries[1], "second");
    }

    #[test]
    fn event_log_accepts_owned_string() {
        let mut log = EventLog::new();
        log.push(String::from("owned string"));
        assert_eq!(log.entries()[0], "owned string");
    }

    #[test]
    fn event_log_evicts_oldest_when_full() {
        let mut log = EventLog::new();
        for i in 0..LOG_CAPACITY + 5 {
            log.push(format!("event-{i}"));
        }
        assert_eq!(
            log.entries().len(),
            LOG_CAPACITY,
            "log must not exceed capacity"
        );

        assert_eq!(log.entries()[0], "event-5");
        assert_eq!(
            log.entries()[LOG_CAPACITY - 1],
            format!("event-{}", LOG_CAPACITY + 4)
        );
    }

    #[test]
    fn event_log_exactly_at_capacity() {
        let mut log = EventLog::new();
        for i in 0..LOG_CAPACITY {
            log.push(format!("e{i}"));
        }
        assert_eq!(log.entries().len(), LOG_CAPACITY);

        log.push("overflow");
        assert_eq!(log.entries().len(), LOG_CAPACITY);
        assert_eq!(log.entries()[0], "e1");
        assert_eq!(log.entries()[LOG_CAPACITY - 1], "overflow");
    }

    #[test]
    fn connection_status_labels() {
        assert_eq!(ConnectionStatus::Disconnected.label(), "Disconnected");
        assert_eq!(ConnectionStatus::Connecting.label(), "Connecting…");
        assert_eq!(ConnectionStatus::InitialSync.label(), "Initial sync…");
        assert_eq!(ConnectionStatus::Idle.label(), "Connected");
        assert_eq!(ConnectionStatus::Paused.label(), "Paused");
        assert_eq!(
            ConnectionStatus::Error("some error".into()).label(),
            "Error"
        );
    }

    #[test]
    fn connection_status_colours_are_opaque() {
        let statuses = [
            ConnectionStatus::Idle,
            ConnectionStatus::InitialSync,
            ConnectionStatus::Connecting,
            ConnectionStatus::Paused,
            ConnectionStatus::Error("x".into()),
            ConnectionStatus::Disconnected,
        ];
        for s in &statuses {
            let c = s.colour();
            assert_eq!(
                c[3],
                255,
                "{:?} must have alpha=255 (fully opaque)",
                s.label()
            );
        }
    }

    #[test]
    fn connection_status_colours_are_all_distinct() {
        let statuses = [
            ConnectionStatus::Idle,
            ConnectionStatus::InitialSync,
            ConnectionStatus::Connecting,
            ConnectionStatus::Paused,
            ConnectionStatus::Error("x".into()),
            ConnectionStatus::Disconnected,
        ];
        let colours: Vec<_> = statuses.iter().map(|s| s.colour()).collect();
        for i in 0..colours.len() {
            for j in (i + 1)..colours.len() {
                assert_ne!(
                    colours[i],
                    colours[j],
                    "{} and {} must have distinct colours",
                    statuses[i].label(),
                    statuses[j].label()
                );
            }
        }
    }

    #[test]
    fn connection_status_equality() {
        assert_eq!(
            ConnectionStatus::Disconnected,
            ConnectionStatus::Disconnected
        );
        assert_eq!(
            ConnectionStatus::Error("a".into()),
            ConnectionStatus::Error("a".into())
        );
        assert_ne!(
            ConnectionStatus::Error("a".into()),
            ConnectionStatus::Error("b".into())
        );
        assert_ne!(ConnectionStatus::Idle, ConnectionStatus::Disconnected);
    }

    #[test]
    fn sync_snapshot_default_values() {
        let snap = SyncSnapshot::default();
        assert_eq!(snap.status, ConnectionStatus::Disconnected);
        assert_eq!(snap.file_count, 0);
        assert_eq!(snap.dir_count, 0);
        assert_eq!(snap.total_bytes, 0);
        assert_eq!(snap.files_sent, 0);
        assert_eq!(snap.bytes_sent, 0);
        assert_eq!(snap.files_received, 0);
        assert_eq!(snap.bytes_received, 0);
        assert_eq!(snap.transfer_total, 0);
        assert!(snap.last_connected.is_none());
        assert!(snap.log.entries().is_empty());
    }

    #[test]
    fn sync_snapshot_log_event_appends() {
        let mut snap = SyncSnapshot::default();
        snap.log_event("connected");
        snap.log_event("sync complete");
        assert_eq!(snap.log.entries().len(), 2);
        assert_eq!(snap.log.entries()[0], "connected");
    }

    #[test]
    fn new_shared_state_starts_disconnected() {
        let state = new_shared_state();
        assert_eq!(state.read().status, ConnectionStatus::Disconnected);
    }

    #[test]
    fn shared_state_write_then_read() {
        let state = new_shared_state();
        {
            let mut s = state.write();
            s.status = ConnectionStatus::Idle;
            s.file_count = 42;
            s.total_bytes = 1024;
        }
        let s = state.read();
        assert_eq!(s.status, ConnectionStatus::Idle);
        assert_eq!(s.file_count, 42);
        assert_eq!(s.total_bytes, 1024);
    }

    #[test]
    fn shared_state_is_cheaply_cloneable() {
        let state = new_shared_state();
        let clone = state.clone();
        clone.write().status = ConnectionStatus::Connecting;

        assert_eq!(state.read().status, ConnectionStatus::Connecting);
    }
}
