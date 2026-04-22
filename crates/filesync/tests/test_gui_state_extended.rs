use bytehive_filesync::gui::state::{
    flatten_tree, Conflict, ConflictKind, FileNode, SideTab, SyncSnapshot,
};

// ─── ConflictKind ─────────────────────────────────────────────────────────────

#[test]
fn conflict_kind_both_modified_label() {
    assert_eq!(ConflictKind::BothModified.label(), "Both modified");
}

#[test]
fn conflict_kind_local_only_label() {
    assert_eq!(ConflictKind::LocalOnly.label(), "Deleted remotely");
}

#[test]
fn conflict_kind_remote_only_label() {
    assert_eq!(ConflictKind::RemoteOnly.label(), "Deleted locally");
}

#[test]
fn conflict_kind_both_created_label() {
    assert_eq!(ConflictKind::BothCreated.label(), "Both created");
}

#[test]
fn conflict_kind_equality_same_variants() {
    assert_eq!(ConflictKind::BothModified, ConflictKind::BothModified);
    assert_eq!(ConflictKind::LocalOnly, ConflictKind::LocalOnly);
    assert_eq!(ConflictKind::RemoteOnly, ConflictKind::RemoteOnly);
    assert_eq!(ConflictKind::BothCreated, ConflictKind::BothCreated);
}

#[test]
fn conflict_kind_inequality_different_variants() {
    assert_ne!(ConflictKind::BothModified, ConflictKind::LocalOnly);
    assert_ne!(ConflictKind::LocalOnly, ConflictKind::RemoteOnly);
    assert_ne!(ConflictKind::RemoteOnly, ConflictKind::BothCreated);
    assert_ne!(ConflictKind::BothCreated, ConflictKind::BothModified);
}

// ─── Conflict ─────────────────────────────────────────────────────────────────

#[test]
fn conflict_fields_are_accessible() {
    let c = Conflict {
        id: 42,
        filename: "document.txt".into(),
        folder_path: "/home/user/sync/docs".into(),
        local_modified: "2024-06-01 10:00".into(),
        remote_modified: "2024-06-01 11:30".into(),
        kind: ConflictKind::BothModified,
    };
    assert_eq!(c.id, 42);
    assert_eq!(c.filename, "document.txt");
    assert_eq!(c.folder_path, "/home/user/sync/docs");
    assert_eq!(c.local_modified, "2024-06-01 10:00");
    assert_eq!(c.remote_modified, "2024-06-01 11:30");
    assert_eq!(c.kind, ConflictKind::BothModified);
}

#[test]
fn conflict_clone_produces_equal_fields() {
    let c = Conflict {
        id: 7,
        filename: "notes.md".into(),
        folder_path: "/sync".into(),
        local_modified: "t1".into(),
        remote_modified: "t2".into(),
        kind: ConflictKind::BothCreated,
    };
    let cloned = c.clone();
    assert_eq!(cloned.id, c.id);
    assert_eq!(cloned.filename, c.filename);
    assert_eq!(cloned.folder_path, c.folder_path);
    assert_eq!(cloned.kind, c.kind);
}

#[test]
fn sync_snapshot_conflicts_initially_empty() {
    let snap = SyncSnapshot::default();
    assert!(snap.conflicts.is_empty());
}

#[test]
fn sync_snapshot_conflicts_can_be_pushed() {
    let mut snap = SyncSnapshot::default();
    snap.conflicts.push(Conflict {
        id: 1,
        filename: "file.txt".into(),
        folder_path: "/sync".into(),
        local_modified: "t1".into(),
        remote_modified: "t2".into(),
        kind: ConflictKind::BothModified,
    });
    assert_eq!(snap.conflicts.len(), 1);
}

#[test]
fn sync_snapshot_conflicts_multiple_kinds() {
    let mut snap = SyncSnapshot::default();
    let kinds = [
        ConflictKind::BothModified,
        ConflictKind::LocalOnly,
        ConflictKind::RemoteOnly,
        ConflictKind::BothCreated,
    ];
    for (i, kind) in kinds.into_iter().enumerate() {
        snap.conflicts.push(Conflict {
            id: i,
            filename: format!("file_{i}.txt"),
            folder_path: "/sync".into(),
            local_modified: "t1".into(),
            remote_modified: "t2".into(),
            kind,
        });
    }
    assert_eq!(snap.conflicts.len(), 4);
    assert_eq!(snap.conflicts[0].kind, ConflictKind::BothModified);
    assert_eq!(snap.conflicts[3].kind, ConflictKind::BothCreated);
}

