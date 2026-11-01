use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

pub const TRASH_DIR: &str = ".bh_filesync/trash";
pub const TRASH_INDEX_FILE: &str = "index.json";

#[derive(Debug, Clone)]
pub enum TrashRestoreError {
    NotFound(String),
    UnsafePath(String),
    DestinationExists(PathBuf),
    Io(String),
}

impl std::fmt::Display for TrashRestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) => write!(f, "{m}"),
            Self::UnsafePath(m) => write!(f, "{m}"),
            Self::DestinationExists(p) => {
                write!(f, "destination already exists: {:?}", p)
            }
            Self::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for TrashRestoreError {}

fn validate_rel(rel: &Path) -> Result<(), &'static str> {
    if rel.as_os_str().is_empty() {
        return Err("empty path");
    }
    for component in rel.components() {
        match component {
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err("unsafe path in trash entry");
            }
            Component::Normal(_) | Component::CurDir => {}
        }
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashEntry {
    pub id: String,
    pub original_path: PathBuf,
    pub trash_rel_path: PathBuf,
    pub deleted_at_ms: u64,
    pub size: u64,
    pub is_dir: bool,
    pub deleted_by: String,
}

pub struct TrashManager {
    root: PathBuf,
    lock: Mutex<()>,
    pub expiry_days: Option<u64>,
}

