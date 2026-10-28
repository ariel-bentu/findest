//! Every user-facing operation is a [`Command`], addressable by a string name
//! so keybindings, menus and context menus can all be driven from config.

use std::str::FromStr;

use gpui::{Action, KeyBinding, KeyBindingContextPredicate};
use schemars::JsonSchema;
use serde::Deserialize;

/// The single gpui action that carries a command name. Keybindings from
/// the config file are bound to `Run { command }`.
#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Action)]
#[action(namespace = findest)]
pub struct Run {
    pub command: String,
}

impl Run {
    pub fn new(command: impl Into<String>) -> Self {
        Self { command: command.into() }
    }
}

gpui::actions!(findest, [Quit]);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    SelectPrev,
    SelectNext,
    ExtendPrev,
    ExtendNext,
    SelectFirst,
    SelectLast,
    SelectAll,
    ClearSelection,
    /// Expand selected folders in place (tree view).
    Expand,
    /// Collapse selected folders, or select the parent row.
    Collapse,
    Open,
    Rename,
    GoUp,
    GoBack,
    GoForward,
    GoHome,
    GoToFolder,
    NewFolder,
    NewFile,
    Duplicate,
    /// Zip the selection next to it.
    Compress,
    /// Unzip selected .zip files next to them.
    Extract,
    /// Move to Trash, asking first only if `confirm_trash` is set.
    Trash,
    /// Move to Trash, always asking first.
    TrashConfirm,
    /// Move to Trash without asking.
    TrashNow,
    DeletePermanently,
    EmptyTrash,
    /// Reverse the last paste, drop or duplicate.
    Undo,
    Copy,
    /// Like Copy, but Paste then moves the items.
    Cut,
    /// Forget what was copied or cut.
    ClearClipboard,
    Paste,
    MoveHere,
    CopyPath,
    ToggleHidden,
    /// Folders before files, or mixed in with them by the sort.
    ToggleFoldersFirst,
    ToggleInfo,
    /// Preview the selection in the Quick Look panel, or close it.
    QuickLook,
    /// In the text preview of a Markdown file: formatted text or source.
    TogglePreviewSource,
    /// In the text preview's find bar: the next / previous match.
    FindNext,
    FindPrevious,
    ViewList,
    ViewTiles,
    /// Bigger / smaller tiles (tiles view).
    ZoomIn,
    ZoomOut,
    Find,
    RebuildIndex,
    /// Hold off indexing until resumed (or quit) / for an hour.
    PauseIndexing,
    PauseIndexingHour,
    ResumeIndexing,
    /// Turn search indexing on or off for good (saved in the config).
    ToggleIndexing,
    /// Open folders and "Show in Finder" in Findest, or put Finder back.
    UseFindestAsDefault,
    UseFinderAsDefault,
    Refresh,
    AddFavorite,
    ShowRecents,
    RevealInFinder,
    /// Opens the selection's folder with it selected (from search results, Recents, a subfolder).
    RevealInFindest,
    OpenConfig,
    NewWindow,
    CloseWindow,
    /// Reset sidebar and info panel widths to their defaults.
    ResetLayout,
    OpenWith(String),
}

impl FromStr for Command {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        use Command::*;
        if let Some(name) = s.strip_prefix("open_with:") {
            return Ok(OpenWith(name.to_string()));
        }
        Ok(match s {
            "select_prev" => SelectPrev,
            "select_next" => SelectNext,
            "extend_prev" => ExtendPrev,
            "extend_next" => ExtendNext,
            "select_first" => SelectFirst,
            "select_last" => SelectLast,
            "select_all" => SelectAll,
            "clear_selection" => ClearSelection,
            "expand" => Expand,
            "collapse" => Collapse,
            "open" => Open,
            "rename" => Rename,
            "go_up" => GoUp,
            "go_back" => GoBack,
            "go_forward" => GoForward,
            "go_home" => GoHome,
            "go_to_folder" => GoToFolder,
            "new_folder" => NewFolder,
            "new_file" => NewFile,
            "duplicate" => Duplicate,
            "compress" => Compress,
            "extract" => Extract,
            "trash" => Trash,
            "trash_confirm" => TrashConfirm,
            "trash_now" => TrashNow,
            "empty_trash" => EmptyTrash,
            "delete_permanently" => DeletePermanently,
            "copy" => Copy,
            "undo" => Undo,
            "cut" => Cut,
            "clear_clipboard" => ClearClipboard,
            "paste" => Paste,
            "move_here" => MoveHere,
            "copy_path" => CopyPath,
            "toggle_hidden" => ToggleHidden,
            "toggle_folders_first" => ToggleFoldersFirst,
            "toggle_info" => ToggleInfo,
            "quick_look" => QuickLook,
            "toggle_preview_source" => TogglePreviewSource,
            "find_next" => FindNext,
            "find_previous" => FindPrevious,
            "view_list" => ViewList,
            "view_tiles" => ViewTiles,
            "zoom_in" => ZoomIn,
            "zoom_out" => ZoomOut,
            "find" => Find,
            "rebuild_index" => RebuildIndex,
            "pause_indexing" => PauseIndexing,
            "pause_indexing_hour" => PauseIndexingHour,
            "resume_indexing" => ResumeIndexing,
            "toggle_indexing" => ToggleIndexing,
            "use_findest_as_default" => UseFindestAsDefault,
            "use_finder_as_default" => UseFinderAsDefault,
            "refresh" => Refresh,
            "add_favorite" => AddFavorite,
            "show_recents" => ShowRecents,
            "reveal_in_finder" => RevealInFinder,
            "reveal_in_findest" => RevealInFindest,
            "open_config" => OpenConfig,
            "new_window" => NewWindow,
            "close_window" => CloseWindow,
            "reset_layout" => ResetLayout,
            other => return Err(format!("unknown command \"{other}\"")),
        })
    }
}