// ─── FileNode ─────────────────────────────────────────────────────────────────

#[test]
fn file_node_dir_constructor_sets_correct_fields() {
    let node = FileNode::dir(1, "src", "/project/src", vec![]);
    assert_eq!(node.id, 1);
    assert_eq!(node.name, "src");
    assert_eq!(node.path, "/project/src");
    assert!(node.is_dir);
    assert!(node.included);
    assert!(node.expanded);
    assert!(node.children.is_empty());
}

#[test]
fn file_node_file_constructor_sets_correct_fields() {
    let node = FileNode::file(2, "main.rs", "/project/src/main.rs");
    assert_eq!(node.id, 2);
    assert_eq!(node.name, "main.rs");
    assert_eq!(node.path, "/project/src/main.rs");
    assert!(!node.is_dir);
    assert!(node.included);
    assert!(!node.expanded);
    assert!(node.children.is_empty());
}

#[test]
fn file_node_dir_with_children_stores_them() {
    let child1 = FileNode::file(2, "a.rs", "/src/a.rs");
    let child2 = FileNode::file(3, "b.rs", "/src/b.rs");
    let dir = FileNode::dir(1, "src", "/src", vec![child1, child2]);
    assert_eq!(dir.children.len(), 2);
    assert_eq!(dir.children[0].name, "a.rs");
    assert_eq!(dir.children[1].name, "b.rs");
}

#[test]
fn file_node_clone_is_independent() {
    let original = FileNode::file(1, "x.txt", "/x.txt");
    let mut cloned = original.clone();
    cloned.included = false;
    assert!(
        original.included,
        "original should be unaffected by clone mutation"
    );
    assert!(!cloned.included);
}

// ─── flatten_tree ─────────────────────────────────────────────────────────────

#[test]
fn flatten_tree_empty_input_returns_empty() {
    let flat = flatten_tree(&[]);
    assert!(flat.is_empty());
}

#[test]
fn flatten_tree_single_file_no_children() {
    let nodes = vec![FileNode::file(1, "readme.txt", "/readme.txt")];
    let flat = flatten_tree(&nodes);
    assert_eq!(flat.len(), 1);
    assert_eq!(flat[0].name, "readme.txt");
    assert_eq!(flat[0].depth, 0);
    assert!(!flat[0].is_dir);
    assert!(!flat[0].has_children);
}

#[test]
fn flatten_tree_collapsed_dir_hides_children() {
    let mut dir = FileNode::dir(
        1,
        "src",
        "/src",
        vec![FileNode::file(2, "main.rs", "/src/main.rs")],
    );
    dir.expanded = false;
    let flat = flatten_tree(&[dir]);
    assert_eq!(
        flat.len(),
        1,
        "collapsed dir should not expose its children"
    );
    assert_eq!(flat[0].name, "src");
    assert!(
        flat[0].has_children,
        "has_children should be true even when collapsed"
    );
}

#[test]
fn flatten_tree_expanded_dir_exposes_children() {
    let dir = FileNode::dir(
        1,
        "src",
        "/src",
        vec![
            FileNode::file(2, "main.rs", "/src/main.rs"),
            FileNode::file(3, "lib.rs", "/src/lib.rs"),
        ],
    );
    let flat = flatten_tree(&[dir]);
    assert_eq!(flat.len(), 3);
    assert_eq!(flat[0].name, "src");
    assert_eq!(flat[0].depth, 0);
    assert_eq!(flat[1].name, "main.rs");
    assert_eq!(flat[1].depth, 1);
    assert_eq!(flat[2].name, "lib.rs");
    assert_eq!(flat[2].depth, 1);
}

