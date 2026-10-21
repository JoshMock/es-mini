//! Crash safety of es_index::disk::save: a save either fully replaces the data directory or
//! leaves the previous snapshot intact.

use es_index::{Indices, disk};
use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("esm-save-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn one_doc(source: serde_json::Value) -> Indices {
    let mut indices = Indices::new();
    indices.entry("idx".into()).or_default().put("1", source);
    indices
}

fn source_of(dir: &std::path::Path) -> serde_json::Value {
    disk::open(dir).unwrap()["idx"]
        .get("1")
        .unwrap()
        .source
        .clone()
}

#[test]
fn failed_save_keeps_the_previous_snapshot() {
    let root = scratch("fail");
    let dir = root.join("data");
    disk::save(&dir, &one_doc(json!({"v": 1}))).unwrap();
    // A read-only parent makes the new snapshot impossible to write.
    fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();
    let result = disk::save(&dir, &one_doc(json!({"v": 2})));
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.is_err());
    assert_eq!(source_of(&dir), json!({"v": 1}));
}

#[test]
fn recover_restores_a_swap_interrupted_between_renames() {
    let root = scratch("recover");
    let dir = root.join("data");
    disk::save(&dir, &one_doc(json!({"v": 1}))).unwrap();
    // Simulate a crash after the old snapshot was moved aside, before the new one moved in.
    fs::rename(&dir, root.join("data.esmini-old")).unwrap();
    fs::create_dir_all(root.join("data.esmini-new/half-written")).unwrap();
    disk::recover(&dir).unwrap();
    assert_eq!(source_of(&dir), json!({"v": 1}));
    assert!(!root.join("data.esmini-new").exists());
}

#[test]
fn save_keeps_other_top_level_entries() {
    let root = scratch("extras");
    let dir = root.join("data");
    fs::create_dir_all(dir.join("nodes")).unwrap();
    fs::write(dir.join("node.lock"), b"").unwrap();
    disk::save(&dir, &one_doc(json!({"v": 1}))).unwrap();
    assert!(dir.join("nodes").is_dir() && dir.join("node.lock").is_file());
}
