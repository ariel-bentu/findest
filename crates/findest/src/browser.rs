//! The main window: state, navigation, selection and command handling.
//! Rendering lives in `render.rs`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use findest_core::config::{Config, Favorite};
use findest_core::listing::{self, Entry};
use findest_core::locations::{self, Place};
use findest_core::paths::{contract_tilde, display_name, expand_tilde, home_dir};
use findest_core::recents::Recents;
use findest_core::{launch, ops};
use futures::StreamExt;
use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, FocusHandle, Focusable, Pixels, Point,
    PromptLevel, ScrollStrategy, Subscription, Task, UniformListScrollHandle, Window,
};

use crate::command::{self, Command};
use crate::icons::{IconCache, IconKey};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;
use crate::watcher::FsWatcher;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Location {
    Dir(PathBuf),
    Recents,
}

pub enum EditKind {
    Rename(PathBuf),
    GoToFolder,
}

pub struct Edit {
    pub kind: EditKind,
    pub input: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

pub struct Status {
    pub message: String,
    pub is_error: bool,
}

pub struct ContextMenu {
    pub position: Point<Pixels>,
    pub entries: Vec<MenuEntry>,
}

pub enum MenuEntry {
    /// `detail` is shown muted on the right: a shortcut or a location.
    Item { label: String, detail: Option<String>, action: MenuAction, danger: bool },
    Separator,
    Header(String),
}

#[derive(Clone)]
pub enum MenuAction {
    Command(Command),
    RemoveFavorite(usize),
    OpenPathWith(PathBuf, String),
    RevealPath(PathBuf),
    Navigate(PathBuf),
    /// Jump `steps` entries back (or forward) through history.
    History { back: bool, steps: usize },
}

/// Why the current folder couldn't be listed.
pub struct LoadError {
    pub message: String,
    /// macOS privacy protection (TCC) blocked access, e.g. cloud drives.
    pub not_permitted: bool,
}

/// Payload while dragging items around inside Findest.
#[derive(Clone)]
pub struct DraggedFiles {
    pub paths: std::rc::Rc<Vec<PathBuf>>,
}

/// What to select (and optionally start renaming) once the next listing arrives.
struct PendingSelect {
    paths: Vec<PathBuf>,
    rename: bool,
}

pub struct Browser {
    pub(crate) focus: FocusHandle,
    pub(crate) config: Config,
    config_path: PathBuf,
    pub(crate) keymap: BTreeMap<String, String>,
    pub(crate) theme: Theme,
    pub(crate) location: Location,
    back: Vec<Location>,
    forward: Vec<Location>,
    pub(crate) entries: Vec<Entry>,
    pub(crate) loading: bool,
    pub(crate) load_error: Option<LoadError>,
    /// A pending press on back/forward: a quick click navigates, a long press opens history.
    nav_press: Option<Task<()>>,
    load_task: Option<Task<()>>,
    pending_select: Option<PendingSelect>,
    /// Set when a just-created item should enter rename mode on the next frame.
    pub(crate) pending_rename: bool,
    pub(crate) selected: HashSet<PathBuf>,
    cursor: Option<PathBuf>,
    anchor: Option<PathBuf>,
    pub(crate) edit: Option<Edit>,
    file_clipboard: Vec<PathBuf>,
    pub(crate) status: Option<Status>,
    pub(crate) menu: Option<ContextMenu>,
    pub(crate) scroll: UniformListScrollHandle,
    recents: Recents,
    pub(crate) volumes: Vec<Place>,
    pub(crate) cloud: Vec<Place>,
    pub(crate) home: PathBuf,
    pub(crate) icons: IconCache,
    /// Recently visited folders, most recent first.
    visited: Recents,
    watcher: Option<FsWatcher>,
    type_ahead: (String, Instant),
    pub(crate) last_title: String,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for Browser {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Browser {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let config_path = Config::path();
        let (config, mut errors) = match Config::load_or_init(&config_path) {
            Ok(config) => (config, Vec::new()),
            Err(e) => (Config::default(), vec![format!("{e:#}")]),
        };
        let (theme, theme_errors) = Theme::resolve(&config.appearance, window.appearance());
        errors.extend(theme_errors);
        let keymap = config.keybindings();
        errors.extend(apply_keybindings(&keymap, cx));

        let mut tasks = Vec::new();
        let watcher = match config_path.parent().map(FsWatcher::new) {
            Some(Ok((watcher, rx))) => {
                tasks.push(cx.spawn_in(window, async move |this, cx| {
                    let mut rx = rx.ready_chunks(256);
                    while let Some(paths) = rx.next().await {
                        // Coalesce bursts (e.g. a large copy) into one refresh.
                        cx.background_executor().timer(Duration::from_millis(60)).await;
                        if this.update_in(cx, |this, window, cx| this.on_fs_change(paths, window, cx)).is_err() {
                            break;
                        }
                    }
                }));
                Some(watcher)
            }
            Some(Err(e)) => {
                errors.push(format!("file watching unavailable: {e}"));
                None
            }
            None => None,
        };

        let subscriptions = vec![cx.observe_window_appearance(window, |this, window, cx| {
            this.apply_config(this.config.clone(), window, cx);
        })];

        let start = config.start_path();
        let mut this = Self {
            focus: cx.focus_handle(),
            config,
            config_path,
            keymap,
            theme,
            location: Location::Recents,
            back: Vec::new(),
            forward: Vec::new(),
            entries: Vec::new(),
            loading: false,
            load_error: None,
            nav_press: None,
            load_task: None,
            pending_select: None,
            pending_rename: false,
            selected: HashSet::new(),
            cursor: None,
            anchor: None,
            edit: None,
            file_clipboard: Vec::new(),
            status: None,
            menu: None,
            scroll: UniformListScrollHandle::new(),
            recents: Recents::load(&Recents::path()),
            volumes: locations::volumes(),
            cloud: locations::cloud_places(&home_dir()),
            home: home_dir(),
            icons: IconCache::default(),
            visited: Recents::load(&Recents::locations_path()),
            watcher,
            type_ahead: (String::new(), Instant::now()),
            last_title: String::new(),
            _tasks: tasks,
            _subscriptions: subscriptions,
        };
        let start = if start.is_dir() { start } else { home_dir() };
        this.navigate(Location::Dir(start), false, cx);
        if !errors.is_empty() {
            this.set_error(format!("Config: {}", errors.join("; ")), cx);
        }
        this
    }

    // ---- Config -------------------------------------------------------

    fn reload_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match Config::load(&self.config_path) {
            Ok(config) => {
                self.apply_config(config, window, cx);
                if self.status.as_ref().is_none_or(|s| !s.is_error) {
                    self.set_info("Config reloaded", cx);
                }
            }
            Err(e) => self.set_error(format!("{e:#}"), cx),
        }
    }

