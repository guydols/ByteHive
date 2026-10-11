//! Persistent journal of user actions captured while the client is
//! disconnected (or in flight). Client-only.
//!
//! - File: `<root>/.bh_filesync/action-journal.jsonl`, one JSON record per
//!   line, fsynced per append. Records are either entries
//!   (`{seq, op, path, to?, action_ms}`) or acknowledgements (`{ack}`).
//! - Metadata only: upsert entries carry no bytes; content is read at drain.
//! - Entries are applied in `seq` order and coalesced on append and on
//!   replay with identical rules, so replay reproduces the live state.
//! - An entry leaves the journal only once the peer acknowledged a batch
//!   that contains it (`ack`). Re-sending an unacked entry is idempotent.
//! - A torn tail or unparsable line is skipped; the file is backed up to
//!   `action-journal.corrupt-<ts>.jsonl` and rewritten compacted.

use crate::ledger::now_ms;
use crate::protocol::BH_DIR;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// File name of the action journal inside `<root>/.bh_filesync`.
pub const JOURNAL_FILE_NAME: &str = "action-journal.jsonl";
/// Rewrite the file once it holds at least this many records.
const COMPACT_MIN_LINES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalOp {
    Upsert,
    Delete,
    Rename { to: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub seq: u64,
    pub op: JournalOp,
    /// Path for upsert/delete; source path for rename.
    pub path: PathBuf,
    /// Wall-clock time of the user action (event receipt), in ms.
    pub action_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum Line {
    Ack {
        ack: u64,
    },
    Entry {
        seq: u64,
        op: String,
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<PathBuf>,
        action_ms: u64,
    },
}

impl JournalEntry {
    fn to_line(&self) -> Line {
        let (op, to) = match &self.op {
            JournalOp::Upsert => ("upsert", None),
            JournalOp::Delete => ("delete", None),
            JournalOp::Rename { to } => ("rename", Some(to.clone())),
        };
        Line::Entry {
            seq: self.seq,
            op: op.to_string(),
            path: self.path.clone(),
            to,
            action_ms: self.action_ms,
        }
    }
}

#[derive(Default)]
struct State {
    file: Option<File>,
    entries: BTreeMap<u64, JournalEntry>,
    /// Index: `entry.path` -> seqs.
    by_path: BTreeMap<PathBuf, BTreeSet<u64>>,
    /// Index: rename target -> seqs.
    by_to: BTreeMap<PathBuf, BTreeSet<u64>>,
    acked: u64,
    next_seq: u64,
    lines_on_disk: usize,
}

/// Append-only, fsynced journal of pending user actions for one sync root.
pub struct ActionJournal {
    path: PathBuf,
    state: Mutex<State>,
}

fn under(index: &BTreeMap<PathBuf, BTreeSet<u64>>, prefix: &Path) -> Vec<u64> {
    index
        .range(prefix.to_path_buf()..)
        .take_while(|(k, _)| k.starts_with(prefix))
        .flat_map(|(_, seqs)| seqs.iter().copied())
        .collect()
}

fn exact(index: &BTreeMap<PathBuf, BTreeSet<u64>>, path: &Path) -> Vec<u64> {
    index
        .get(path)
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default()
}

impl State {
    fn insert(&mut self, e: JournalEntry) {
        self.by_path
            .entry(e.path.clone())
            .or_default()
            .insert(e.seq);
        if let JournalOp::Rename { to } = &e.op {
            self.by_to.entry(to.clone()).or_default().insert(e.seq);
        }
        self.next_seq = self.next_seq.max(e.seq + 1);
        self.entries.insert(e.seq, e);
    }

    fn remove(&mut self, seq: u64) -> Option<JournalEntry> {
        let e = self.entries.remove(&seq)?;
        drop_index(&mut self.by_path, &e.path, seq);
        if let JournalOp::Rename { to } = &e.op {
            drop_index(&mut self.by_to, to, seq);
        }
        Some(e)
    }

    fn ack_local(&mut self, upto: u64) {
        let victims: Vec<u64> = self.entries.range(..=upto).map(|(s, _)| *s).collect();
        for seq in victims {
            self.remove(seq);
        }
        self.acked = self.acked.max(upto);
        self.next_seq = self.next_seq.max(upto + 1);
    }

    /// Apply one entry with the coalescing rules (shared by append & replay).
    fn apply(&mut self, new: JournalEntry) {
        match &new.op {
            JournalOp::Upsert => {
                let drop: Vec<u64> = exact(&self.by_path, &new.path)
                    .into_iter()
                    .filter(|s| {
                        matches!(
                            self.entries.get(s).map(|e| &e.op),
                            Some(JournalOp::Upsert | JournalOp::Delete)
                        )
                    })
                    .collect();
                for s in drop {
                    self.remove(s);
                }
                self.insert(new);
            }
            JournalOp::Delete => {
                let mut victims: BTreeSet<u64> = under(&self.by_path, &new.path)
                    .into_iter()
                    .collect();
                victims.extend(under(&self.by_to, &new.path));
                for seq in victims {
                    let Some(v) = self.entries.get(&seq).cloned() else {
                        continue;
                    };
                    match &v.op {
                        JournalOp::Upsert | JournalOp::Delete => {
                            self.remove(seq);
                        }
                        JournalOp::Rename { to } => {
                            let from_in = v.path.starts_with(&new.path);
                            let to_in = to.starts_with(&new.path);
                            match (from_in, to_in) {
                                (true, true) => {
                                    self.remove(seq);
                                }
                                (true, false) => {
                                    // Source is deleted, content lives on at `to`.
                                    let to = to.clone();
                                    self.remove(seq);
                                    self.insert(JournalEntry {
                                        seq,
                                        op: JournalOp::Upsert,
                                        path: to,
                                        action_ms: v.action_ms,
                                    });
                                }
                                (false, true) => {
                                    // Target is deleted; the source still exists remotely.
                                    self.remove(seq);
                                    self.insert(JournalEntry {
                                        seq,
                                        op: JournalOp::Delete,
                                        path: v.path.clone(),
                                        action_ms: v.action_ms,
                                    });
                                }
                                (false, false) => {}
                            }
                        }
                    }
                }
                self.insert(new);
            }
            JournalOp::Rename { .. } => {
                let drop: Vec<u64> = exact(&self.by_path, &new.path)
                    .into_iter()
                    .filter(|s| {
                        matches!(
                            self.entries.get(s).map(|e| &e.op),
                            Some(JournalOp::Upsert)
                        )
                    })
                    .collect();
                for s in drop {
                    self.remove(s);
                }
                if let JournalOp::Rename { to } = &new.op {
                    let to = to.clone();
                    let drop_to: Vec<u64> = exact(&self.by_path, &to)
                        .into_iter()
                        .filter(|s| {
                            matches!(
                                self.entries.get(s).map(|e| &e.op),
                                Some(JournalOp::Upsert | JournalOp::Delete)
                            )
                        })
                        .collect();
                    for s in drop_to {
                        self.remove(s);
                    }
                }
                self.insert(new);
            }
        }
    }

    fn apply_line(&mut self, line: Line) -> bool {
        match line {
            Line::Ack { ack } => {
                self.ack_local(ack);
                true
            }
            Line::Entry {
                seq,
                op,
                path,
                to,
                action_ms,
            } => {
                let op = match (op.as_str(), to) {
                    ("upsert", _) => JournalOp::Upsert,
                    ("delete", _) => JournalOp::Delete,
                    ("rename", Some(to)) => JournalOp::Rename { to },
                    _ => return false,
                };
                self.apply(JournalEntry {
                    seq,
                    op,
                    path,
                    action_ms,
                });
                true
            }
        }
    }
}

fn drop_index(index: &mut BTreeMap<PathBuf, BTreeSet<u64>>, key: &Path, seq: u64) {
    if let Some(set) = index.get_mut(key) {
        set.remove(&seq);
        if set.is_empty() {
            index.remove(key);
        }
    }
}

impl ActionJournal {
    /// Open (or create) the journal for `root`, replaying existing records.
    /// Never fails: unreadable or torn data is backed up and skipped.
    pub fn open(root: &Path) -> Self {
        let path = root.join(BH_DIR).join(JOURNAL_FILE_NAME);
        let mut st = State {
            next_seq: 1,
            ..State::default()
        };
        let mut damaged = false;
        if let Ok(raw) = std::fs::read_to_string(&path) {
            damaged = !raw.is_empty() && !raw.ends_with('\n');
            let mut lines: Vec<&str> = raw.split('\n').collect();
            // The last element is either "" (clean EOF) or the torn tail.
            lines.pop();
            for line in lines {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Line>(line) {
                    Ok(l) => {
                        if !st.apply_line(l) {
                            damaged = true;
                        }
                    }
                    Err(_) => damaged = true,
                }
                st.lines_on_disk += 1;
            }
        }
        let journal = Self {
            path,
            state: Mutex::new(st),
        };
        if damaged {
            let backup = journal
                .path
                .with_file_name(format!("action-journal.corrupt-{}.jsonl", now_ms()));
            let _ = std::fs::copy(&journal.path, &backup);
            log::warn!(
                "action journal: damaged records at {:?}; backed up to {:?}",
                journal.path,
                backup
            );
            let mut st = journal.state.lock();
            if let Err(e) = write_compacted(&journal.path, &mut st) {
                log::warn!("action journal: compaction after damage failed: {e}");
            }
        }
        journal
    }

    /// Path of the journal file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when no unacknowledged entries remain.
    pub fn is_empty(&self) -> bool {
        self.state.lock().entries.is_empty()
    }

    /// Number of unacknowledged (coalesced) entries.
    pub fn len(&self) -> usize {
        self.state.lock().entries.len()
    }

    /// Highest sequence number acknowledged by the peer so far.
    pub fn acked_upto(&self) -> u64 {
        self.state.lock().acked
    }

    /// Unacknowledged entries in sequence order.
    pub fn snapshot(&self) -> Vec<JournalEntry> {
        self.state.lock().entries.values().cloned().collect()
    }

    /// Record a create/modify of `path` at `action_ms`.
    pub fn upsert(&self, path: PathBuf, action_ms: u64) -> io::Result<()> {
        self.append(JournalOp::Upsert, path, action_ms)
    }

    /// Record a removal of `path` (and, implicitly, its subtree) at `action_ms`.
    pub fn delete(&self, path: PathBuf, action_ms: u64) -> io::Result<()> {
        self.append(JournalOp::Delete, path, action_ms)
    }

    /// Record `from` renamed to `to` at `action_ms`. When `from` had a pending
    /// upsert (content not yet sent), that content is re-recorded at `to`.
    pub fn rename(&self, from: PathBuf, to: PathBuf, action_ms: u64) -> io::Result<()> {
        let mut st = self.state.lock();
        let had_upsert = exact(&st.by_path, &from).iter().any(|s| {
            matches!(
                st.entries.get(s).map(|e| &e.op),
                Some(JournalOp::Upsert)
            )
        });
        let seq = st.next_seq;
        let entry = JournalEntry {
            seq,
            op: JournalOp::Rename { to: to.clone() },
            path: from,
            action_ms,
        };
        append_line(&self.path, &mut st, &entry.to_line())?;
        st.apply(entry);
        if had_upsert {
            let seq = st.next_seq;
            let entry = JournalEntry {
                seq,
                op: JournalOp::Upsert,
                path: to,
                action_ms,
            };
            append_line(&self.path, &mut st, &entry.to_line())?;
            st.apply(entry);
        }
        maybe_compact(&self.path, &mut st);
        Ok(())
    }

    /// Mark entries up to and including `upto_seq` as delivered and applied
    /// by the peer. Entries recorded after the batch started survive.
    pub fn ack(&self, upto_seq: u64) -> io::Result<()> {
        let mut st = self.state.lock();
        if upto_seq <= st.acked {
            st.ack_local(upto_seq);
            return Ok(());
        }
        append_line(&self.path, &mut st, &Line::Ack { ack: upto_seq })?;
        st.ack_local(upto_seq);
        maybe_compact(&self.path, &mut st);
        Ok(())
    }

    fn append(&self, op: JournalOp, path: PathBuf, action_ms: u64) -> io::Result<()> {
        let mut st = self.state.lock();
        let entry = JournalEntry {
            seq: st.next_seq,
            op,
            path,
            action_ms,
        };
        append_line(&self.path, &mut st, &entry.to_line())?;
        st.apply(entry);
        maybe_compact(&self.path, &mut st);
        Ok(())
    }
}

/// Rewrite the file once superseded and acked records accumulate, so a long
/// disconnected session does not grow it without bound.
fn maybe_compact(path: &Path, st: &mut State) {
    if st.lines_on_disk >= COMPACT_MIN_LINES {
        if let Err(e) = write_compacted(path, st) {
            log::warn!("action journal: compaction failed: {e}");
        }
    }
}

fn append_line(path: &Path, st: &mut State, line: &Line) -> io::Result<()> {
    if st.file.is_none() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        st.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?,
        );
    }
    let mut raw = serde_json::to_string(line)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    raw.push('\n');
    let file = st.file.as_mut().expect("journal file opened above");
    file.write_all(raw.as_bytes())?;
    file.sync_data()?;
    st.lines_on_disk += 1;
    Ok(())
}

/// Rewrite the journal atomically: the acked marker, then pending entries
/// in sequence order. Replaying the result yields the same pending set.
fn write_compacted(path: &Path, st: &mut State) -> io::Result<()> {
    let tmp = path.with_extension("jsonl.tmp");
    let mut out = String::new();
    if st.acked > 0 {
        out.push_str(
            &serde_json::to_string(&Line::Ack { ack: st.acked })
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        );
        out.push('\n');
    }
    let mut lines = usize::from(st.acked > 0);
    for e in st.entries.values() {
        out.push_str(
            &serde_json::to_string(&e.to_line())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        );
        out.push('\n');
        lines += 1;
    }
    {
        let mut f = File::create(&tmp)?;
        f.write_all(out.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    st.file = None;
    st.lines_on_disk = lines;
    Ok(())
}
