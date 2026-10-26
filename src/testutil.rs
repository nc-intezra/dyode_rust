//! Small helpers for the tests (no external crates).

use std::path::{Path, PathBuf};

/// A temporary directory removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let base =
            std::env::var_os("DYODE_TEST_TMP").map_or_else(std::env::temp_dir, PathBuf::from);
        let p = base.join(format!(
            "dyode-test-{:08x}{:08x}",
            crate::auth::random_u32(),
            crate::auth::random_u32()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Default for TempDir {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