    fn apply_config(&mut self, config: Config, window: &mut Window, cx: &mut Context<Self>) {
        let (theme, mut errors) = Theme::resolve(&config.appearance, window.appearance());
        let keymap = config.keybindings();
        errors.extend(apply_keybindings(&keymap, cx));
        let listing_changed = config.general != self.config.general;
        self.theme = theme;
        self.keymap = keymap;
        self.config = config;
        if listing_changed {
            self.reload(cx);
        }
        if errors.is_empty() {
            self.status = None;
        } else {
            self.set_error(format!("Config: {}", errors.join("; ")), cx);
        }
        cx.notify();
    }

    fn on_fs_change(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let mut refresh = false;
        let mut config_changed = false;
        for path in &paths {
            if *path == self.config_path {
                config_changed = true;
            }
            if let Location::Dir(dir) = &self.location
                && (path == dir || path.parent() == Some(dir.as_path()))
            {
                refresh = true;
            }
        }
        if config_changed {
            self.reload_config(window, cx);
        }
        if refresh {
            if let Location::Dir(dir) = &self.location
                && !dir.exists()
            {
                // The folder we're in was deleted or moved: walk up to something real.
                let mut up = dir.clone();
                while !up.is_dir() && up.pop() {}
                self.navigate(Location::Dir(up), false, cx);
            } else {
                self.reload(cx);
            }
        }
    }

    // ---- Navigation & loading ----------------------------------------

