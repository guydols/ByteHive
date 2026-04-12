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

pub fn start_watcher(
    root: PathBuf,
    tx: Sender<FsEvent>,
) -> std::io::Result<thread::JoinHandle<()>> {
    let root = root.canonicalize().unwrap_or(root);

    let watch_mask = WatchMask::CREATE
        | WatchMask::CLOSE_WRITE
        | WatchMask::DELETE
        | WatchMask::DELETE_SELF
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
            const ORPHAN_TIMEOUT: Duration = Duration::from_millis(500);

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
                        if let Some((from_rel, _)) = pending_moves.remove(&cookie) {
                            debug!("inotify RENAME {:?} → {:?}", from_rel, rel);
                            let _ = tx.send(FsEvent::Renamed(from_rel, rel));
                        } else {
                            let _ = tx.send(FsEvent::WriteComplete(rel));
                        }
                    } else if mask.contains(EventMask::DELETE)
                        || mask.contains(EventMask::DELETE_SELF)
                    {
                        let _ = tx.send(FsEvent::Deleted(rel));
                    } else if mask.contains(EventMask::CREATE) && mask.contains(EventMask::ISDIR) {
                        match inotify.watches().add(&full, watch_mask) {
                            Ok(new_wd) => {
                                wd_map.insert(new_wd, full);
                            }
                            Err(e) => warn!("failed to watch new dir {:?}: {e}", rel),
                        }
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
