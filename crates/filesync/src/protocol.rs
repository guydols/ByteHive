use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;

pub const BUNDLE_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const BUNDLE_MAX_FILES: usize = 500;
pub const LARGE_FILE_THRESHOLD: u64 = 8 * 1024 * 1024;
pub const FILE_CHUNK_SIZE: usize = 8 * 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
pub const PROTOCOL_VERSION: u32 = 5;
pub const DEBOUNCE_MS: u64 = 200;
pub const FILE_STABILITY_MS: u64 = 500;
pub const SUPPRESSION_SECS: u64 = 2;
pub const SEND_QUEUE_DEPTH: usize = 512;
pub const CLIENT_BROADCAST_DEPTH: usize = 512;
pub const HASH_THREADS: usize = 4;
pub const READ_THREADS: usize = 4;
pub const TMP_DIR: &str = ".filesync_tmp";
pub const FULL_SCAN_INTERVAL_SECS: u64 = 900; // 15 minutes

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    pub rel_path: PathBuf,
    pub size: u64,
    pub hash: [u8; 32],
    pub modified_ms: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub files: HashMap<PathBuf, FileMetadata>,
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileData {
    pub metadata: FileMetadata,
    pub content: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileBundle {
    pub files: Vec<FileData>,
    pub bundle_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    Hello {
        node_id: String,
        protocol_version: u32,
        credential: Option<String>,
    },
    ManifestExchange(Manifest),
    Bundle(FileBundle),
    Delete {
        paths: Vec<PathBuf>,
    },
    SyncComplete,
    LargeFileStart {
        metadata: FileMetadata,
        total_chunks: u32,
    },
    LargeFileChunk {
        path: PathBuf,
        chunk_index: u32,
        data: Vec<u8>,
    },

    LargeFileEnd {
        path: PathBuf,
        final_hash: [u8; 32],
    },

    RequestChunks {
        path: PathBuf,
        chunk_indices: Vec<u32>,
    },

    Rename {
        from: PathBuf,
        to: PathBuf,
    },
}

pub fn serialise_message(msg: &Message) -> io::Result<Vec<u8>> {
    let raw = bincode::serialize(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let compressed = lz4_flex::compress_prepend_size(&raw);

    if compressed.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "serialised frame {} B exceeds MAX_FRAME_BYTES {} B",
                compressed.len(),
                MAX_FRAME_BYTES
            ),
        ));
    }

    let mut frame = Vec::with_capacity(4 + compressed.len());
    frame.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    frame.extend_from_slice(&compressed);
    Ok(frame)
}

pub fn write_frame(w: &mut impl Write, frame: &[u8]) -> io::Result<()> {
    w.write_all(frame)?;
    w.flush()
}

pub fn write_message(w: &mut impl Write, msg: &Message) -> io::Result<()> {
    let frame = serialise_message(msg)?;
    write_frame(w, &frame)
}

pub fn read_message(r: &mut impl Read) -> io::Result<Message> {
    let mut hdr = [0u8; 4];
    r.read_exact(&mut hdr)?;
    let len = u32::from_be_bytes(hdr) as usize;

    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("incoming frame {len} B exceeds MAX_FRAME_BYTES {MAX_FRAME_BYTES} B"),
        ));
    }

    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;

    let raw = lz4_flex::decompress_size_prepended(&buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    bincode::deserialize(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