pub const BROWSER_CONTEXT: &str = "Browser";

/// Builds gpui keybindings from the merged config map. Invalid entries are
/// skipped and described in the returned error list.
pub fn keybindings(map: &std::collections::BTreeMap<String, String>) -> (Vec<KeyBinding>, Vec<String>) {
    let mut bindings = Vec::new();
    let mut errors = Vec::new();
    let context = KeyBindingContextPredicate::parse(BROWSER_CONTEXT).ok().map(std::rc::Rc::new);
    for (keys, command) in map {
        if let Err(e) = command.parse::<Command>() {
            errors.push(format!("keybinding \"{keys}\": {e}"));
            continue;
        }
        match KeyBinding::load(
            keys,
            Box::new(Run::new(command.clone())),
            context.clone(),
            false,
            None,
            &gpui::DummyKeyboardMapper,
        ) {
            Ok(binding) => bindings.push(binding),
            Err(e) => errors.push(format!("keybinding \"{keys}\": {e}")),
        }
    }
    (bindings, errors)
}

/// The first key bound to `command`, formatted for display in menus (e.g. "⌘⇧N" or "Ctrl+Shift+N").
pub fn shortcut_label(map: &std::collections::BTreeMap<String, String>, command: &str) -> Option<String> {
    let keys = map.iter().filter(|(_, c)| *c == command).map(|(k, _)| k).min_by_key(|k| k.len())?;
    let mut out = String::new();
    let parts: Vec<&str> = keys.split('-').collect();
    let (mods, key) = match parts.split_last() {
        // "cmd--" style bindings for the minus key end with an empty part.
        Some((&"", rest)) => (&rest[..rest.len().saturating_sub(1)], "-"),
        Some((last, rest)) => (rest, *last),
        None => return None,
    };

    #[cfg(target_os = "macos")]
    {
        for m in ["ctrl", "alt", "shift", "cmd", "secondary"] {
            if mods.contains(&m) {
                out.push_str(match m {
                    "ctrl" => "⌃",
                    "alt" => "⌥",
                    "shift" => "⇧",
                    _ => "⌘",
                });
            }
        }
        out.push_str(&match key {
            "backspace" => "⌫".to_string(),
            "enter" => "↩".to_string(),
            "up" => "↑".to_string(),
            "down" => "↓".to_string(),
            "left" => "←".to_string(),
            "right" => "→".to_string(),
            "escape" => "⎋".to_string(),
            "space" => "Space".to_string(),
            k => k.to_uppercase(),
        });
    }

    #[cfg(not(target_os = "macos"))]
    {
        for m in ["ctrl", "secondary", "alt", "shift", "cmd"] {
            if mods.contains(&m) {
                out.push_str(match m {
                    "ctrl" | "secondary" => "Ctrl+",
                    "alt" => "Alt+",
                    "shift" => "Shift+",
                    _ => "Super+",
                });
            }
        }
        out.push_str(&match key {
            "backspace" => "Backspace".to_string(),
            "enter" => "Enter".to_string(),
            "up" => "Up".to_string(),
            "down" => "Down".to_string(),
            "left" => "Left".to_string(),
            "right" => "Right".to_string(),
            "escape" => "Esc".to_string(),
            "space" => "Space".to_string(),
            k => {
                let mut chars = k.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        });
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn all_default_keybindings_are_valid() {
        let map = findest_core::config::Config::default().keybindings();
        let (bindings, errors) = keybindings(&map);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(bindings.len(), map.len());
    }

    #[test]
    fn reports_bad_bindings() {
        let map = BTreeMap::from([("cmd-x".to_string(), "explode".to_string())]);
        let (_, errors) = keybindings(&map);
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn labels() {
        let map = findest_core::config::Config::default().keybindings();
        #[cfg(target_os = "macos")]
        {
            assert_eq!(shortcut_label(&map, "new_folder").unwrap(), "⇧⌘N");
            assert_eq!(shortcut_label(&map, "trash").unwrap(), "⌘⌫");
            assert_eq!(shortcut_label(&map, "open").unwrap(), "⌘O");
            assert_eq!(shortcut_label(&map, "zoom_out").unwrap(), "⌘-");
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(shortcut_label(&map, "new_folder").unwrap(), "Ctrl+Shift+N");
            // The shortest binding is shown: Delete also trashes on Linux.
            assert_eq!(shortcut_label(&map, "trash").unwrap(), "Delete");
            assert_eq!(shortcut_label(&map, "rename").unwrap(), "F2");
            assert_eq!(shortcut_label(&map, "open").unwrap(), "Ctrl+O");
            assert_eq!(shortcut_label(&map, "zoom_out").unwrap(), "Ctrl+-");
        }
    }
}
