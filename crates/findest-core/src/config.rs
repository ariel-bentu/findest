use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::listing::{SortKey, SortOrder};
use crate::paths::expand_tilde;

pub const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

/// Built-in shortcuts. User `[keybindings]` entries are layered on top.
pub const DEFAULT_KEYBINDINGS: &[(&str, &str)] = &[
    ("up", "select_prev"),
    ("down", "select_next"),
    ("shift-up", "extend_prev"),
    ("shift-down", "extend_next"),
    ("alt-up", "select_first"),
    ("alt-down", "select_last"),
    ("home", "select_first"),
    ("end", "select_last"),
    ("cmd-a", "select_all"),
    ("escape", "clear_selection"),
    ("cmd-down", "open"),
    ("cmd-o", "open"),
    ("enter", "rename"),
    ("cmd-up", "go_up"),
    ("cmd-[", "go_back"),
    ("cmd-]", "go_forward"),
    ("cmd-shift-h", "go_home"),
    ("cmd-shift-g", "go_to_folder"),
    ("cmd-shift-n", "new_folder"),
    ("cmd-alt-n", "new_file"),
    ("cmd-d", "duplicate"),
    ("cmd-backspace", "trash"),
    ("backspace", "trash_confirm"),
    ("shift-backspace", "trash_now"),
    ("cmd-alt-backspace", "delete_permanently"),
    ("cmd-c", "copy"),
    ("cmd-v", "paste"),
    ("cmd-alt-v", "move_here"),
    ("cmd-alt-c", "copy_path"),
    ("cmd-shift-.", "toggle_hidden"),
    ("cmd-r", "refresh"),
    ("ctrl-cmd-t", "add_favorite"),
    ("cmd-shift-r", "show_recents"),
    ("cmd-alt-r", "reveal_in_finder"),
    ("cmd-,", "open_config"),
    ("cmd-e", "open_with:VS Code"),
    ("cmd-alt-t", "open_with:Terminal"),
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub appearance: Appearance,
    pub favorites: Vec<Favorite>,
    pub openers: Vec<Opener>,
    /// User overrides only; see [`Config::keybindings`] for the merged map.
    #[serde(rename = "keybindings")]
    pub keybinding_overrides: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        let fav = |path: &str| Favorite { path: path.into(), name: None };
        let opener = |name: &str, app: &str, arg: &str, target| Opener {
            name: name.into(),
            command: vec!["open".into(), "-a".into(), app.into(), arg.into()],
            target,
        };
        Self {
            general: General::default(),
            appearance: Appearance::default(),
            favorites: vec![fav("~"), fav("~/Desktop"), fav("~/Documents"), fav("~/Downloads"), fav("/Applications")],
            openers: vec![
                opener("VS Code", "Visual Studio Code", "{path}", OpenerTarget::Any),
                opener("Terminal", "Terminal", "{dir}", OpenerTarget::Any),
                opener("Preview", "Preview", "{path}", OpenerTarget::Files),
            ],
            keybinding_overrides: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub start_path: String,
    pub show_hidden: bool,
    pub folders_first: bool,
    pub sort_by: SortKey,
    pub sort_ascending: bool,
    pub confirm_trash: bool,
    pub max_recents: usize,
}

impl Default for General {
    fn default() -> Self {
        Self {
            start_path: "~".into(),
            show_hidden: false,
            folders_first: true,
            sort_by: SortKey::Name,
            sort_ascending: true,
            confirm_trash: false,
            max_recents: 50,
        }
    }
}

impl General {
    pub fn sort_order(&self) -> SortOrder {
        SortOrder { key: self.sort_by, ascending: self.sort_ascending, folders_first: self.folders_first }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    Dark,
    Light,
    #[default]
    System,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: ThemeMode,
    pub font_size: f32,
    pub row_height: f32,
    pub font_family: String,
    pub colors: BTreeMap<String, String>,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            font_size: 13.0,
            row_height: 26.0,
            font_family: String::new(),
            colors: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Favorite {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Favorite {
    pub fn resolved_path(&self) -> PathBuf {
        expand_tilde(&self.path)
    }

    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| crate::paths::display_name(&self.resolved_path()))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenerTarget {
    #[default]
    Any,
    Files,
    Folders,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Opener {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default, rename = "for")]
    pub target: OpenerTarget,
}

impl Opener {
    pub fn applies_to(&self, is_dir: bool) -> bool {
        match self.target {
            OpenerTarget::Any => true,
            OpenerTarget::Files => !is_dir,
            OpenerTarget::Folders => is_dir,
        }
    }
}

impl Config {
    /// `$FINDEST_CONFIG`, or `~/.config/findest/config.toml`.
    pub fn path() -> PathBuf {
        std::env::var_os("FINDEST_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::paths::home_dir().join(".config/findest/config.toml"))
    }

    /// Loads the config, writing the commented default file first if none exists.
    pub fn load_or_init(path: &Path) -> Result<Config> {
        if !path.exists() {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::write(path, DEFAULT_CONFIG).with_context(|| format!("writing {}", path.display()))?;
        }
        Self::load(path)
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Default keybindings merged with user overrides; "none" removes a binding.
    pub fn keybindings(&self) -> BTreeMap<String, String> {
        let mut map: BTreeMap<String, String> =
            DEFAULT_KEYBINDINGS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        for (key, command) in &self.keybinding_overrides {
            if command == "none" {
                map.remove(key);
            } else {
                map.insert(key.clone(), command.clone());
            }
        }
        map
    }

    pub fn start_path(&self) -> PathBuf {
        expand_tilde(&self.general.start_path)
    }

    /// Rewrites only the `[[favorites]]` section of the file at `path`,
    /// preserving the user's comments and formatting elsewhere.
    pub fn save_favorites(path: &Path, favorites: &[Favorite]) -> Result<()> {
        let text = fs::read_to_string(path).unwrap_or_default();
        let mut doc: toml_edit::DocumentMut = text.parse().with_context(|| format!("parsing {}", path.display()))?;
        let mut array = toml_edit::ArrayOfTables::new();
        for fav in favorites {
            let mut table = toml_edit::Table::new();
            table["path"] = toml_edit::value(fav.path.clone());
            if let Some(name) = &fav.name {
                table["name"] = toml_edit::value(name.clone());
            }
            array.push(table);
        }
        doc.insert("favorites", toml_edit::Item::ArrayOfTables(array));
        fs::write(path, doc.to_string()).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_template_matches_default() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn partial_config_uses_defaults() {
        let cfg: Config = toml::from_str("[general]\nshow_hidden = true\n").unwrap();
        assert!(cfg.general.show_hidden);
        assert!(cfg.general.folders_first);
        // A missing top-level section falls back to the full default config.
        assert_eq!(cfg.openers.len(), 3);
    }

    #[test]
    fn keybinding_overrides_merge() {
        let cfg: Config = toml::from_str(
            "[keybindings]\n\"cmd-d\" = \"none\"\n\"enter\" = \"open\"\n\"cmd-j\" = \"open_with:VS Code\"\n",
        )
        .unwrap();
        let keys = cfg.keybindings();
        assert_eq!(keys.get("enter").unwrap(), "open");
        assert!(!keys.contains_key("cmd-d"));
        assert_eq!(keys.get("cmd-j").unwrap(), "open_with:VS Code");
        assert_eq!(keys.get("cmd-up").unwrap(), "go_up");
    }

    #[test]
    fn save_favorites_preserves_rest_of_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        Config::load_or_init(&path).unwrap();
        let favs = vec![Favorite { path: "~/code".into(), name: Some("Code".into()) }];
        Config::save_favorites(&path, &favs).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Findest configuration."));
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.favorites, favs);
        assert_eq!(cfg.openers.len(), 3);
    }
}
