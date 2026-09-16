use crossbeam_channel::Sender;
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub enum FsEvent {
    Changed(PathBuf),
    WriteComplete(PathBuf),
    Deleted(PathBuf),
    Renamed(PathBuf, PathBuf),
}

/// Add inotify watches for `path` and every directory below it.
///
/// A freshly created / moved-in directory may already contain a populated
/// subtree (e.g. `mv outside/ new/`), so watching only the top level would
/// leave the new children unwatched.
fn add_watch_recursive(
    inotify: &mut Inotify,
    wd_map: &mut HashMap<WatchDescriptor, PathBuf>,
    path: &PathBuf,
    mask: WatchMask,
) {
    for entry in WalkDir::new(path).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_dir() {
            match inotify.watches().add(entry.path(), mask) {
                Ok(new_wd) => {
                    wd_map.insert(new_wd, entry.path().to_path_buf());
                }
                Err(e) => warn!("failed to watch {:?}: {e}", entry.path()),
            }
        }
    }
}

/// Pure prefix-remap used by [`rewrite_wd_map_for_rename`]: returns the new
/// absolute path if `path` is `from_abs` itself or lives below it.
fn remap_path_for_rename(
    path: &std::path::Path,
    from_abs: &std::path::Path,
    to_abs: &std::path::Path,
) -> Option<PathBuf> {
    if path == from_abs {
        Some(to_abs.to_path_buf())
    } else if let Ok(stripped) = path.strip_prefix(from_abs) {
        Some(to_abs.join(stripped))
    } else {
        None
    }
}

/// Rewrite `wd_map` entries whose path is `from_rel` (or a descendant of it)
/// to point at `to_rel`.
///
/// Inotify watches follow the renamed inode, so the kernel watch descriptors
/// stay valid across a rename — only our `wd -> path` bookkeeping goes stale.
/// Without this rewrite, later events inside the moved tree would be reported
/// under the old (phantom) path.
fn rewrite_wd_map_for_rename(
    wd_map: &mut HashMap<WatchDescriptor, PathBuf>,
    root: &std::path::Path,
    from_rel: &std::path::Path,
    to_rel: &std::path::Path,
) {
    let from_abs = root.join(from_rel);
    let to_abs = root.join(to_rel);
    let updates: Vec<(WatchDescriptor, PathBuf)> = wd_map
        .iter()
        .filter_map(|(wd, path)| {
            remap_path_for_rename(path, &from_abs, &to_abs).map(|p| (wd.clone(), p))
        })
        .collect();
    for (wd, new_path) in updates {
        wd_map.insert(wd, new_path);
    }
}

