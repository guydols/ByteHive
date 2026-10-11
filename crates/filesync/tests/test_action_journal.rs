//! Unit tests for the client action journal (`action_journal.rs`):
//! append/replay, coalescing rules, torn-tail recovery, and ack semantics.

use std::io::Write;
use std::path::{Path, PathBuf};

use bytehive_filesync::action_journal::{ActionJournal, JournalEntry, JournalOp};

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

fn journal_in(root: &Path) -> ActionJournal {
    ActionJournal::open(root)
}

fn summary(j: &ActionJournal) -> Vec<(String, String, u64)> {
    j.snapshot()
        .iter()
        .map(|e: &JournalEntry| {
            let kind = match &e.op {
                JournalOp::Upsert => "upsert".to_string(),
                JournalOp::Delete => "delete".to_string(),
                JournalOp::Rename { to } => format!("rename->{}", to.display()),
            };
            (kind, e.path.display().to_string(), e.action_ms)
        })
        .collect()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "bh_journal_{name}_{:x}",
        bytehive_filesync::timestamp_id()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn fill_while_disconnected_records_each_action() {
    let root = tmp("fill");
    let j = journal_in(&root);
    j.upsert(p("a.txt"), 100).unwrap();
    j.delete(p("b.txt"), 200).unwrap();
    j.rename(p("c.txt"), p("d.txt"), 300).unwrap();
    assert!(!j.is_empty());
    let seqs: Vec<u64> = j.snapshot().iter().map(|e| e.seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seqs must increase");
    assert_eq!(
        summary(&j),
        vec![
            ("upsert".into(), "a.txt".into(), 100),
            ("delete".into(), "b.txt".into(), 200),
            ("rename->d.txt".into(), "c.txt".into(), 300),
        ]
    );
}

#[test]
fn reopen_replays_pending_entries() {
    let root = tmp("reopen");
    {
        let j = journal_in(&root);
        j.upsert(p("a.txt"), 100).unwrap();
        j.delete(p("b.txt"), 200).unwrap();
        j.rename(p("c.txt"), p("d.txt"), 300).unwrap();
    }
    let reopened = journal_in(&root);
    assert_eq!(summary(&reopened).len(), 3);
    assert_eq!(
        summary(&reopened),
        vec![
            ("upsert".into(), "a.txt".into(), 100),
            ("delete".into(), "b.txt".into(), 200),
            ("rename->d.txt".into(), "c.txt".into(), 300),
        ]
    );
    // New appends continue after the replayed sequence numbers.
    let max_before = reopened.snapshot().iter().map(|e| e.seq).max().unwrap();
    reopened.upsert(p("e.txt"), 400).unwrap();
    let max_after = reopened.snapshot().iter().map(|e| e.seq).max().unwrap();
    assert!(max_after > max_before);
}

#[test]
fn upsert_twice_keeps_later_time() {
    let root = tmp("upsert2");
    let j = journal_in(&root);
    j.upsert(p("a.txt"), 100).unwrap();
    j.upsert(p("a.txt"), 250).unwrap();
    assert_eq!(summary(&j), vec![("upsert".into(), "a.txt".into(), 250)]);
}

#[test]
fn upsert_then_delete_leaves_delete() {
    let root = tmp("ud");
    let j = journal_in(&root);
    j.upsert(p("a.txt"), 100).unwrap();
    j.delete(p("a.txt"), 200).unwrap();
    assert_eq!(summary(&j), vec![("delete".into(), "a.txt".into(), 200)]);
}

#[test]
fn delete_then_recreate_leaves_upsert() {
    let root = tmp("du");
    let j = journal_in(&root);
    j.delete(p("a.txt"), 100).unwrap();
    j.upsert(p("a.txt"), 200).unwrap();
    assert_eq!(summary(&j), vec![("upsert".into(), "a.txt".into(), 200)]);
}

#[test]
fn delete_dir_drops_pending_children() {
    let root = tmp("deldir");
    let j = journal_in(&root);
    j.upsert(p("d/x.txt"), 1).unwrap();
    j.upsert(p("d/y/z.txt"), 2).unwrap();
    j.upsert(p("e.txt"), 3).unwrap();
    j.delete(p("d"), 4).unwrap();
    assert_eq!(
        summary(&j),
        vec![
            ("upsert".into(), "e.txt".into(), 3),
            ("delete".into(), "d".into(), 4),
        ]
    );
}

#[test]
fn rename_of_offline_created_file_becomes_upsert_at_target() {
    let root = tmp("rename_created");
    let j = journal_in(&root);
    j.upsert(p("f.txt"), 1).unwrap();
    j.rename(p("f.txt"), p("g.txt"), 2).unwrap();
    let entries = summary(&j);
    assert!(
        !entries.iter().any(|(k, path, _)| k == "upsert" && path == "f.txt"),
        "content recorded at the old name must not survive the rename: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|(k, path, t)| k == "upsert" && path == "g.txt" && *t == 2),
        "content must be sent at the new name: {entries:?}"
    );
}

#[test]
fn directory_rename_keeps_children_before_the_rename() {
    // Ordered journal: children recorded before a directory rename keep their
    // pre-rename path, so the peer applies them before moving the directory.
    let root = tmp("rename_dir");
    let j = journal_in(&root);
    j.upsert(p("d/x.txt"), 1).unwrap();
    j.rename(p("d"), p("e"), 2).unwrap();
    assert_eq!(
        summary(&j),
        vec![
            ("upsert".into(), "d/x.txt".into(), 1),
            ("rename->e".into(), "d".into(), 2),
        ]
    );
}

#[test]
fn rename_out_of_deleted_dir_becomes_upsert_of_target() {
    let root = tmp("rename_out");
    let j = journal_in(&root);
    j.rename(p("d/a.txt"), p("keep.txt"), 5).unwrap();
    j.delete(p("d"), 6).unwrap();
    let entries = summary(&j);
    assert!(
        entries
            .iter()
            .any(|(k, path, t)| k == "upsert" && path == "keep.txt" && *t == 5),
        "the moved file still exists and must be delivered: {entries:?}"
    );
    assert!(entries
        .iter()
        .any(|(k, path, _)| k == "delete" && path == "d"));
    assert!(!entries.iter().any(|(k, _, _)| k.starts_with("rename")));
}

#[test]
fn torn_tail_is_ignored_and_backed_up() {
    let root = tmp("torn");
    {
        let j = journal_in(&root);
        j.upsert(p("a.txt"), 100).unwrap();
        j.upsert(p("b.txt"), 200).unwrap();
    }
    let path = root.join(".bh_filesync").join("action-journal.jsonl");
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(br#"{"seq":9,"op":"upsert","pa"#).unwrap();
    drop(f);

    let j = journal_in(&root);
    assert_eq!(
        summary(&j),
        vec![
            ("upsert".into(), "a.txt".into(), 100),
            ("upsert".into(), "b.txt".into(), 200),
        ]
    );
    let backups: Vec<_> = std::fs::read_dir(root.join(".bh_filesync"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("action-journal.corrupt-")
        })
        .collect();
    assert_eq!(backups.len(), 1, "torn file must be backed up once");
    // The rewritten file must accept appends without corrupting the next line.
    j.upsert(p("c.txt"), 300).unwrap();
    let reopened = journal_in(&root);
    assert_eq!(summary(&reopened).len(), 3);
}

#[test]
fn ack_keeps_entries_recorded_after_the_batch() {
    let root = tmp("ack");
    let j = journal_in(&root);
    j.upsert(p("a.txt"), 100).unwrap();
    j.upsert(p("b.txt"), 200).unwrap();
    let upto = j.snapshot().last().unwrap().seq;

    // An event arrives while the batch is in flight.
    j.upsert(p("c.txt"), 300).unwrap();
    j.ack(upto).unwrap();

    assert_eq!(summary(&j), vec![("upsert".into(), "c.txt".into(), 300)]);
    assert_eq!(j.acked_upto(), upto);

    // The ack survives a restart: only the late entry is replayed.
    let reopened = journal_in(&root);
    assert_eq!(summary(&reopened), vec![("upsert".into(), "c.txt".into(), 300)]);
    reopened.ack(reopened.snapshot()[0].seq).unwrap();
    assert!(reopened.is_empty());
}
