//! Persistent deletion ledger (tombstones) for filesync.
//!
//! Foundation for resurrection-safe deletes (TODO 1):
//! - Tombstones persist in `<root>/.bh_filesync/deletion-ledger.json`
//! - Last-writer-wins per path on `deleted_at_ms`
//! - 90-day TTL via [`DeletionLedger::prune`]
//! - Never panics on corrupt / missing files: corrupt JSON is backed up
//!   to `deletion-ledger.corrupt-<ts>.json` and an empty ledger is returned.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Directory (under the sync root) holding filesync state.
pub const LEDGER_DIR_NAME: &str = ".bh_filesync";
/// File name of the persistent deletion ledger.
pub const LEDGER_FILE_NAME: &str = "deletion-ledger.json";
/// Default time-to-live for tombstones: 90 days in milliseconds.
pub const DELETION_LEDGER_TTL_MS: u64 = 90 * 24 * 60 * 60 * 1000;

/// A single deletion tombstone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub path: PathBuf,
    pub deleted_at_ms: u64,
    pub deleter_node: String,
    pub prev_hash: Option<String>,
    pub prev_mtime_ms: Option<u64>,
}

impl Tombstone {
    pub fn new(
        path: PathBuf,
        deleted_at_ms: u64,
        deleter_node: String,
        prev_hash: Option<String>,
        prev_mtime_ms: Option<u64>,
    ) -> Self {
        Self {
            path,
            deleted_at_ms,
            deleter_node,
            prev_hash,
            prev_mtime_ms,
        }
    }
}

/// Persistent set of deletion tombstones, keyed by relative path.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeletionLedger {
    #[serde(default)]
    pub entries: HashMap<PathBuf, Tombstone>,
}

fn ledger_path(root: &Path) -> PathBuf {
    root.join(LEDGER_DIR_NAME).join(LEDGER_FILE_NAME)
}

/// Wall-clock milliseconds since the Unix epoch. Never panics: on clock
/// error (time before epoch) returns 0.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl DeletionLedger {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Load the ledger from `<root>/.bh_filesync/deletion-ledger.json`.
    /// Missing file => empty ledger. Corrupt JSON => back the file up to
    /// `deletion-ledger.corrupt-<ts>.json` (best effort) and return empty.
    /// Never panics.
    pub fn load(root: &Path) -> Self {
        let path = ledger_path(root);
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Self::new(),
            Err(_) => return Self::new(),
        };
        // Primary format: {"entries": {...}}. Fall back to a bare map for
        // forward-compat, then give up (backup + empty).
        if let Ok(ledger) = serde_json::from_str::<DeletionLedger>(&raw) {
            return ledger;
        }
        if let Ok(map) = serde_json::from_str::<HashMap<PathBuf, Tombstone>>(&raw) {
            return Self { entries: map };
        }
        // Corrupt: back up best-effort, then start empty.
        let backup = path.with_extension(format!("corrupt-{}.json", now_ms()));
        let _ = std::fs::copy(&path, &backup);
        Self::new()
    }

    /// Persist atomically: write `<file>.tmp` then rename. Creates
    /// `<root>/.bh_filesync` if needed.
    pub fn save_atomic(&self, root: &Path) -> io::Result<()> {
        let dir = root.join(LEDGER_DIR_NAME);
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(LEDGER_FILE_NAME);
        let tmp = dest.with_extension("json.tmp");
        let raw = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, &dest)?;
        Ok(())
    }

    /// Record a tombstone. Last-writer-wins: keeps whichever entry has the
    /// newest `deleted_at_ms`.
    pub fn record(
        &mut self,
        path: PathBuf,
        deleted_at_ms: u64,
        deleter_node: String,
        prev_hash: Option<String>,
        prev_mtime_ms: Option<u64>,
    ) {
        let entry = Tombstone::new(
            path.clone(),
            deleted_at_ms,
            deleter_node,
            prev_hash,
            prev_mtime_ms,
        );
        match self.entries.get(&path) {
            Some(existing) if existing.deleted_at_ms >= deleted_at_ms => {}
            _ => {
                self.entries.insert(path, entry);
            }
        }
    }

    /// Returns true if a tombstone for `path` exists and is strictly newer
    /// than `mtime_ms` (i.e. the delete wins over that file version).
    pub fn has_newer_than(&self, path: &Path, mtime_ms: u64) -> bool {
        match self.entries.get(path) {
            Some(t) => t.deleted_at_ms > mtime_ms,
            None => false,
        }
    }

    /// Merge remote tombstones, keeping the newest per path (LWW).
    pub fn merge_remote(&mut self, remote: HashMap<PathBuf, Tombstone>) {
        for (path, tomb) in remote {
            match self.entries.get(&path) {
                Some(existing) if existing.deleted_at_ms >= tomb.deleted_at_ms => {}
                _ => {
                    self.entries.insert(path, tomb);
                }
            }
        }
    }

    /// Drop tombstones older than `ttl_ms` relative to `now_ms`.
    /// Entries from the future (`deleted_at_ms > now_ms`) are kept.
    /// Returns the number of entries removed.
    pub fn prune(&mut self, now_ms: u64, ttl_ms: u64) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, t| {
            if t.deleted_at_ms > now_ms {
                return true;
            }
            now_ms.saturating_sub(t.deleted_at_ms) <= ttl_ms
        });
        before - self.entries.len()
    }

    /// Drop tombstones older than the default 90-day TTL. Returns the
    /// number of entries removed.
    pub fn prune_expired(&mut self, now_ms: u64) -> usize {
        self.prune(now_ms, DELETION_LEDGER_TTL_MS)
    }

    /// Remove the tombstone for `path` (e.g. on recreate / upload win).
    /// Returns the removed tombstone, if any.
    pub fn remove(&mut self, path: &Path) -> Option<Tombstone> {
        self.entries.remove(path)
    }

    /// Borrow the tombstone for `path`, if any.
    pub fn get(&self, path: &Path) -> Option<&Tombstone> {
        self.entries.get(path)
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&PathBuf, &Tombstone)> {
        self.entries.iter()
    }
}