#[test]
fn flatten_tree_depth_increments_for_nested_dirs() {
    let level2_file = FileNode::file(4, "deep.rs", "/a/b/deep.rs");
    let level2_dir = FileNode::dir(3, "b", "/a/b", vec![level2_file]);
    let level1_dir = FileNode::dir(2, "a_inner", "/a/a_inner", vec![]);
    let root_dir = FileNode::dir(1, "a", "/a", vec![level2_dir, level1_dir]);

    let flat = flatten_tree(&[root_dir]);
    assert_eq!(flat.len(), 4); // a, b, deep.rs, a_inner
    assert_eq!(flat[0].depth, 0); // a
    assert_eq!(flat[1].depth, 1); // b
    assert_eq!(flat[2].depth, 2); // deep.rs
    assert_eq!(flat[3].depth, 1); // a_inner
}

#[test]
fn flatten_tree_three_levels_deep() {
    let leaf = FileNode::file(4, "leaf.rs", "/a/b/c/leaf.rs");
    let level3 = FileNode::dir(3, "c", "/a/b/c", vec![leaf]);
    let level2 = FileNode::dir(2, "b", "/a/b", vec![level3]);
    let root = FileNode::dir(1, "a", "/a", vec![level2]);

    let flat = flatten_tree(&[root]);
    assert_eq!(flat.len(), 4);
    assert_eq!(flat[0].depth, 0);
    assert_eq!(flat[1].depth, 1);
    assert_eq!(flat[2].depth, 2);
    assert_eq!(flat[3].depth, 3);
}

#[test]
fn flatten_tree_multiple_root_nodes() {
    let nodes = vec![
        FileNode::file(1, "a.txt", "/a.txt"),
        FileNode::dir(
            2,
            "src",
            "/src",
            vec![FileNode::file(3, "x.rs", "/src/x.rs")],
        ),
        FileNode::file(4, "b.txt", "/b.txt"),
    ];
    let flat = flatten_tree(&nodes);
    assert_eq!(flat.len(), 4); // a.txt, src, x.rs, b.txt
    assert_eq!(flat[0].name, "a.txt");
    assert_eq!(flat[1].name, "src");
    assert_eq!(flat[2].name, "x.rs");
    assert_eq!(flat[3].name, "b.txt");
}

#[test]
fn flatten_tree_has_children_flag_true_for_dir_with_children() {
    let dir = FileNode::dir(
        1,
        "src",
        "/src",
        vec![FileNode::file(2, "f.rs", "/src/f.rs")],
    );
    let flat = flatten_tree(&[dir]);
    assert!(flat[0].has_children);
}

#[test]
fn flatten_tree_has_children_flag_false_for_empty_dir() {
    let dir = FileNode::dir(1, "empty", "/empty", vec![]);
    let flat = flatten_tree(&[dir]);
    assert!(!flat[0].has_children);
}

#[test]
fn flatten_tree_included_flag_propagated() {
    let mut file = FileNode::file(1, "x.txt", "/x.txt");
    file.included = false;
    let flat = flatten_tree(&[file]);
    assert!(!flat[0].included);
}

#[test]
fn flatten_tree_path_and_id_preserved() {
    let node = FileNode::file(99, "config.toml", "/project/config.toml");
    let flat = flatten_tree(&[node]);
    assert_eq!(flat[0].id, 99);
    assert_eq!(flat[0].path, "/project/config.toml");
}

#[test]
fn flatten_tree_is_dir_flag_preserved() {
    let nodes = vec![
        FileNode::dir(1, "d", "/d", vec![]),
        FileNode::file(2, "f", "/f"),
    ];
    let flat = flatten_tree(&nodes);
    assert!(flat[0].is_dir);
    assert!(!flat[1].is_dir);
}

// ─── SideTab ──────────────────────────────────────────────────────────────────

#[test]
fn side_tab_stats_equals_stats() {
    assert_eq!(SideTab::Stats, SideTab::Stats);
}

#[test]
fn side_tab_conflicts_equals_conflicts() {
    assert_eq!(SideTab::Conflicts, SideTab::Conflicts);
}

#[test]
fn side_tab_stats_not_equal_to_conflicts() {
    assert_ne!(SideTab::Stats, SideTab::Conflicts);
}

#[test]
fn side_tab_clone_equals_original() {
    let tab = SideTab::Stats;
    assert_eq!(tab.clone(), SideTab::Stats);
    let tab2 = SideTab::Conflicts;
    assert_eq!(tab2.clone(), SideTab::Conflicts);
}
