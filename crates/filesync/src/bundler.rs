use crate::protocol::*;
use crossbeam_channel::Sender;
use log::{debug, warn};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_bundle_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn modified_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn stream_messages(root: &Path, rel_paths: &[PathBuf], tx: &Sender<Message>) {
    debug!(
        "bundler: stream_messages starting — {} path(s) from {:?}",
        rel_paths.len(),
        root
    );
    let mut cur: Vec<FileData> = Vec::new();
    let mut cur_bytes: usize = 0;

    for rel in rel_paths {
        let full = root.join(rel);

        let meta = match std::fs::metadata(&full) {
            Ok(m) => m,
            Err(e) => {
                warn!("metadata({full:?}): {e}");
                continue;
            }
        };

        if meta.is_dir() {
            cur.push(FileData {
                metadata: FileMetadata {
                    rel_path: rel.clone(),
                    size: 0,
                    hash: [0u8; 32],
                    modified_ms: modified_ms(&meta),
                    is_dir: true,
                },
                content: Vec::new(),
            });
            continue;
        }

        if meta.len() >= LARGE_FILE_THRESHOLD as u64 {
            debug!(
                "bundler: {:?} is large ({} B >= threshold {} B), streaming directly",
                rel,
                meta.len(),
                LARGE_FILE_THRESHOLD
            );
            flush_bundle(&mut cur, &mut cur_bytes, tx);

            if let Err(e) = stream_large_file(root, rel, &meta, tx) {
                warn!("large-file stream({rel:?}): {e}");
            }
            continue;
        }

        let content = match std::fs::read(&full) {
            Ok(c) => c,
            Err(e) => {
                warn!("read({full:?}): {e}");
                continue;
            }
        };
        let size = content.len();
        let hash: [u8; 32] = blake3::hash(&content).into();

        if !cur.is_empty() && (cur.len() >= BUNDLE_MAX_FILES || cur_bytes + size > BUNDLE_MAX_BYTES)
        {
            flush_bundle(&mut cur, &mut cur_bytes, tx);
        }

        cur_bytes += size;
        cur.push(FileData {
            metadata: FileMetadata {
                rel_path: rel.clone(),
                size: size as u64,
                hash,
                modified_ms: modified_ms(&meta),
                is_dir: false,
            },
            content,
        });
    }

    flush_bundle(&mut cur, &mut cur_bytes, tx);
}

fn flush_bundle(cur: &mut Vec<FileData>, cur_bytes: &mut usize, tx: &Sender<Message>) {
    if cur.is_empty() {
        return;
    }
    let file_count = cur.iter().filter(|f| !f.metadata.is_dir).count();
    let dir_count = cur.iter().filter(|f| f.metadata.is_dir).count();
    let id = next_bundle_id();
    debug!(
        "bundler: flush_bundle id={} files={} dirs={} size={} B",
        id, file_count, dir_count, cur_bytes
    );
    let bundle = Message::Bundle(FileBundle {
        files: std::mem::take(cur),
        bundle_id: id,
    });
    *cur_bytes = 0;

    let _ = tx.send(bundle);
}

fn stream_large_file(
    root: &Path,
    rel: &PathBuf,
    meta: &std::fs::Metadata,
    tx: &Sender<Message>,
) -> std::io::Result<()> {
    let full = root.join(rel);
    let file_size = meta.len();
    let total_chunks = ((file_size as usize + FILE_CHUNK_SIZE - 1) / FILE_CHUNK_SIZE).max(1) as u32;
    let mms = modified_ms(meta);

    debug!(
        "bundler: large file {:?} — size={} B total_chunks={} chunk_size={} B",
        rel, file_size, total_chunks, FILE_CHUNK_SIZE
    );

    let hash_start = std::time::Instant::now();
    let final_hash: [u8; 32] = {
        let mut hasher = blake3::Hasher::new();
        let mut f = std::fs::File::open(&full)?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        hasher.finalize().into()
    };
    debug!(
        "bundler: large file {:?} hash computed in {}ms",
        rel,
        hash_start.elapsed().as_millis()
    );

    debug!("bundler: sending LargeFileStart for {:?}", rel);
    tx.send(Message::LargeFileStart {
        metadata: FileMetadata {
            rel_path: rel.clone(),
            size: file_size,
            hash: final_hash,
            modified_ms: mms,
            is_dir: false,
        },
        total_chunks,
    })
    .ok();

    let mut file = std::io::BufReader::with_capacity(FILE_CHUNK_SIZE, std::fs::File::open(&full)?);
    let mut chunk_index: u32 = 0;
    let send_start = std::time::Instant::now();

    loop {
        let mut data = vec![0u8; FILE_CHUNK_SIZE];
        let mut filled = 0;
        while filled < FILE_CHUNK_SIZE {
            let n = file.read(&mut data[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        data.truncate(filled);

        debug!(
            "bundler: sending LargeFileChunk {:?} chunk={}/{} size={} B",
            rel,
            chunk_index,
            total_chunks - 1,
            filled
        );
        tx.send(Message::LargeFileChunk {
            path: rel.clone(),
            chunk_index,
            data,
        })
        .ok();

        chunk_index += 1;
    }

    debug!(
        "bundler: sending LargeFileEnd {:?} — {} chunk(s) queued in {}ms",
        rel,
        chunk_index,
        send_start.elapsed().as_millis()
    );
    tx.send(Message::LargeFileEnd {
        path: rel.clone(),
        final_hash,
    })
    .ok();

    Ok(())
}
