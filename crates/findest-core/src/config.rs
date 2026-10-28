use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::listing::{SortKey, SortOrder};
use crate::paths::expand_tilde;

#[cfg(target_os = "macos")]
pub const DEFAULT_CONFIG: &str = include_str!("default_config.toml");
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_CONFIG: &str = include_str!("default_config_linux.toml");

/// Built-in shortcuts. `secondary` is Command on macOS and Control elsewhere.
/// User `[keybindings]` entries are layered on top.
pub const DEFAULT_KEYBINDINGS: &[(&str, &str)] = &[
    ("up", "select_prev"),
    ("down", "select_next"),
    ("shift-up", "extend_prev"),
    ("shift-down", "extend_next"),
    ("alt-up", "select_first"),
    ("alt-down", "select_last"),
    ("home", "select_first"),
    ("end", "select_last"),
    ("secondary-a", "select_all"),
    ("escape", "clear_selection"),
    ("right", "expand"),
    ("left", "collapse"),
    ("secondary-down", "open"),
    ("secondary-o", "open"),
    ("enter", "rename"),
    ("secondary-up", "go_up"),
    ("secondary-[", "go_back"),
    ("secondary-]", "go_forward"),
    ("secondary-shift-h", "go_home"),
    ("secondary-shift-g", "go_to_folder"),
    ("secondary-shift-n", "new_folder"),
    ("secondary-alt-n", "new_file"),
    ("secondary-d", "duplicate"),
    ("secondary-backspace", "trash"),
    ("backspace", "trash_confirm"),
    ("shift-backspace", "trash_now"),
    ("secondary-alt-backspace", "delete_permanently"),
    ("secondary-shift-backspace", "empty_trash"),
    ("secondary-c", "copy"),
    ("secondary-x", "cut"),
    ("secondary-z", "undo"),
    ("secondary-v", "paste"),
    ("secondary-alt-v", "move_here"),
    ("secondary-alt-c", "copy_path"),
    ("secondary-shift-.", "toggle_hidden"),
    ("secondary-f", "find"),
    ("secondary-i", "toggle_info"),
    ("space", "quick_look"),
    ("secondary-y", "quick_look"),
    ("secondary-shift-m", "toggle_preview_source"),
    ("secondary-g", "find_next"),
    ("secondary-shift-g", "find_previous"),
    ("secondary-1", "view_list"),
    ("secondary-2", "view_tiles"),
    ("secondary-=", "zoom_in"),
    ("secondary--", "zoom_out"),
    ("secondary-r", "refresh"),
    #[cfg(target_os = "macos")]
    ("ctrl-cmd-t", "add_favorite"),
    #[cfg(not(target_os = "macos"))]
    ("secondary-shift-t", "add_favorite"),
    ("secondary-shift-r", "show_recents"),
    ("secondary-alt-r", "reveal_in_finder"),
    ("secondary-,", "open_config"),
    ("secondary-n", "new_window"),
    ("secondary-w", "close_window"),
    ("secondary-e", "open_with:VS Code"),
    ("secondary-alt-t", "open_with:Terminal"),
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub appearance: Appearance,
    pub locations: Locations,
    pub favorites: Vec<Favorite>,
    pub openers: Vec<Opener>,
    pub sources: Vec<crate::sources::Source>,
    pub search: SearchConfig,
    /// User overrides only; see [`Config::keybindings`] for the merged map.
    #[serde(rename = "keybindings")]
    pub keybinding_overrides: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General::default(),
            appearance: Appearance::default(),
            locations: Locations::default(),
            favorites: default_favorites(),
            openers: default_openers(),
            sources: Vec::new(),
            keybinding_overrides: BTreeMap::new(),
            search: SearchConfig::default(),
        }
    }
}

#[cfg(target_os = "macos")]
fn default_favorites() -> Vec<Favorite> {
    let fav = |path: &str| Favorite { path: path.into(), name: None };
    vec![fav("~"), fav("~/Desktop"), fav("~/Documents"), fav("~/Downloads"), fav("/Applications")]
}

#[cfg(not(target_os = "macos"))]
fn default_favorites() -> Vec<Favorite> {
    let fav = |path: &str| Favorite { path: path.into(), name: None };
    vec![fav("~"), fav("~/Desktop"), fav("~/Documents"), fav("~/Downloads")]
}

