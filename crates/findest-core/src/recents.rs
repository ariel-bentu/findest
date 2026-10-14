use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Most-recently-opened files, newest first, persisted as JSON.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Recents {
    pub items: Vec<PathBuf>,
}

impl Recents {
    /// `~/Library/Application Support/findest/recents.json`.
    pub fn path() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| crate::paths::home_dir().join("Library/Application Support"))
            .join("findest/recents.json")
    }

    /// Recently visited folders: `~/Library/Application Support/findest/locations.json`.
    pub fn locations_path() -> PathBuf {
        Self::path().with_file_name("locations.json")
    }

    /// Missing or unreadable files yield an empty list rather than an error.
    pub fn load(path: &Path) -> Recents {
        fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, serde_json::to_vec_pretty(self)?).with_context(|| format!("writing {}", path.display()))
    }

    pub fn push(&mut self, path: PathBuf, max: usize) {
        self.items.retain(|p| *p != path);
        self.items.insert(0, path);
        self.items.truncate(max);
    }

    /// Recents that still exist on disk.
    pub fn existing(&self) -> impl Iterator<Item = &PathBuf> {
        self.items.iter().filter(|p| fs::symlink_metadata(p).is_ok())
    }

    pub fn remove(&mut self, path: &Path) {
        self.items.retain(|p| p != path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_dedupes_and_caps() {
        let mut r = Recents::default();
        for p in ["a", "b", "c", "a"] {
            r.push(p.into(), 3);
        }
        assert_eq!(r.items, vec![PathBuf::from("a"), "c".into(), "b".into()]);
        r.push("d".into(), 3);
        assert_eq!(r.items.len(), 3);
        assert_eq!(r.items[0], PathBuf::from("d"));
    }

    #[test]
    fn round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub/recents.json");
        let mut r = Recents::default();
        r.push("/x".into(), 10);
        r.save(&path).unwrap();
        assert_eq!(Recents::load(&path), r);
        assert_eq!(Recents::load(&tmp.path().join("missing.json")), Recents::default());
    }
}
