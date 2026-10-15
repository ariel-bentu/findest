//! What was open when Findest last ran, restored on the next launch.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// The folder that was open; `None` with `recents` means the Recents view.
    pub folder: Option<PathBuf>,
    pub recents: bool,
    /// Folders expanded in place (tree view).
    pub expanded: Vec<PathBuf>,
    /// Whether the info panel was open; `None` defers to the config.
    pub info_panel: Option<bool>,
}

impl Session {
    /// `~/Library/Application Support/findest/session.json`.
    pub fn path() -> PathBuf {
        crate::recents::Recents::path().with_file_name("session.json")
    }

    /// Missing or unreadable files yield an empty session. Folders that no
    /// longer exist are dropped.
    pub fn load(path: &Path) -> Session {
        let mut session: Session =
            fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        session.folder = session.folder.filter(|p| p.is_dir());
        session.expanded.retain(|p| p.is_dir());
        session
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, serde_json::to_vec_pretty(self)?).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_drops_missing_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.json");
        let session = Session {
            folder: Some(tmp.path().to_path_buf()),
            recents: false,
            expanded: vec![tmp.path().to_path_buf(), tmp.path().join("gone")],
            info_panel: Some(false),
        };
        session.save(&path).unwrap();
        let loaded = Session::load(&path);
        assert_eq!(loaded.folder, session.folder);
        assert_eq!(loaded.expanded, vec![tmp.path().to_path_buf()]);
        assert_eq!(loaded.info_panel, Some(false));
        assert_eq!(Session::load(&tmp.path().join("missing.json")), Session::default());
    }
}
