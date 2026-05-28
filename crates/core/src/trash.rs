use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

pub const TRASH_DIR: &str = ".bh_filesync/trash";
pub const TRASH_INDEX_FILE: &str = "index.json";

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
        let _guard = self.lock.lock();
        let mut index = self.load_index();

        let pos = index
            .iter()
            .position(|e| e.id == id)
            .ok_or_else(|| format!("no trash entry with id {id:?}"))?;

        let entry = index[pos].clone();

        // Reject unsafe paths
        for component in entry.original_path.components() {
            match component {
                Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                    return Err("unsafe original_path in trash entry".into());
                }
                _ => {}
            }
        }

        let trash_dir = self.trash_dir();
        let trash_full = trash_dir.join(&entry.trash_rel_path);
        let original_full = self.root.join(&entry.original_path);

        if original_full.exists() {
            return Err(format!(
                "destination already exists: {:?}",
                entry.original_path
            ));
        }

        if let Some(parent) = original_full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create parent dirs: {e}"))?;
        }

        std::fs::rename(&trash_full, &original_full).map_err(|e| format!("restore failed: {e}"))?;

        let entry_dir = trash_dir.join(&entry.id);
        let _ = std::fs::remove_dir_all(&entry_dir);

        index.remove(pos);
        self.save_index(&index);
        Ok(())
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
