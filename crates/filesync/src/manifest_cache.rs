//! Persistent manifest hash cache (initial-scan speedup).
//!
//! Template: [`crate::ledger`] — state lives in
//! `<root>/.bh_filesync/manifest-cache.json`, loads tolerate missing/corrupt
//! files (corrupt JSON is backed up, never panics), saves are atomic
//! (`.tmp` + rename) and skipped when nothing changed.
//!
//! Trust rule: a cached hash is reused only when size, mtime (secs + nanos)
//! and inode ALL match the current stat AND the file is older than
//! [`CACHE_MIN_AGE_MS`] (stability gate — a recently modified file may still
//! be mid-write, so it is always re-hashed).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Directory (under the sync root) holding filesync state.
pub const CACHE_DIR_NAME: &str = ".bh_filesync";
/// File name of the persistent manifest hash cache.
pub const CACHE_FILE_NAME: &str = "manifest-cache.json";
/// Minimum file age before a cache hit is trusted (stability gate).
pub const CACHE_MIN_AGE_MS: u64 = 5_000;

/// Hash + identity snapshot for one relative path. `hash` is lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedEntry {
    pub size: u64,
    pub mtime_s: i64,
    pub mtime_ns: u32,
    pub inode: u64,
    pub hash: String,
}

/// Persistent path → [`CachedEntry`] map with dirty tracking so warm scans
/// skip disk writes entirely.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestCache {
    #[serde(default)]
    pub entries: HashMap<PathBuf, CachedEntry>,
    #[serde(skip)]
    dirty: bool,
}

fn cache_path(root: &Path) -> PathBuf {
    root.join(CACHE_DIR_NAME).join(CACHE_FILE_NAME)
}

/// Wall-clock milliseconds since the Unix epoch (re-export of the ledger
/// clock so all timestamps share one definition).
pub fn now_ms() -> u64 {
    crate::ledger::now_ms()
}

/// Split `Metadata::modified()` into `(unix secs, subsec nanos)`.
/// Returns `None` for unavailable/pre-epoch mtimes (caller re-hashes).
pub fn mtime_parts(meta: &std::fs::Metadata) -> Option<(i64, u32)> {
    let d = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    let secs: i64 = d.as_secs().try_into().ok()?;
    Some((secs, d.subsec_nanos()))
}

/// Inode for cache identity (unix; the crate already depends on inotify).
pub fn inode_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

impl ManifestCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            dirty: false,
        }
    }

    /// Load the cache from `<root>/.bh_filesync/manifest-cache.json`.
    /// Missing file => empty cache. Corrupt JSON => back the file up to
    /// `manifest-cache.corrupt-<ts>.json` (best effort) and return empty.
    /// Never panics.
    pub fn load(root: &Path) -> Self {
        let path = cache_path(root);
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Self::new(),
            Err(_) => return Self::new(),
        };
        if let Ok(cache) = serde_json::from_str::<ManifestCache>(&raw) {
            return cache;
        }
        let backup = path.with_extension(format!("corrupt-{}.json", now_ms()));
        let _ = std::fs::copy(&path, &backup);
        Self::new()
    }

    /// Persist atomically (write `<file>.tmp` then rename), creating
    /// `<root>/.bh_filesync` if needed. Skips the write entirely when
    /// nothing changed since load/last save.
    pub fn save_atomic(&mut self, root: &Path) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let dir = root.join(CACHE_DIR_NAME);
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(CACHE_FILE_NAME);
        let tmp = dest.with_extension("json.tmp");
        let raw = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, &dest)?;
        self.dirty = false;
        Ok(())
    }

    /// Whether the cache changed since load/last save.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Look up a cached hash. Returns `Some(hex)` only when size, mtime
    /// (secs + nanos) and inode all match AND the file is older than
    /// [`CACHE_MIN_AGE_MS`] as of `now_ms`.
    pub fn lookup(
        &self,
        path: &Path,
        size: u64,
        mtime_s: i64,
        mtime_ns: u32,
        inode: u64,
        now_ms: u64,
    ) -> Option<String> {
        let e = self.entries.get(path)?;
        if e.size != size || e.mtime_s != mtime_s || e.mtime_ns != mtime_ns || e.inode != inode {
            return None;
        }
        let mtime_ms = (mtime_s.max(0) as u64)
            .saturating_mul(1_000)
            .saturating_add(mtime_ns as u64 / 1_000_000);
        if now_ms.saturating_sub(mtime_ms) <= CACHE_MIN_AGE_MS {
            return None; // too fresh — stability gate forces a re-hash
        }
        Some(e.hash.clone())
    }

    /// Insert/replace an entry. Marks dirty only on actual change so
    /// fully-warm scans skip the save.
    pub fn upsert(&mut self, path: PathBuf, entry: CachedEntry) {
        if self.entries.get(&path) != Some(&entry) {
            self.entries.insert(path, entry);
            self.dirty = true;
        }
    }

    /// Drop entries for paths no longer present. Returns the number removed
    /// (marks dirty only when something was removed).
    pub fn prune_missing(&mut self, existing: &HashSet<PathBuf>) -> usize {
        let before = self.entries.len();
        self.entries.retain(|p, _| existing.contains(p));
        let removed = before - self.entries.len();
        if removed > 0 {
            self.dirty = true;
        }
        removed
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