pub fn start_watcher(
    root: PathBuf,
    tx: Sender<FsEvent>,
) -> std::io::Result<thread::JoinHandle<()>> {
    let root = root.canonicalize().unwrap_or(root);

    let watch_mask = WatchMask::CREATE
        | WatchMask::CLOSE_WRITE
        | WatchMask::DELETE
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO;

    let mut inotify = Inotify::init()?;
    let mut wd_map: HashMap<WatchDescriptor, PathBuf> = HashMap::new();

    for entry in WalkDir::new(&root).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_dir() {
            match inotify.watches().add(entry.path(), watch_mask) {
                Ok(wd) => {
                    wd_map.insert(wd, entry.path().to_path_buf());
                }
                Err(e) => warn!("failed to watch {:?}: {e}", entry.path()),
            }
        }
    }

    info!("watching {:?} ({} dirs)", root, wd_map.len());

    let handle = thread::Builder::new()
        .name("inotify-watcher".into())
        .spawn(move || {
            let mut buffer = vec![0u8; 65536];
            let mut pending_moves: HashMap<u32, (PathBuf, Instant)> = HashMap::new();
            // Long enough to keep MOVED_FROM/MOVED_TO pairs together under
            // load so a slow move isn't split into Delete + Create.
            const ORPHAN_TIMEOUT: Duration = Duration::from_millis(2000);

            loop {
                let events: Vec<_> = match inotify.read_events(&mut buffer) {
                    Ok(evs) => evs
                        .map(|e| {
                            (
                                e.wd.clone(),
                                e.mask,
                                e.cookie,
                                e.name.map(|n| PathBuf::from(n)),
                            )
                        })
                        .collect(),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Vec::new(),
                    Err(e) => {
                        warn!("inotify read error: {e}");
                        break;
                    }
                };

                for (wd, mask, cookie, name) in events {
                    // Watch-lifecycle events target the watched dir itself
                    // (no filename) and must be handled before the
                    // name-based logic below, which would otherwise skip
                    // them via `None => continue`.
                    if mask.contains(EventMask::IGNORED) {
                        if let Some(old) = wd_map.remove(&wd) {
                            debug!("inotify IGNORED, dropped stale watch for {old:?}");
                        }
                        continue;
                    }
                    if mask.contains(EventMask::MOVE_SELF) || mask.contains(EventMask::DELETE_SELF)
                    {
                        if let Some(dir) = wd_map.get(&wd).cloned() {
                            // Drop the stale entry. For a rename the watch
                            // follows the inode: the paired MOVED_FROM /
                            // MOVED_TO handler below rewrites wd_map and
                            // re-adds the subtree, so dropping here is safe
                            // whichever event arrives first. Anything missed
                            // is picked up by the next periodic rescan.
                            wd_map.remove(&wd);
                            let _ = inotify.watches().remove(wd);
                            debug!(
                                "inotify self-event mask={:?} dropped stale watch for {dir:?}",
                                mask
                            );
                        }
                        continue;
                    }

                    let name = match name {
                        Some(n) => n,
                        None => continue,
                    };

                    let dir = match wd_map.get(&wd) {
                        Some(d) => d.clone(),
                        None => continue,
                    };

                    let full = dir.join(&name);
                    let rel = match full.strip_prefix(&root) {
                        Ok(r) if !r.as_os_str().is_empty() => r.to_path_buf(),
                        _ => continue,
                    };

                    debug!("inotify mask={:?} rel={:?}", mask, rel);

                    if mask.contains(EventMask::MOVED_FROM) {
                        pending_moves.insert(cookie, (rel, Instant::now()));
                    } else if mask.contains(EventMask::MOVED_TO) {
                        let is_dir = mask.contains(EventMask::ISDIR);
                        if let Some((from_rel, _)) = pending_moves.remove(&cookie) {
                            debug!("inotify RENAME {:?} → {:?}", from_rel, rel);
                            // Fix stale bookkeeping: watches followed the
                            // inode, so point them at the new location.
                            rewrite_wd_map_for_rename(&mut wd_map, &root, &from_rel, &rel);
                            // Drop the kernel watch if the source path somehow
                            // survived (e.g. cross-device move leaving an
                            // empty dir); ignore errors — it is usually gone.
                            let from_abs = root.join(&from_rel);
                            if !from_abs.exists() {
                                let stale: Vec<WatchDescriptor> = wd_map
                                    .iter()
                                    .filter_map(|(w, p)| {
                                        if p == &from_abs {
                                            Some(w.clone())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();
                                for w in stale {
                                    wd_map.remove(&w);
                                    let _ = inotify.watches().remove(w);
                                }
                            }
                            if is_dir {
                                // Re-watch the new subtree: covers entries
                                // dropped by an early MOVE_SELF as well as
                                // children created mid-move.
                                add_watch_recursive(&mut inotify, &mut wd_map, &full, watch_mask);
                            }
                            let _ = tx.send(FsEvent::Renamed(from_rel, rel));
                        } else {
                            // Move from outside the watched tree (or an
                            // orphaned MOVED_TO): treat as a new path, and
                            // watch the whole subtree if it is a directory.
                            if is_dir {
                                add_watch_recursive(&mut inotify, &mut wd_map, &full, watch_mask);
                            }
                            let _ = tx.send(FsEvent::WriteComplete(rel));
                        }
                    } else if mask.contains(EventMask::DELETE)
                        || mask.contains(EventMask::DELETE_SELF)
                    {
                        let _ = tx.send(FsEvent::Deleted(rel));
                    } else if mask.contains(EventMask::CREATE) && mask.contains(EventMask::ISDIR) {
                        // Watch the whole new subtree, not just the top dir:
                        // it may already contain files created before the
                        // watch was added.
                        add_watch_recursive(&mut inotify, &mut wd_map, &full, watch_mask);
                        let _ = tx.send(FsEvent::Changed(rel));
                    } else if mask.contains(EventMask::CLOSE_WRITE) {
                        let _ = tx.send(FsEvent::WriteComplete(rel));
                    } else if mask.contains(EventMask::CREATE) {
                        debug!("inotify CREATE (awaiting CLOSE_WRITE) rel={:?}", rel);
                    }
                }
                let now = Instant::now();
                let expired: Vec<u32> = pending_moves
                    .iter()
                    .filter(|(_, (_, t))| now.duration_since(*t) > ORPHAN_TIMEOUT)
                    .map(|(k, _)| *k)
                    .collect();
                for cookie in expired {
                    if let Some((rel, _)) = pending_moves.remove(&cookie) {
                        debug!("inotify MOVED_FROM orphan (→ Deleted) rel={:?}", rel);
                        let _ = tx.send(FsEvent::Deleted(rel));
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
        })?;

    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::remap_path_for_rename;
    use std::path::Path;

    #[test]
    fn remap_dir_rename_prefix() {
        let root = Path::new("/root");
        let from_abs = root.join("a");
        let to_abs = root.join("b");
        assert_eq!(
            remap_path_for_rename(&from_abs, &from_abs, &to_abs),
            Some(to_abs.clone())
        );
        assert_eq!(
            remap_path_for_rename(&from_abs.join("sub/dir"), &from_abs, &to_abs),
            Some(to_abs.join("sub/dir"))
        );
        // Sibling with a similar name prefix must not match.
        assert_eq!(
            remap_path_for_rename(&root.join("ab"), &from_abs, &to_abs),
            None
        );
        assert_eq!(
            remap_path_for_rename(&root.join("other"), &from_abs, &to_abs),
            None
        );
    }
}