    pub fn navigate(&mut self, location: Location, push_history: bool, cx: &mut Context<Self>) {
        if push_history && location != self.location {
            self.back.push(self.location.clone());
            self.forward.clear();
        }
        if let Location::Dir(dir) = &location {
            self.visited.push(dir.clone(), 25);
            let visited = self.visited.clone();
            cx.background_executor().spawn(async move { visited.save(&Recents::locations_path()) }).detach();
        }
        self.location = location;
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
        self.edit = None;
        self.menu = None;
        self.entries.clear();
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        let dir = self.current_dir();
        if let Some(watcher) = &mut self.watcher {
            watcher.watch_dir(dir.as_deref());
        }
        self.reload(cx);
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let location = self.location.clone();
        let show_hidden = self.config.general.show_hidden;
        let order = self.config.general.sort_order();
        let recents: Vec<PathBuf> = self.recents.items.clone();
        self.loading = true;
        let listing = cx.background_executor().spawn(async move {
            match &location {
                Location::Dir(dir) => listing::read_dir(dir, show_hidden).map(|mut entries| {
                    listing::sort_entries(&mut entries, order);
                    entries
                }),
                Location::Recents => Ok(recents.iter().filter_map(|p| Entry::from_path(p).ok()).collect()),
            }
        });
        // Replacing the task drops (cancels) any listing still in flight.
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = listing.await;
            this.update(cx, |this, cx| this.finish_load(result, cx)).ok();
        }));
        cx.notify();
    }

    fn finish_load(&mut self, result: std::io::Result<Vec<Entry>>, cx: &mut Context<Self>) {
        self.loading = false;
        self.load_error = None;
        match result {
            Ok(entries) => self.entries = entries,
            Err(e) => {
                self.entries.clear();
                self.load_error = Some(LoadError {
                    not_permitted: matches!(e.raw_os_error(), Some(1 /* EPERM */ | 13 /* EACCES */)),
                    message: format!("Can't open {}: {e}", self.location_label()),
                });
            }
        }
        let present: HashSet<&PathBuf> = self.entries.iter().map(|e| &e.path).collect();
        self.selected.retain(|p| present.contains(p));
        if self.cursor.as_ref().is_some_and(|c| !present.contains(c)) {
            self.cursor = None;
        }
        if let Some(pending) = self.pending_select.take() {
            let found: Vec<PathBuf> = pending.paths.into_iter().filter(|p| present.contains(p)).collect();
            if !found.is_empty() {
                self.selected = found.iter().cloned().collect();
                self.set_cursor(found.last().cloned());
                self.anchor = found.first().cloned();
                if pending.rename && found.len() == 1 {
                    self.pending_rename = true;
                }
            }
        }
        cx.notify();
    }

    pub fn current_dir(&self) -> Option<PathBuf> {
        match &self.location {
            Location::Dir(dir) => Some(dir.clone()),
            Location::Recents => None,
        }
    }

    pub fn location_label(&self) -> String {
        match &self.location {
            Location::Dir(dir) => locations::friendly_name(&self.home, dir),
            Location::Recents => "Recents".into(),
        }
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    // ---- Selection ----------------------------------------------------

    fn index_of(&self, path: &Path) -> Option<usize> {
        self.entries.iter().position(|e| e.path == path)
    }

    fn cursor_index(&self) -> Option<usize> {
        self.cursor.as_deref().and_then(|c| self.index_of(c))
    }

    fn set_cursor(&mut self, path: Option<PathBuf>) {
        if let Some(ix) = path.as_deref().and_then(|p| self.index_of(p)) {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Top);
        }
        self.cursor = path;
    }

    pub fn select_only(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(path) = self.entries.get(ix).map(|e| e.path.clone()) else { return };
        self.selected.clear();
        self.selected.insert(path.clone());
        self.anchor = Some(path.clone());
        self.set_cursor(Some(path));
        cx.notify();
    }

    pub fn toggle_selected(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(path) = self.entries.get(ix).map(|e| e.path.clone()) else { return };
        if !self.selected.remove(&path) {
            self.selected.insert(path.clone());
        }
        self.anchor = Some(path.clone());
        self.set_cursor(Some(path));
        cx.notify();
    }

    /// Selects everything between the anchor and `ix`.
    pub fn select_range_to(&mut self, ix: usize, cx: &mut Context<Self>) {
        let anchor_ix = self.anchor.as_deref().and_then(|a| self.index_of(a)).unwrap_or(ix);
        let (lo, hi) = (anchor_ix.min(ix), anchor_ix.max(ix));
        self.selected = self.entries[lo..=hi].iter().map(|e| e.path.clone()).collect();
        if self.anchor.is_none() {
            self.anchor = self.entries.get(ix).map(|e| e.path.clone());
        }
        self.set_cursor(self.entries.get(ix).map(|e| e.path.clone()));
        cx.notify();
    }

    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
        cx.notify();
    }

    fn move_cursor(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        let ix = match self.cursor_index() {
            Some(ix) => (ix as isize + delta).clamp(0, last as isize) as usize,
            None if delta < 0 => last,
            None => 0,
        };
        if extend { self.select_range_to(ix, cx) } else { self.select_only(ix, cx) }
    }

    /// Selected paths in display order.
    pub fn selection(&self) -> Vec<PathBuf> {
        self.entries.iter().filter(|e| self.selected.contains(&e.path)).map(|e| e.path.clone()).collect()
    }

    fn single_selected_entry(&self) -> Option<&Entry> {
        if self.selected.len() != 1 {
            return None;
        }
        self.entries.iter().find(|e| self.selected.contains(&e.path))
    }

    /// The selection, or the current folder when nothing is selected.
    fn selection_or_current(&self) -> Vec<PathBuf> {
        let selection = self.selection();
        if selection.is_empty() { self.current_dir().into_iter().collect() } else { selection }
    }

    fn type_ahead(&mut self, ch: &str, cx: &mut Context<Self>) {
        let (buffer, last) = &mut self.type_ahead;
        if last.elapsed() > Duration::from_millis(900) {
            buffer.clear();
        }
        *last = Instant::now();
        buffer.push_str(&ch.to_lowercase());
        let needle = buffer.clone();
        if let Some(ix) = self.entries.iter().position(|e| e.display_name().to_lowercase().starts_with(&needle)) {
            self.select_only(ix, cx);
        }
    }

    /// Handles printable keys not claimed by a keybinding: jump-to-name.
    pub fn on_key_down(&mut self, event: &gpui::KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let m = event.keystroke.modifiers;
        if self.edit.is_some() || m.platform || m.control || m.alt || m.function {
            return;
        }
        if let Some(ch) = event.keystroke.key_char.as_deref()
            && !ch.chars().any(char::is_control)
            && !(ch == " " && self.type_ahead.0.is_empty())
        {
            self.type_ahead(ch, cx);
            cx.stop_propagation();
        }
    }

    // ---- Status -------------------------------------------------------

    pub fn set_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.status = Some(Status { message: message.into(), is_error: true });
        cx.notify();
    }

    pub fn set_info(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.status = Some(Status { message: message.into(), is_error: false });
        cx.notify();
    }

    // ---- Commands -----------------------------------------------------

    pub fn on_run(&mut self, action: &command::Run, window: &mut Window, cx: &mut Context<Self>) {
        // While a text field is focused, leave keys to it.
        if self.edit.is_some() {
            cx.propagate();
            return;
        }
        match action.command.parse::<Command>() {
            Ok(command) => self.run(command, window, cx),
            Err(e) => self.set_error(e, cx),
        }
    }

    pub fn run(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        use Command::*;
        self.menu = None;
        if !matches!(command, SelectPrev | SelectNext | ExtendPrev | ExtendNext) {
            self.status = None;
        }
        match command {
            SelectPrev => self.move_cursor(-1, false, cx),
            SelectNext => self.move_cursor(1, false, cx),
            ExtendPrev => self.move_cursor(-1, true, cx),
            ExtendNext => self.move_cursor(1, true, cx),
            SelectFirst => self.select_only(0, cx),
            SelectLast => self.select_only(self.entries.len().saturating_sub(1), cx),
            SelectAll => {
                self.selected = self.entries.iter().map(|e| e.path.clone()).collect();
                cx.notify();
            }
            ClearSelection => self.clear_selection(cx),
            Open => self.open_selection(cx),
            Rename => self.start_rename(window, cx),
            GoUp => self.go_up(cx),
            GoBack => self.go_history(true, 1, cx),
            GoForward => self.go_history(false, 1, cx),
            GoHome => self.navigate(Location::Dir(home_dir()), true, cx),
            GoToFolder => self.start_go_to_folder(window, cx),
            NewFolder | NewFile => {
                let Some(dir) = self.current_dir() else {
                    return self.set_error("Open a folder first", cx);
                };
                let folder = command == NewFolder;
                self.run_op(
                    cx,
                    move || if folder { ops::create_folder(&dir) } else { ops::create_file(&dir) },
                    |this, path, cx| this.reload_and_select(vec![path], true, cx),
                );
            }
            Duplicate => {
                let paths = self.selection();
                if paths.is_empty() {
                    return;
                }
                self.run_op(
                    cx,
                    move || paths.iter().map(|p| ops::duplicate(p)).collect::<Result<Vec<_>>>(),
                    |this, created, cx| this.reload_and_select(created, false, cx),
                );
            }
            Trash => self.trash(self.config.general.confirm_trash, window, cx),
            TrashConfirm => self.trash(true, window, cx),
            TrashNow => self.trash(false, window, cx),
            DeletePermanently => self.delete_permanently(window, cx),
            Copy => {
                let paths = self.selection();
                if paths.is_empty() {
                    return;
                }
                self.set_info(format!("Copied {}", count_label(paths.len())), cx);
                self.file_clipboard = paths;
            }
            Paste | MoveHere => {
                let Some(dir) = self.current_dir() else {
                    return self.set_error("Open a folder to paste into", cx);
                };
                if self.file_clipboard.is_empty() {
                    return self.set_error("Nothing to paste — copy items with ⌘C first", cx);
                }
                let sources = self.file_clipboard.clone();
                let moving = command == MoveHere;
                if moving {
                    self.file_clipboard.clear();
                }
                self.run_op(
                    cx,
                    move || if moving { ops::move_into(&sources, &dir) } else { ops::copy_into(&sources, &dir) },
                    |this, created, cx| this.reload_and_select(created, false, cx),
                );
            }
            CopyPath => {
                let paths = self.selection_or_current();
                let text = paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.set_info(format!("Copied {}", if paths.len() == 1 { "path".into() } else { format!("{} paths", paths.len()) }), cx);
            }
            ToggleHidden => {
                self.config.general.show_hidden = !self.config.general.show_hidden;
                self.reload(cx);
            }
            Refresh => {
                self.volumes = locations::volumes();
                self.cloud = locations::cloud_places(&self.home);
                self.icons.clear_paths();
                self.reload(cx);
            }
            AddFavorite => self.add_favorite(cx),
            ShowRecents => self.navigate(Location::Recents, true, cx),
            RevealInFinder => {
                for path in self.selection_or_current() {
                    self.report(launch::reveal_in_finder(&path), cx);
                }
            }
            OpenConfig => {
                let path = self.config_path.clone();
                let opener = self.config.openers.iter().find(|o| o.applies_to(false)).cloned();
                let result = match opener {
                    Some(opener) => launch::run_opener(&opener, &path),
                    None => launch::open_default(&path),
                };
                self.report(result, cx);
            }
            OpenWith(name) => {
                let targets = self.selection_or_current();
                self.open_with(&name, &targets, cx);
            }
        }
    }

    pub fn run_menu_action(&mut self, action: MenuAction, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        match action {
            MenuAction::Command(command) => self.run(command, window, cx),
            MenuAction::RemoveFavorite(ix) => {
                if ix < self.config.favorites.len() {
                    let mut favorites = self.config.favorites.clone();
                    favorites.remove(ix);
                    self.save_favorites(favorites, cx);
                }
            }
            MenuAction::OpenPathWith(path, opener) => self.open_with(&opener, &[path], cx),
            MenuAction::Navigate(path) => self.navigate_to_path(path, cx),
            MenuAction::History { back, steps } => self.go_history(back, steps, cx),
            MenuAction::RevealPath(path) => {
                let result = launch::reveal_in_finder(&path);
                self.report(result, cx);
            }
        }
        cx.notify();
    }

    fn report(&mut self, result: Result<()>, cx: &mut Context<Self>) {
        if let Err(e) = result {
            self.set_error(format!("{e:#}"), cx);
        }
    }

    /// Runs a file operation off the UI thread, then `done` on success.
    fn run_op<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        op: impl FnOnce() -> Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        let task = cx.background_executor().spawn(async move { op() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(value) => done(this, value, cx),
                Err(e) => {
                    this.set_error(format!("{e:#}"), cx);
                    this.reload(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn reload_and_select(&mut self, paths: Vec<PathBuf>, rename: bool, cx: &mut Context<Self>) {
        self.pending_select = Some(PendingSelect { paths, rename });
        self.reload(cx);
    }

    pub fn open_entry(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.select_only(ix, cx);
        self.open_selection(cx);
    }

    fn open_selection(&mut self, cx: &mut Context<Self>) {
        let entries: Vec<Entry> = self.entries.iter().filter(|e| self.selected.contains(&e.path)).cloned().collect();
        if let [entry] = entries.as_slice()
            && entry.is_dir
        {
            return self.navigate(Location::Dir(entry.path.clone()), true, cx);
        }
        for entry in entries.iter().filter(|e| !e.is_dir) {
            let result = launch::open_default(&entry.path);
            if result.is_ok() {
                self.remember(&entry.path);
            }
            self.report(result, cx);
        }
        self.save_recents(cx);
    }

    fn open_with(&mut self, name: &str, targets: &[PathBuf], cx: &mut Context<Self>) {
        let Some(opener) = self.config.openers.iter().find(|o| o.name == name).cloned() else {
            return self.set_error(format!("No opener named \"{name}\" in config"), cx);
        };
        let mut ran = 0;
        for path in targets {
            let is_dir = path.is_dir() && !self.entries.iter().any(|e| e.path == *path && e.is_package);
            if !opener.applies_to(is_dir) {
                continue;
            }
            let result = launch::run_opener(&opener, path);
            if result.is_ok() {
                ran += 1;
                if !is_dir {
                    self.remember(path);
                }
            }
            self.report(result, cx);
        }
        if ran == 0 && !targets.is_empty() {
            self.set_error(format!("{} doesn't open {}", opener.name, if targets[0].is_dir() { "folders" } else { "files" }), cx);
        }
        self.save_recents(cx);
    }

    fn remember(&mut self, path: &Path) {
        self.recents.push(path.to_path_buf(), self.config.general.max_recents);
    }

    fn save_recents(&mut self, cx: &mut Context<Self>) {
        let recents = self.recents.clone();
        cx.background_executor().spawn(async move { recents.save(&Recents::path()) }).detach();
        if self.location == Location::Recents {
            self.reload(cx);
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.current_dir() else { return };
        if let Some(parent) = dir.parent() {
            self.navigate(Location::Dir(parent.to_path_buf()), true, cx);
            self.pending_select = Some(PendingSelect { paths: vec![dir], rename: false });
        }
    }

    pub fn navigate_to_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if path.is_dir() {
            self.navigate(Location::Dir(path), true, cx);
        } else if let Some(parent) = path.parent().filter(|_| path.exists()) {
            self.navigate(Location::Dir(parent.to_path_buf()), true, cx);
            self.pending_select = Some(PendingSelect { paths: vec![path], rename: false });
        } else {
            self.set_error(format!("{} doesn't exist", contract_tilde(&path)), cx);
        }
    }

    fn trash(&mut self, confirm: bool, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.selection();
        if paths.is_empty() {
            return;
        }
        let message = format!("Move {} to the Trash?", describe(&paths));
        self.confirm_then(confirm, &message, None, "Move to Trash", window, cx, move |this, cx| {
            let n = paths.len();
            this.run_op(cx, move || ops::move_to_trash(&paths), move |this, (), cx| {
                this.set_info(format!("Moved {} to the Trash", count_label(n)), cx);
                this.reload(cx);
            });
        });
    }

    fn delete_permanently(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.selection();
        if paths.is_empty() {
            return;
        }
        let message = format!("Delete {} permanently?", describe(&paths));
        self.confirm_then(true, &message, Some("This can't be undone."), "Delete", window, cx, move |this, cx| {
            let n = paths.len();
            this.run_op(cx, move || ops::delete_permanently(&paths), move |this, (), cx| {
                this.set_info(format!("Deleted {}", count_label(n)), cx);
                this.reload(cx);
            });
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn confirm_then(
        &mut self,
        ask: bool,
        message: &str,
        detail: Option<&str>,
        confirm_label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        if !ask {
            return then(self, cx);
        }
        let answer = window.prompt(PromptLevel::Warning, message, detail, &[confirm_label, "Cancel"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                this.update(cx, |this, cx| then(this, cx)).ok();
            }
        })
        .detach();
    }

    // ---- Favorites ----------------------------------------------------

    fn add_favorite(&mut self, cx: &mut Context<Self>) {
        let target = match self.single_selected_entry() {
            Some(entry) if entry.is_dir => entry.path.clone(),
            _ => match self.current_dir() {
                Some(dir) => dir,
                None => return self.set_error("Select a folder to add to Favorites", cx),
            },
        };
        if self.config.favorites.iter().any(|f| f.resolved_path() == target) {
            return self.set_info(format!("{} is already a favorite", display_name(&target)), cx);
        }
        let mut favorites = self.config.favorites.clone();
        favorites.push(Favorite { path: contract_tilde(&target), name: None });
        self.set_info(format!("Added {} to Favorites", display_name(&target)), cx);
        self.save_favorites(favorites, cx);
    }

    fn save_favorites(&mut self, favorites: Vec<Favorite>, cx: &mut Context<Self>) {
        let result = Config::save_favorites(&self.config_path, &favorites);
        self.config.favorites = favorites;
        self.report(result, cx);
        cx.notify();
    }

    // ---- Inline editing -----------------------------------------------

    pub fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.single_selected_entry().cloned() else { return };
        // Pre-select the name without its extension, like Finder.
        let stem_len = match entry.name.rfind('.') {
            Some(i) if i > 0 && !entry.is_dir => i,
            _ => entry.name.len(),
        };
        self.start_edit(EditKind::Rename(entry.path.clone()), entry.name.clone(), 0..stem_len, "", window, cx);
    }

    fn start_go_to_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = match &self.location {
            Location::Dir(dir) => {
                let mut s = contract_tilde(dir);
                if !s.ends_with('/') {
                    s.push('/');
                }
                s
            }
            Location::Recents => "~/".into(),
        };
        let len = text.len();
        self.start_edit(EditKind::GoToFolder, text, len..len, "Path, e.g. ~/Projects", window, cx);
    }

    fn start_edit(
        &mut self,
        kind: EditKind,
        text: String,
        selection: std::ops::Range<usize>,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let theme = self.theme.clone();
        let placeholder = placeholder.to_string();
        let input = cx.new(|cx| {
            let mut input = TextInput::new(cx);
            input.cursor_color = theme.accent;
            input.selection_color = theme.accent.opacity(0.35);
            input.placeholder_color = theme.text_muted;
            input.set_placeholder(placeholder);
            input.set_text(text, selection, cx);
            input
        });
        let subscriptions = vec![
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                TextInputEvent::Confirm => this.commit_edit(window, cx),
                TextInputEvent::Cancel => this.cancel_edit(window, cx),
            }),
            cx.on_blur(&input.focus_handle(cx), window, |this, window, cx| {
                // Clicking away commits a rename, but abandons go-to-folder.
                match this.edit.as_ref().map(|e| &e.kind) {
                    Some(EditKind::Rename(_)) => this.commit_edit(window, cx),
                    Some(EditKind::GoToFolder) => this.cancel_edit(window, cx),
                    None => {}
                }
            }),
        ];
        window.focus(&input.focus_handle(cx));
        self.edit = Some(Edit { kind, input, _subscriptions: subscriptions });
        cx.notify();
    }

    pub fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.edit.take() else { return };
        let text = edit.input.read(cx).text().to_string();
        window.focus(&self.focus);
        match edit.kind {
            EditKind::Rename(path) => {
                if text == display_name(&path) {
                    return cx.notify();
                }
                match ops::rename(&path, &text) {
                    Ok(new_path) => {
                        if self.recents.items.contains(&path) {
                            self.recents.remove(&path);
                            self.remember(&new_path);
                            self.save_recents(cx);
                        }
                        self.reload_and_select(vec![new_path], false, cx);
                    }
                    Err(e) => self.set_error(format!("{e:#}"), cx),
                }
            }
            EditKind::GoToFolder => {
                let text = text.trim();
                if !text.is_empty() {
                    self.navigate_to_path(expand_tilde(text), cx);
                }
            }
        }
        cx.notify();
    }

    pub fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.take().is_some() {
            window.focus(&self.focus);
            cx.notify();
        }
    }

    pub fn renaming(&self) -> Option<(&Path, &Entity<TextInput>)> {
        match &self.edit {
            Some(Edit { kind: EditKind::Rename(path), input, .. }) => Some((path, input)),
            _ => None,
        }
    }

    // ---- Context menus ------------------------------------------------

    fn item(&self, label: impl Into<String>, command: Command, command_name: &str) -> MenuEntry {
        MenuEntry::Item {
            label: label.into(),
            detail: command::shortcut_label(&self.keymap, command_name),
            action: MenuAction::Command(command),
            danger: false,
        }
    }

    fn opener_items(&self, is_dir: bool) -> Vec<MenuEntry> {
        self.config
            .openers
            .iter()
            .filter(|o| o.applies_to(is_dir))
            .map(|o| {
                let name = format!("open_with:{}", o.name);
                self.item(format!("Open in {}", o.name), Command::OpenWith(o.name.clone()), &name)
            })
            .collect()
    }

    /// Context menu for the file list. With `on_item`, the clicked row is
    /// already part of the selection.
    pub fn open_list_menu(&mut self, position: Point<Pixels>, on_item: bool, cx: &mut Context<Self>) {
        use Command::*;
        let mut entries = Vec::new();
        let in_dir = self.current_dir().is_some();
        if on_item {
            let selection: Vec<Entry> =
                self.entries.iter().filter(|e| self.selected.contains(&e.path)).cloned().collect();
            let single = selection.len() == 1;
            let all_dirs = selection.iter().all(|e| e.is_dir);
            entries.push(self.item("Open", Open, "open"));
            entries.extend(self.opener_items(all_dirs));
            entries.push(MenuEntry::Separator);
            if single {
                entries.push(self.item("Rename", Rename, "rename"));
            }
            entries.push(self.item("Duplicate", Duplicate, "duplicate"));
            entries.push(self.item("Copy", Copy, "copy"));
            entries.push(self.item(if single { "Copy Path" } else { "Copy Paths" }, CopyPath, "copy_path"));
            if single && all_dirs {
                entries.push(self.item("Add to Favorites", AddFavorite, "add_favorite"));
            }
            entries.push(self.item("Reveal in Finder", RevealInFinder, "reveal_in_finder"));
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item {
                label: "Move to Trash".into(),
                detail: command::shortcut_label(&self.keymap, "trash"),
                action: MenuAction::Command(Trash),
                danger: true,
            });
        } else {
            if in_dir {
                entries.push(self.item("New Folder", NewFolder, "new_folder"));
                entries.push(self.item("New File", NewFile, "new_file"));
                if !self.file_clipboard.is_empty() {
                    let n = count_label(self.file_clipboard.len());
                    entries.push(self.item(format!("Paste {n}"), Paste, "paste"));
                    entries.push(self.item(format!("Move {n} Here"), MoveHere, "move_here"));
                }
                entries.push(MenuEntry::Separator);
                entries.extend(self.opener_items(true));
                entries.push(MenuEntry::Separator);
                entries.push(self.item("Copy Path", CopyPath, "copy_path"));
                entries.push(self.item("Add to Favorites", AddFavorite, "add_favorite"));
            }
            let hidden = if self.config.general.show_hidden { "Hide Hidden Files" } else { "Show Hidden Files" };
            entries.push(self.item(hidden, ToggleHidden, "toggle_hidden"));
            entries.push(self.item("Refresh", Refresh, "refresh"));
        }
        self.menu = Some(ContextMenu { position, entries });
        cx.notify();
    }

    pub fn open_sidebar_menu(&mut self, position: Point<Pixels>, path: PathBuf, favorite: Option<usize>, cx: &mut Context<Self>) {
        let mut entries: Vec<MenuEntry> = self
            .config
            .openers
            .iter()
            .filter(|o| o.applies_to(true))
            .map(|o| MenuEntry::Item {
                label: format!("Open in {}", o.name),
                detail: None,
                action: MenuAction::OpenPathWith(path.clone(), o.name.clone()),
                danger: false,
            })
            .collect();
        entries.push(MenuEntry::Item {
            label: "Reveal in Finder".into(),
            detail: None,
            action: MenuAction::RevealPath(path),
            danger: false,
        });
        if let Some(ix) = favorite {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item {
                label: "Remove from Favorites".into(),
                detail: None,
                action: MenuAction::RemoveFavorite(ix),
                danger: true,
            });
        }
        self.menu = Some(ContextMenu { position, entries });
        cx.notify();
    }

    /// Moves `steps` entries through the back (or forward) stack, like a browser.
    fn go_history(&mut self, back: bool, steps: usize, cx: &mut Context<Self>) {
        let (from, to) = if back { (&mut self.back, &mut self.forward) } else { (&mut self.forward, &mut self.back) };
        if steps == 0 || steps > from.len() {
            return;
        }
        to.push(self.location.clone());
        for _ in 1..steps {
            let skipped = from.pop().expect("checked length");
            to.push(skipped);
        }
        let target = from.pop().expect("checked length");
        self.navigate(target, false, cx);
    }

    /// Mouse down on back/forward: navigate on release, or open history after a long press.
    pub fn press_nav(&mut self, back: bool, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.nav_press = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(400)).await;
            this.update(cx, |this, cx| {
                if this.nav_press.take().is_some() {
                    this.open_history_menu(back, position, cx);
                }
            })
            .ok();
        }));
    }

    /// Mouse up on back/forward: a short press navigates.
    pub fn release_nav(&mut self, back: bool, cx: &mut Context<Self>) {
        if self.nav_press.take().is_some() {
            self.status = None;
            self.go_history(back, 1, cx);
        }
    }

    pub fn cancel_nav_press(&mut self) {
        self.nav_press = None;
    }

    fn location_entry(&self, location: &Location, action: MenuAction) -> MenuEntry {
        let (label, detail) = match location {
            Location::Dir(dir) => (locations::friendly_name(&self.home, dir), dir.parent().map(contract_tilde)),
            Location::Recents => ("Recents".into(), None),
        };
        MenuEntry::Item { label, detail, action, danger: false }
    }

    /// History menu for back/forward. The back menu also lists recently
    /// visited folders from earlier sessions.
    pub fn open_history_menu(&mut self, back: bool, position: Point<Pixels>, cx: &mut Context<Self>) {
        let stack = if back { &self.back } else { &self.forward };
        let mut entries: Vec<MenuEntry> = stack
            .iter()
            .rev()
            .enumerate()
            .take(15)
            .map(|(i, loc)| self.location_entry(loc, MenuAction::History { back, steps: i + 1 }))
            .collect();
        if back {
            let shown: HashSet<&Location> = self.back.iter().chain([&self.location]).collect();
            let recent: Vec<MenuEntry> = self
                .visited
                .existing()
                .filter(|p| !shown.contains(&Location::Dir((*p).clone())))
                .take(10)
                .map(|p| self.location_entry(&Location::Dir(p.clone()), MenuAction::Navigate(p.clone())))
                .collect();
            if !recent.is_empty() {
                if !entries.is_empty() {
                    entries.push(MenuEntry::Separator);
                }
                entries.push(MenuEntry::Header("Recent Locations".into()));
                entries.extend(recent);
            }
        }
        if entries.is_empty() {
            return;
        }
        self.menu = Some(ContextMenu { position, entries });
        cx.notify();
    }

    /// The icon for row `ix`, kicking off a background load on first request.
    pub fn icon(&mut self, ix: usize, cx: &mut Context<Self>) -> Option<std::sync::Arc<gpui::Image>> {
        let key = IconKey::for_entry(self.entries.get(ix)?, &self.home);
        let (icon, needs_load) = self.icons.get(&key);
        if needs_load {
            let task = cx.background_executor().spawn({
                let key = key.clone();
                async move { crate::icons::load_icon(&key) }
            });
            cx.spawn(async move |this, cx| {
                let png = task.await;
                this.update(cx, |this, cx| {
                    this.icons.insert(key, png);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        icon
    }

    /// Drops `paths` into `dest`: moves within a volume, copies across volumes
    /// or when ⌥ is held (like Finder).
    pub fn drop_paths(&mut self, paths: Vec<PathBuf>, dest: PathBuf, force_copy: bool, cx: &mut Context<Self>) {
        // Dropping onto itself or into the folder it's already in is a no-op.
        let paths: Vec<PathBuf> =
            paths.into_iter().filter(|p| *p != dest && p.parent() != Some(dest.as_path())).collect();
        if paths.is_empty() || !dest.is_dir() {
            return;
        }
        let copy = force_copy || !ops::same_volume(&paths[0], &dest);
        let n = paths.len();
        let dest_name = locations::friendly_name(&self.home, &dest);
        self.run_op(
            cx,
            move || if copy { ops::copy_into(&paths, &dest) } else { ops::move_into(&paths, &dest) },
            move |this, _, cx| {
                let verb = if copy { "Copied" } else { "Moved" };
                this.set_info(format!("{verb} {} to {dest_name}", count_label(n)), cx);
                this.reload(cx);
            },
        );
    }

    pub fn set_sort(&mut self, key: listing::SortKey, cx: &mut Context<Self>) {
        let general = &mut self.config.general;
        if general.sort_by == key {
            general.sort_ascending = !general.sort_ascending;
        } else {
            general.sort_by = key;
            general.sort_ascending = key == listing::SortKey::Name || key == listing::SortKey::Kind;
        }
        let order = general.sort_order();
        if self.location != Location::Recents {
            listing::sort_entries(&mut self.entries, order);
        }
        cx.notify();
    }
}

/// Re-registers all keybindings: text input defaults plus the browser map.
fn apply_keybindings(keymap: &BTreeMap<String, String>, cx: &mut App) -> Vec<String> {
    let (bindings, errors) = command::keybindings(keymap);
    cx.clear_key_bindings();
    crate::bind_global_keys(cx);
    crate::text_input::bind_keys(cx);
    cx.bind_keys(bindings);
    errors
}


pub fn count_label(n: usize) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} items") }
}

fn describe(paths: &[PathBuf]) -> String {
    match paths {
        [one] => format!("\"{}\"", display_name(one)),
        _ => count_label(paths.len()),
    }
}