impl TrashManager {
    pub fn new(root: PathBuf, expiry_days: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            root,
            lock: Mutex::new(()),
            expiry_days,
        })
    }

    fn trash_dir(&self) -> PathBuf {
        self.root.join(TRASH_DIR)
    }

    fn load_index(&self) -> Vec<TrashEntry> {
        let path = self.trash_dir().join(TRASH_INDEX_FILE);
        let Ok(data) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<TrashEntry>>(&data).unwrap_or_else(|e| {
            log::warn!("trash: failed to parse index: {e}; treating as empty");
            Vec::new()
        })
    }

    fn save_index(&self, index: &[TrashEntry]) {
        let trash_dir = self.trash_dir();
        if let Err(e) = std::fs::create_dir_all(&trash_dir) {
            log::warn!("trash: cannot create trash dir: {e}");
            return;
        }
        let path = trash_dir.join(TRASH_INDEX_FILE);
        let tmp = trash_dir.join("index.json.tmp");
        match serde_json::to_string_pretty(index) {
            Ok(json) => {
                if std::fs::write(&tmp, json.as_bytes()).is_ok() {
                    if let Err(e) = std::fs::rename(&tmp, &path) {
                        log::warn!("trash: atomic index write failed: {e}");
                        let _ = std::fs::remove_file(&tmp);
                    }
                }
            }
            Err(e) => log::warn!("trash: failed to serialize index: {e}"),
        }
    }

    pub fn move_to_trash(&self, full_path: &Path, rel: &Path, deleted_by: &str) {
        if !full_path.exists() {
            return;
        }
        let id = trash_id();
        let trash_dir = self.trash_dir();
        let entry_dir = trash_dir.join(&id);

        // Mirror folder structure: trash/<id>/<original/relative/path>
        let dest = entry_dir.join(rel);
        let dest_parent = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(&entry_dir);

        if let Err(e) = std::fs::create_dir_all(dest_parent) {
            log::warn!("trash: cannot create entry dir for {rel:?}: {e}; hard-deleting");
            hard_delete(full_path);
            return;
        }

        let size = if full_path.is_dir() {
            0u64
        } else {
            std::fs::metadata(full_path).map(|m| m.len()).unwrap_or(0)
        };
        let is_dir = full_path.is_dir();

        let moved = if std::fs::rename(full_path, &dest).is_ok() {
            true
        } else if is_dir {
            copy_dir_all(full_path, &dest).is_ok() && {
                let _ = std::fs::remove_dir_all(full_path);
                true
            }
        } else {
            std::fs::copy(full_path, &dest).is_ok() && {
                let _ = std::fs::remove_file(full_path);
                true
            }
        };

        if !moved {
            log::warn!("trash: move failed for {rel:?}; hard-deleting");
            hard_delete(full_path);
            let _ = std::fs::remove_dir_all(&entry_dir);
            return;
        }

        let unix_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let entry = TrashEntry {
            id: id.clone(),
            original_path: rel.to_path_buf(),
            trash_rel_path: PathBuf::from(&id).join(rel),
            deleted_at_ms: unix_ms,
            size,
            is_dir,
            deleted_by: deleted_by.to_string(),
        };

        let _guard = self.lock.lock();
        let mut index = self.load_index();
        index.push(entry);
        self.save_index(&index);
    }

    pub fn list_trash(&self) -> Vec<TrashEntry> {
        let _guard = self.lock.lock();
        self.load_index()
    }

    pub fn restore_entry(&self, id: &str) -> Result<(), String> {
        self.restore_entry_with_dest(id, None)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Restore a trash entry, optionally to a different destination.
    ///
    /// - `dest_override`: when `Some`, restore to this root-relative path
    ///   instead of the original location. Must be a safe relative path
    ///   (no `..`, no absolute, no prefix); parent dirs are created.
    /// - When the destination already exists the restore is refused with
    ///   [`TrashRestoreError::DestinationExists`] (callers map to 409) and
    ///   nothing is overwritten.
    /// - Returns the final root-relative destination path on success.
    pub fn restore_entry_with_dest(
        &self,
        id: &str,
        dest_override: Option<&Path>,
    ) -> Result<PathBuf, TrashRestoreError> {
        let _guard = self.lock.lock();
        let mut index = self.load_index();

        let pos = index
            .iter()
            .position(|e| e.id == id)
            .ok_or_else(|| TrashRestoreError::NotFound(format!("no trash entry with id {id:?}")))?;

        let entry = index[pos].clone();

        let dest_rel: PathBuf = match dest_override {
            Some(o) => {
                if o.as_os_str().is_empty() {
                    return Err(TrashRestoreError::UnsafePath(
                        "empty dest override".into(),
                    ));
                }
                validate_rel(o).map_err(|m| TrashRestoreError::UnsafePath(m.to_string()))?;
                o.to_path_buf()
            }
            None => {
                validate_rel(&entry.original_path)
                    .map_err(|m| TrashRestoreError::UnsafePath(m.to_string()))?;
                entry.original_path.clone()
            }
        };

        let trash_dir = self.trash_dir();
        let trash_full = trash_dir.join(&entry.trash_rel_path);
        if !trash_full.exists() {
            return Err(TrashRestoreError::NotFound(format!(
                "trash data missing for id {id:?}"
            )));
        }
        let dest_full = self.root.join(&dest_rel);

        if dest_full.exists() {
            return Err(TrashRestoreError::DestinationExists(dest_rel));
        }

        if let Some(parent) = dest_full.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| TrashRestoreError::Io(format!("create parent dirs: {e}")))?;
        }

        if std::fs::rename(&trash_full, &dest_full).is_err() {
            // Cross-device fallback: copy then remove source.
            let copied = if trash_full.is_dir() {
                copy_dir_all(&trash_full, &dest_full).is_ok()
            } else {
                std::fs::copy(&trash_full, &dest_full).is_ok()
            };
            if !copied {
                return Err(TrashRestoreError::Io("restore failed".into()));
            }
            if trash_full.is_dir() {
                let _ = std::fs::remove_dir_all(&trash_full);
            } else {
                let _ = std::fs::remove_file(&trash_full);
            }
        }

        let entry_dir = trash_dir.join(&entry.id);
        let _ = std::fs::remove_dir_all(&entry_dir);

        index.remove(pos);
        self.save_index(&index);
        Ok(dest_rel)
    }

    pub fn purge_entry(&self, id: &str) -> Result<(), String> {
        let _guard = self.lock.lock();
        let mut index = self.load_index();

        let pos = index
            .iter()
            .position(|e| e.id == id)
            .ok_or_else(|| format!("no trash entry with id {id:?}"))?;

        let entry_dir = self.trash_dir().join(&index[pos].id);
        if let Err(e) = std::fs::remove_dir_all(&entry_dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(format!("delete failed: {e}"));
            }
        }

        index.remove(pos);
        self.save_index(&index);
        Ok(())
    }

    pub fn purge_expired(&self) -> usize {
        let expiry_days = match self.expiry_days {
            Some(d) if d > 0 => d,
            _ => return 0,
        };

        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let cutoff_ms = now_ms.saturating_sub(expiry_days * 24 * 3600 * 1000);

        let _guard = self.lock.lock();
        let mut index = self.load_index();
        let trash_dir = self.trash_dir();
        let mut count = 0usize;

        index.retain(|e| {
            if e.deleted_at_ms < cutoff_ms {
                let entry_dir = trash_dir.join(&e.id);
                if let Err(err) = std::fs::remove_dir_all(&entry_dir) {
                    if err.kind() != std::io::ErrorKind::NotFound {
                        log::warn!("trash: failed to purge {entry_dir:?}: {err}");
                    }
                }
                count += 1;
                false
            } else {
                true
            }
        });

        if count > 0 {
            self.save_index(&index);
            log::info!("trash: purged {count} expired entry/entries");
        }
        count
    }

    pub fn empty(&self) -> usize {
        let _guard = self.lock.lock();
        let mut index = self.load_index();
        let trash_dir = self.trash_dir();
        let count = index.len();
        for e in &index {
            let entry_dir = trash_dir.join(&e.id);
            if let Err(err) = std::fs::remove_dir_all(&entry_dir) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("trash: failed to remove {entry_dir:?}: {err}");
                }
            }
        }
        index.clear();
        self.save_index(&index);
        count
    }
}

