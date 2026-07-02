use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    InitialSync,
    /// Actively exchanging an incremental (post-initial-sync) batch of
    /// changes with the peer — files being sent/received in the background
    /// while the connection is otherwise idle.
    Syncing,
    Idle,
    Paused,
    /// The server has received the connection but an administrator has not yet
    /// approved this client.  The client retries with a longer back-off while
    /// in this state.
    AwaitingApproval,
    Error(String),
}

impl ConnectionStatus {
    pub fn label(&self) -> &str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting…",
            Self::InitialSync => "Initial sync…",
            Self::Syncing => "Syncing…",
            Self::Idle => "Connected",
            Self::Paused => "Paused",
            Self::AwaitingApproval => "Awaiting approval…",
            Self::Error(_) => "Error",
        }
    }

    pub fn colour(&self) -> [u8; 4] {
        match self {
            Self::Idle => [34, 197, 94, 255],
            Self::InitialSync => [59, 130, 246, 255],
            Self::Syncing => [14, 165, 233, 255],
            Self::Connecting => [245, 158, 11, 255],
            Self::Paused => [168, 85, 247, 255],
            Self::AwaitingApproval => [251, 191, 36, 255],
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

#[derive(Debug, Clone, PartialEq)]
pub enum ConflictKind {
    BothModified,
    LocalOnly,
    RemoteOnly,
    BothCreated,
}

impl ConflictKind {
    pub fn label(&self) -> &str {
        match self {
            Self::BothModified => "Both modified",
            Self::LocalOnly => "Deleted remotely",
            Self::RemoteOnly => "Deleted locally",
            Self::BothCreated => "Both created",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Conflict {
    pub id: usize,
    pub filename: String,
    /// Absolute path to the containing folder.
    pub folder_path: String,
    pub local_modified: String,
    pub remote_modified: String,
    pub kind: ConflictKind,
}

#[derive(Debug, Clone)]
pub struct FileNode {
    pub id: usize,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    /// Whether this node is included in the sync scope.
    pub included: bool,
    /// Only relevant for directories — whether children are shown.
    pub expanded: bool,
    pub children: Vec<FileNode>,
}

impl FileNode {
    pub fn dir(id: usize, name: &str, path: &str, children: Vec<FileNode>) -> Self {
        Self {
            id,
            name: name.to_string(),
            path: path.to_string(),
            is_dir: true,
            included: true,
            expanded: true,
            children,
        }
    }

    pub fn file(id: usize, name: &str, path: &str) -> Self {
        Self {
            id,
            name: name.to_string(),
            path: path.to_string(),
            is_dir: false,
            included: true,
            expanded: false,
            children: vec![],
        }
    }
}

#[derive(Debug, Clone)]
pub struct FlatNode {
    pub id: usize,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub included: bool,
    pub expanded: bool,
    pub depth: usize,
    pub has_children: bool,
}

/// Recursively flattens the tree into render rows, respecting expansion state.
pub fn flatten_tree(nodes: &[FileNode]) -> Vec<FlatNode> {
    let mut out = Vec::new();
    flatten_recursive(nodes, 0, &mut out);
    out
}

fn flatten_recursive(nodes: &[FileNode], depth: usize, out: &mut Vec<FlatNode>) {
    for node in nodes {
        out.push(FlatNode {
            id: node.id,
            name: node.name.clone(),
            path: node.path.clone(),
            is_dir: node.is_dir,
            included: node.included,
            expanded: node.expanded,
            depth,
            has_children: !node.children.is_empty(),
        });
        if node.is_dir && node.expanded {
            flatten_recursive(&node.children, depth + 1, out);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SideTab {
    Stats,
    Conflicts,
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

    /// Timestamp of the most recent incremental (live) sync activity —
    /// used to detect when a live-sync batch has gone quiet so the UI can
    /// drop back from `Syncing` to `Idle`.
    pub last_activity: Option<Instant>,

    pub conflicts: Vec<Conflict>,

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
            last_activity: None,
            conflicts: Vec::new(),
            last_connected: None,
            log: EventLog::new(),
        }
    }
}

impl SyncSnapshot {
    pub fn log_event(&mut self, msg: impl Into<String>) {
        self.log.push(msg);
    }

    /// Marks the start (or continuation) of a live incremental-sync batch.
    /// Transitions the status to `Syncing` and, if this is the start of a
    /// new batch (status was previously `Idle`), resets the transfer
    /// counters so the UI reflects only this batch's progress.
    pub fn begin_sync_activity(&mut self) {
        if self.status == ConnectionStatus::Idle {
            self.files_sent = 0;
            self.bytes_sent = 0;
            self.files_received = 0;
            self.bytes_received = 0;
            self.transfer_total = 0;
            self.status = ConnectionStatus::Syncing;
        }
        self.last_activity = Some(Instant::now());
    }

    /// Called periodically (e.g. on a UI tick) to drop back from `Syncing`
    /// to `Idle` once no live-sync activity has been observed for
    /// `idle_after`.
    pub fn end_sync_activity_if_quiet(&mut self, idle_after: std::time::Duration) {
        if self.status == ConnectionStatus::Syncing {
            let quiet = self
                .last_activity
                .map(|t| t.elapsed() >= idle_after)
                .unwrap_or(true);
            if quiet {
                self.status = ConnectionStatus::Idle;
                self.last_connected = Some(Instant::now());
            }
        }
    }

    /// Adds a new conflict entry with a freshly-allocated, unique id.
    pub fn push_conflict(
        &mut self,
        filename: String,
        folder_path: String,
        local_modified: String,
        remote_modified: String,
        kind: ConflictKind,
    ) {
        self.conflicts.push(Conflict {
            id: next_conflict_id(),
            filename,
            folder_path,
            local_modified,
            remote_modified,
            kind,
        });
    }
}

static NEXT_CONFLICT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// Allocates a fresh, process-unique conflict id (used so conflicts raised
/// from different code paths never collide).
pub fn next_conflict_id() -> usize {
    NEXT_CONFLICT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
        assert_eq!(ConnectionStatus::Syncing.label(), "Syncing…");
        assert_eq!(ConnectionStatus::Idle.label(), "Connected");
        assert_eq!(ConnectionStatus::Paused.label(), "Paused");
        assert_eq!(
            ConnectionStatus::AwaitingApproval.label(),
            "Awaiting approval…"
        );
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
            ConnectionStatus::Syncing,
            ConnectionStatus::Connecting,
            ConnectionStatus::Paused,
            ConnectionStatus::AwaitingApproval,
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
            ConnectionStatus::Syncing,
            ConnectionStatus::Connecting,
            ConnectionStatus::Paused,
            ConnectionStatus::AwaitingApproval,
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
            ConnectionStatus::AwaitingApproval,
            ConnectionStatus::AwaitingApproval
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
        assert_ne!(
            ConnectionStatus::AwaitingApproval,
            ConnectionStatus::Connecting
        );
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
        assert!(snap.last_activity.is_none());
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
    fn begin_sync_activity_transitions_from_idle_and_resets_counters() {
        let mut snap = SyncSnapshot::default();
        snap.status = ConnectionStatus::Idle;
        snap.bytes_sent = 500;
        snap.files_received = 3;
        snap.begin_sync_activity();
        assert_eq!(snap.status, ConnectionStatus::Syncing);
        assert_eq!(snap.bytes_sent, 0);
        assert_eq!(snap.files_received, 0);
        assert!(snap.last_activity.is_some());
    }

    #[test]
    fn begin_sync_activity_does_not_reset_counters_mid_batch() {
        let mut snap = SyncSnapshot::default();
        snap.status = ConnectionStatus::Syncing;
        snap.bytes_received = 42;
        snap.begin_sync_activity();
        assert_eq!(snap.status, ConnectionStatus::Syncing);
        assert_eq!(snap.bytes_received, 42);
    }

    #[test]
    fn end_sync_activity_if_quiet_returns_to_idle_after_timeout() {
        let mut snap = SyncSnapshot::default();
        snap.status = ConnectionStatus::Syncing;
        snap.last_activity = Some(Instant::now() - std::time::Duration::from_secs(10));
        snap.end_sync_activity_if_quiet(std::time::Duration::from_millis(100));
        assert_eq!(snap.status, ConnectionStatus::Idle);
    }

    #[test]
    fn end_sync_activity_if_quiet_stays_syncing_when_recent() {
        let mut snap = SyncSnapshot::default();
        snap.status = ConnectionStatus::Syncing;
        snap.last_activity = Some(Instant::now());
        snap.end_sync_activity_if_quiet(std::time::Duration::from_secs(5));
        assert_eq!(snap.status, ConnectionStatus::Syncing);
    }

    #[test]
    fn push_conflict_assigns_unique_ids() {
        let mut snap = SyncSnapshot::default();
        snap.push_conflict(
            "a.txt".into(),
            "/sync".into(),
            "t1".into(),
            "t2".into(),
            ConflictKind::BothModified,
        );
        snap.push_conflict(
            "b.txt".into(),
            "/sync".into(),
            "t1".into(),
            "t2".into(),
            ConflictKind::BothModified,
        );
        assert_eq!(snap.conflicts.len(), 2);
        assert_ne!(snap.conflicts[0].id, snap.conflicts[1].id);
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
