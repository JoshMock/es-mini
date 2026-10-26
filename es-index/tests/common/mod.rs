//! Helpers shared by the disk tests.

use std::path::PathBuf;

/// A fresh `esm-{name}-{pid}` directory in the temp dir, removed with everything in it when
/// dropped, even if the test panics. Derefs to `sub` inside it, or to itself without one.
pub struct Scratch {
    root: PathBuf,
    path: PathBuf,
}

impl Scratch {
    pub fn new(name: &str, sub: Option<&str>) -> Self {
        let root = std::env::temp_dir().join(format!("esm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = sub.map_or_else(|| root.clone(), |s| root.join(s));
        Self { root, path }
    }
}

impl std::ops::Deref for Scratch {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
