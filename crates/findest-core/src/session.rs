//! What was open when Findest last ran, restored on the next launch.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::ViewMode;

/// One browser window's state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSession {
    /// The folder that was open; `None` with `recents` means the Recents view.
    pub folder: Option<PathBuf>,
    pub recents: bool,
    /// Folders expanded in place (tree view).
    pub expanded: Vec<PathBuf>,
    /// List or tiles; `None` defers to the config.
    pub view: Option<ViewMode>,
    pub tile_size: Option<f32>,
    pub columns: Option<ColumnWidths>,
    pub sidebar_width: Option<f32>,
    pub info_panel_width: Option<f32>,
}

/// List view column widths in points. The list scrolls sideways when they
/// add up to more than the window is wide.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColumnWidths {
    pub name: f32,
    pub modified: f32,
    pub size: f32,
    /// Kind, or Where in Recents and search results.
    pub kind: f32,
    /// A source's columns, by attribute key.
    pub attributes: std::collections::BTreeMap<String, f32>,
}

impl ColumnWidths {
    pub const MIN: f32 = 40.0;
    pub const MAX: f32 = 800.0;
    pub const ATTRIBUTE: f32 = 120.0;
}

impl Default for ColumnWidths {
    fn default() -> Self {
        ColumnWidths { name: 300.0, modified: 170.0, size: 90.0, kind: 130.0, attributes: Default::default() }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// Open windows, front to back as last saved.
    pub windows: Vec<WindowSession>,
    /// Whether the info panel was open; `None` defers to the config.
    pub info_panel: Option<bool>,

    // Single-window format from earlier versions; migrated on load.
    #[serde(skip_serializing)]
    folder: Option<PathBuf>,
    #[serde(skip_serializing)]
    recents: bool,
    #[serde(skip_serializing)]
    expanded: Vec<PathBuf>,
}

impl Session {
    pub fn new(windows: Vec<WindowSession>, info_panel: Option<bool>) -> Session {
        Session { windows, info_panel, ..Default::default() }
    }

    /// `~/Library/Application Support/findest/session.json`.
    pub fn path() -> PathBuf {
        crate::recents::Recents::path().with_file_name("session.json")
    }

    /// Missing or unreadable files yield an empty session. Folders that no
    /// longer exist are dropped, and so are windows left with nothing to show.
    pub fn load(path: &Path) -> Session {
        let mut session: Session =
            fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        if session.windows.is_empty() && (session.folder.is_some() || session.recents) {
            session.windows.push(WindowSession {
                folder: session.folder.take(),
                recents: session.recents,
                expanded: std::mem::take(&mut session.expanded),
                ..Default::default()
            });
        }
        for window in &mut session.windows {
            window.folder = window.folder.take().filter(|p| p.is_dir());
            window.expanded.retain(|p| p.is_dir());
        }
        session.windows.retain(|w| w.folder.is_some() || w.recents);
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
        let here = tmp.path().to_path_buf();
        let session = Session::new(
            vec![
                WindowSession { folder: Some(here.clone()), recents: false, expanded: vec![here.clone(), here.join("gone")], ..Default::default() },
                WindowSession { folder: Some(here.join("gone")), recents: false, expanded: vec![], ..Default::default() },
                WindowSession { folder: None, recents: true, expanded: vec![], ..Default::default() },
            ],
            Some(false),
        );
        session.save(&path).unwrap();
        let loaded = Session::load(&path);
        assert_eq!(loaded.windows.len(), 2, "window on a deleted folder is dropped");
        assert_eq!(loaded.windows[0].expanded, vec![here.clone()]);
        assert!(loaded.windows[1].recents);
        assert_eq!(loaded.info_panel, Some(false));
        assert_eq!(Session::load(&tmp.path().join("missing.json")), Session::default());
    }

    #[test]
    fn migrates_single_window_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.json");
        let old = format!(r#"{{"folder": "{}", "recents": false, "expanded": [], "info_panel": true}}"#, tmp.path().display());
        fs::write(&path, old).unwrap();
        let loaded = Session::load(&path);
        assert_eq!(loaded.windows.len(), 1);
        assert_eq!(loaded.windows[0].folder.as_deref(), Some(tmp.path()));
        assert_eq!(loaded.info_panel, Some(true));
    }
}