/// Opens a terminal in the folder `$1`: `$TERMINAL` if set, else the first
/// of the common terminals that is installed. (`x-terminal-emulator` exists
/// only on Debian-based systems, and terminals take a command, not a folder.)
#[cfg(not(target_os = "macos"))]
pub const TERMINAL_SCRIPT: &str = r#"cd "$1" || exit 1; for t in "$TERMINAL" x-terminal-emulator gnome-terminal konsole xfce4-terminal alacritty kitty xterm; do if [ -n "$t" ] && command -v "$t" >/dev/null 2>&1; then exec "$t"; fi; done; exit 1"#;

#[cfg(target_os = "macos")]
fn default_openers() -> Vec<Opener> {
    let opener = |name: &str, app: &str, arg: &str, target| Opener {
        name: name.into(),
        command: vec!["open".into(), "-a".into(), app.into(), arg.into()],
        target,
    };
    vec![
        opener("VS Code", "Visual Studio Code", "{path}", OpenerTarget::Any),
        opener("Terminal", "Terminal", "{dir}", OpenerTarget::Any),
        opener("Preview", "Preview", "{path}", OpenerTarget::Files),
    ]
}

#[cfg(not(target_os = "macos"))]
fn default_openers() -> Vec<Opener> {
    vec![
        Opener {
            name: "VS Code".into(),
            command: vec!["code".into(), "{path}".into()],
            target: OpenerTarget::Any,
        },
        Opener {
            name: "Terminal".into(),
            command: vec!["sh".into(), "-c".into(), TERMINAL_SCRIPT.into(), "sh".into(), "{dir}".into()],
            target: OpenerTarget::Any,
        },
        Opener {
            name: "File Manager".into(),
            command: vec!["xdg-open".into(), "{dir}".into()],
            target: OpenerTarget::Any,
        },
    ]
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Locations {
    pub macintosh_hd: bool,
    /// All mounted external drives.
    pub volumes: bool,
    pub trash: bool,
}

impl Default for Locations {
    fn default() -> Self {
        Self { macintosh_hd: true, volumes: true, trash: true }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// Keep the search index. Off: nothing is scanned or watched, and
    /// search has nothing to search.
    pub indexing: bool,
    /// Folders to index for search ("Everywhere").
    pub roots: Vec<String>,
    /// Paths to skip, or "**/name" to skip a folder name anywhere.
    pub exclude: Vec<String>,
    /// Results shown at most (the count still includes all matches).
    pub max_results: usize,
    /// `content:` skips files larger than this many megabytes.
    pub content_max_mb: u64,
}

impl Default for SearchConfig {
    fn default() -> Self {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect();
        #[cfg(target_os = "macos")]
        {
            Self {
                indexing: true,
                roots: s(&["~", "/Applications", "~/Library/Mobile Documents/com~apple~CloudDocs", "~/Library/CloudStorage"]),
                exclude: s(&["~/Library", "~/.Trash", "**/node_modules", "**/.git"]),
                max_results: 10_000,
                content_max_mb: 20,
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self {
                indexing: true,
                roots: s(&["~"]),
                exclude: s(&["~/.cache", "~/.local/share/Trash", "**/node_modules", "**/.git", "**/target"]),
                max_results: 10_000,
                content_max_mb: 20,
            }
        }
    }
}

impl SearchConfig {
    pub fn root_paths(&self) -> Vec<std::path::PathBuf> {
        self.roots.iter().map(|r| expand_tilde(r)).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub start_path: String,
    /// Reopen the folder (and expanded subfolders) from the last session.
    pub restore_session: bool,
    /// Show the info panel (details of the selection) on the right.
    pub info_panel: bool,
    /// How new windows show a folder: a list (tree) or tiles.
    pub view: ViewMode,
    /// Tile edge in points (tiles view).
    pub tile_size: f32,
    pub show_hidden: bool,
    pub folders_first: bool,
    pub sort_by: SortKey,
    pub sort_ascending: bool,
    pub confirm_trash: bool,
    pub max_recents: usize,
    pub show_recents: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            start_path: "~".into(),
            restore_session: true,
            info_panel: true,
            view: ViewMode::List,
            tile_size: 96.0,
            show_hidden: false,
            folders_first: true,
            sort_by: SortKey::Name,
            sort_ascending: true,
            confirm_trash: false,
            max_recents: 50,
            show_recents: true,
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
pub enum ViewMode {
    /// Rows with columns; folders expand in place.
    #[default]
    List,
    /// Icons and image thumbnails in a grid.
    Tiles,
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
    /// Minimum width of the left sidebar in points (default 120).
    pub sidebar_min_width: f32,
    /// Maximum width of the left sidebar in points (default 400).
    pub sidebar_max_width: f32,
    /// Minimum width of the right info panel in points (default 180).
    pub info_panel_min_width: f32,
    /// Maximum width of the right info panel in points (default 500).
    pub info_panel_max_width: f32,
}

impl Appearance {
    pub fn clamp_sidebar(&self, width: f32) -> f32 {
        clamp_width(width, self.sidebar_min_width, self.sidebar_max_width)
    }

    pub fn clamp_info_panel(&self, width: f32) -> f32 {
        clamp_width(width, self.info_panel_min_width, self.info_panel_max_width)
    }
}

/// Like `f32::clamp`, but a config with min > max (or NaN) can't panic:
/// max wins, and a bad width falls back to min.
fn clamp_width(width: f32, min: f32, max: f32) -> f32 {
    let max = if max.is_finite() { max.max(0.0) } else { f32::MAX };
    let min = if min.is_finite() { min.clamp(0.0, max) } else { 0.0 };
    if width.is_finite() { width.clamp(min, max) } else { min }
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            font_size: 13.0,
            row_height: 26.0,
            font_family: String::new(),
            colors: BTreeMap::new(),
            sidebar_min_width: 120.0,
            sidebar_max_width: 400.0,
            info_panel_min_width: 180.0,
            info_panel_max_width: 500.0,
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
    /// Treats `cmd-` and `secondary-` as equivalent so existing `cmd-*` user
    /// overrides continue to match `secondary-*` defaults.
    pub fn keybindings(&self) -> BTreeMap<String, String> {
        let mut map: BTreeMap<String, String> =
            DEFAULT_KEYBINDINGS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        for (key, command) in &self.keybinding_overrides {
            if let Some(alt) = alternate_secondary_cmd(key) {
                map.remove(&alt);
            }
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

    /// Sets `indexing` under `[search]` in the file at `path`, preserving
    /// the user's comments and formatting elsewhere.
    pub fn save_search_indexing(path: &Path, on: bool) -> Result<()> {
        let text = fs::read_to_string(path).unwrap_or_default();
        let mut doc: toml_edit::DocumentMut = text.parse().with_context(|| format!("parsing {}", path.display()))?;
        doc["search"]["indexing"] = toml_edit::value(on);
        fs::write(path, doc.to_string()).with_context(|| format!("writing {}", path.display()))
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

fn alternate_secondary_cmd(key: &str) -> Option<String> {
    let parts: Vec<&str> = key.split('-').collect();
    let (last, mods) = match parts.split_last()? {
        // "cmd--" style bindings for the minus key end with an empty part.
        (&"", rest) if !rest.is_empty() => ("-", &rest[..rest.len() - 1]),
        (last, rest) => (*last, rest),
    };
    if !mods.iter().any(|&m| m == "cmd" || m == "secondary") {
        return None;
    }
    let mut out: Vec<&str> = mods
        .iter()
        .map(|&m| match m {
            "cmd" => "secondary",
            "secondary" => "cmd",
            other => other,
        })
        .collect();
    out.push(last);
    Some(out.join("-"))
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
    fn save_search_indexing_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, DEFAULT_CONFIG).unwrap();
        Config::save_search_indexing(&path, false).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(!cfg.search.indexing);
        assert_eq!(cfg.search.roots, SearchConfig::default().roots);
        assert!(fs::read_to_string(&path).unwrap().contains("# Folders searched by"), "comments kept");

        let bare = dir.path().join("bare.toml");
        Config::save_search_indexing(&bare, false).unwrap();
        assert!(!Config::load(&bare).unwrap().search.indexing, "creates the section");
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
    fn empty_favorites_hides_section() {
        // Absent [[favorites]] → serde default
        let absent: Config = toml::from_str("[general]\nstart_path = \"~\"\n").unwrap();
        assert_eq!(absent.favorites.len(), default_favorites().len());
        // Explicit empty array → 0 items, so sidebar can hide the group
        let empty: Config = toml::from_str("favorites = []\n").unwrap();
        assert_eq!(empty.favorites.len(), 0);
    }

    #[test]
    fn panel_widths_clamp_without_panicking() {
        let mut a = Appearance::default();
        assert_eq!(a.clamp_sidebar(1000.0), 400.0);
        a.sidebar_min_width = 500.0;
        assert_eq!(a.clamp_sidebar(200.0), 400.0);
        a.sidebar_max_width = f32::NAN;
        assert_eq!(a.clamp_sidebar(f32::NAN), 500.0);
    }

    #[test]
    fn locations_config_parses() {
        let cfg: Config = toml::from_str("[locations]\nmacintosh_hd = false\n").unwrap();
        assert!(!cfg.locations.macintosh_hd);
        assert!(cfg.locations.volumes);
        assert!(cfg.locations.trash);
    }

    #[test]
    fn locations_after_favorites_parses() {
        let toml = "
[general]
start_path = \"~\"

[[favorites]]
path = \"~\"

[locations]
macintosh_hd = false
volumes = true
trash = false

[[openers]]
name = \"VS Code\"
command = [\"open\", \"-a\", \"Visual Studio Code\", \"{path}\"]
for = \"any\"
";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(!cfg.locations.macintosh_hd);
        assert!(cfg.locations.volumes);
        assert!(!cfg.locations.trash);
    }

    #[test]
    fn keybinding_overrides_merge() {
        let cfg: Config = toml::from_str(
            "[keybindings]\n\"secondary-d\" = \"none\"\n\"enter\" = \"open\"\n\"secondary-j\" = \"open_with:VS Code\"\n",
        )
        .unwrap();
        let keys = cfg.keybindings();
        assert_eq!(keys.get("enter").unwrap(), "open");
        assert!(!keys.contains_key("secondary-d"));
        assert_eq!(keys.get("secondary-j").unwrap(), "open_with:VS Code");
        assert_eq!(keys.get("secondary-up").unwrap(), "go_up");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn terminal_opener_starts_in_the_folder() {
        use std::os::unix::fs::PermissionsExt;
        let from_file: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        let terminal = |c: &Config| c.openers.iter().find(|o| o.name == "Terminal").unwrap().command.clone();
        assert_eq!(terminal(&from_file), terminal(&Config::default()), "the config file and the built-in default agree");

        // A stand-in terminal that records where it was started.
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-term");
        let out = tmp.path().join("cwd.txt");
        fs::write(&fake, format!("#!/bin/sh\npwd > '{}'\n", out.display())).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let work = tmp.path().join("work dir");
        fs::create_dir(&work).unwrap();

        let argv = crate::launch::expand_command(&terminal(&from_file), &work);
        let status = std::process::Command::new(&argv[0]).args(&argv[1..]).env("TERMINAL", &fake).status().unwrap();
        assert!(status.success());
        assert_eq!(fs::read_to_string(&out).unwrap().trim(), work.to_str().unwrap());

        // With no terminal available it fails instead of hanging.
        let none = std::process::Command::new(&argv[0]).args(&argv[1..]).env("TERMINAL", "").env("PATH", "/nonexistent").status();
        assert!(none.map(|s| !s.success()).unwrap_or(true));
    }

    #[test]
    fn old_style_cmd_overrides_merge() {
        let cfg: Config = toml::from_str(
            "[keybindings]\n\"cmd-d\" = \"none\"\n\"cmd-o\" = \"rename\"\n\"cmd--\" = \"none\"\n\"cmd-j\" = \"open_with:VS Code\"\n",
        )
        .unwrap();
        let keys = cfg.keybindings();
        assert!(!keys.contains_key("cmd-d") && !keys.contains_key("secondary-d"), "\"cmd-d\" = \"none\" removes secondary-d");
        assert!(!keys.contains_key("cmd--") && !keys.contains_key("secondary--"), "\"cmd--\" = \"none\" removes secondary--");
        assert!(!keys.contains_key("secondary-o"), "\"cmd-o\" override replaces secondary-o");
        assert_eq!(keys.get("cmd-o").unwrap(), "rename");
        assert_eq!(keys.get("cmd-j").unwrap(), "open_with:VS Code");
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
