use bytehive_filesync::common::expand_deleted_ancestors;
use std::path::PathBuf;
use tempfile::tempdir;

#[test]
fn expand_deleted_ancestors_adds_missing_parents() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut paths = vec![PathBuf::from("a/b/c/file.txt")];
    expand_deleted_ancestors(root, &mut paths);
    assert!(paths.contains(&PathBuf::from("a/b/c")));
    assert!(paths.contains(&PathBuf::from("a/b")));
    assert!(paths.contains(&PathBuf::from("a")));
}

#[test]
fn expand_deleted_ancestors_does_not_add_existing_dirs() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("existing")).unwrap();
    let mut paths = vec![PathBuf::from("existing/file.txt")];
    expand_deleted_ancestors(root, &mut paths);
    assert!(!paths.contains(&PathBuf::from("existing")));
}

#[test]
fn expand_deleted_ancestors_empty_input_stays_empty() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut paths: Vec<PathBuf> = vec![];
    expand_deleted_ancestors(root, &mut paths);
    assert!(paths.is_empty());
}

#[test]
fn expand_deleted_ancestors_root_level_file_has_no_ancestors() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut paths = vec![PathBuf::from("file.txt")];
    expand_deleted_ancestors(root, &mut paths);
    assert_eq!(
        paths.len(),
        1,
        "root-level file must not gain any ancestors"
    );
}

#[test]
fn expand_deleted_ancestors_shared_ancestor_added_once() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    // Neither a nor a/b exist on disk
    let mut paths = vec![
        PathBuf::from("a/b/file1.txt"),
        PathBuf::from("a/b/file2.txt"),
    ];
    expand_deleted_ancestors(root, &mut paths);
    let count_ab = paths.iter().filter(|p| *p == &PathBuf::from("a/b")).count();
    assert_eq!(count_ab, 1, "shared ancestor a/b must appear exactly once");
    let count_a = paths.iter().filter(|p| *p == &PathBuf::from("a")).count();
    assert_eq!(count_a, 1, "shared ancestor a must appear exactly once");
}