fn trash_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ns = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{ns:020}_{seq:06}")
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

fn hard_delete(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Path of the filesync deletion ledger for a given sync root.
pub fn ledger_file_path(root: &Path) -> PathBuf {
    root.join(".bh_filesync").join("deletion-ledger.json")
}

/// Remove tombstones covering any of `prefixes` (exact match or
/// component-wise child via `strip_prefix`) from
/// `<root>/.bh_filesync/deletion-ledger.json`.
///
/// Generic JSON manipulation so crates without a dependency on
/// `bytehive-filesync` (e.g. filebrowser) can keep the ledger consistent
/// on trash restore/purge. Handles both the canonical
/// `{"entries": {path: tombstone}}` shape and a bare `{path: tombstone}`
/// map. Missing file => 0. Corrupt JSON => 0 (no panic, no backup —
/// `DeletionLedger::load` owns the backup path). Persists atomically only
/// when at least one entry was removed. Returns the number removed.
pub fn clear_ledger_covering(root: &Path, prefixes: &[PathBuf]) -> usize {
    if prefixes.is_empty() {
        return 0;
    }
    let path = ledger_file_path(root);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(_) => return 0,
    };
    let mut value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return 0,
    };

    // Resolve the mutable entries map for either shape.
    let entries_obj: Option<&mut serde_json::Map<String, serde_json::Value>> =
        if value
            .get("entries")
            .is_some_and(|v| v.is_object())
        {
            value
                .get_mut("entries")
                .and_then(|v| v.as_object_mut())
        } else if value.is_object() {
            value.as_object_mut()
        } else {
            None
        };
    let Some(entries) = entries_obj else {
        return 0;
    };

    let doomed: Vec<String> = entries
        .keys()
        .filter(|k| {
            let kp = PathBuf::from(*k);
            prefixes
                .iter()
                .any(|p| kp == *p || kp.strip_prefix(p).is_ok())
        })
        .cloned()
        .collect();
    if doomed.is_empty() {
        return 0;
    }
    for k in &doomed {
        entries.remove(k);
    }
    let count = doomed.len();

    let tmp = path.with_extension("json.tmp");
    match serde_json::to_string_pretty(&value) {
        Ok(json) => {
            if std::fs::write(&tmp, json.as_bytes()).is_ok() {
                if std::fs::rename(&tmp, &path).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                    return 0;
                }
            } else {
                return 0;
            }
        }
        Err(_) => return 0,
    }
    log::info!("trash: cleared {count} covering tombstone(s)");
    count
}
