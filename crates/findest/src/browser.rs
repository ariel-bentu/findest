//! The main window: state, navigation, selection and command handling.
//! Rendering lives in `render.rs`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use findest_core::config::{Config, Favorite};
use findest_core::listing::{self, Entry};
use findest_core::locations::{self, Place};
use findest_core::paths::{contract_tilde, display_name, home_dir};
use findest_core::recents::Recents;
use findest_core::session::{Session, WindowSession};
use findest_core::{launch, ops};
use futures::StreamExt;
use gpui::{
    App, AppContext, Global, WeakEntity, ClipboardItem, Context, Entity, FocusHandle, Focusable, Pixels, Point,
    PromptLevel, ScrollStrategy, Subscription, Task, UniformListScrollHandle, Window,
};

use crate::command::{self, Command};
use crate::icons::{IconCache, IconKey};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;
use crate::watcher::FsWatcher;

/// How a new window starts out (from the last session, `findest <path>`, or ⌘N).
#[derive(Default)]
pub struct BrowserInit {
    pub start: Option<PathBuf>,
    pub recents: bool,
    pub expanded: Vec<PathBuf>,
    pub info_panel: Option<bool>,
}

impl BrowserInit {
    pub fn at(start: Option<PathBuf>) -> Self {
        Self { start, ..Default::default() }
    }
}

/// Every open browser window, in opening order (used to save the session).
#[derive(Default)]
pub struct OpenBrowsers(pub Vec<WeakEntity<Browser>>);
impl Global for OpenBrowsers {}

/// Set while quitting, so closing windows don't overwrite the saved session.
#[derive(Default)]
pub struct Quitting(pub bool);
impl Global for Quitting {}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Location {
    Dir(PathBuf),
    Recents,
}

pub enum EditKind {
    Rename(PathBuf),
    /// Naming a just-created item: Esc or an empty name removes it again.
    NameNew(PathBuf),
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

/// Details of the single selected item, shown in the info panel.
pub struct InfoState {
    pub path: PathBuf,
    pub file: Option<findest_core::info::FileInfo>,
    pub dimensions: Option<(u32, u32)>,
    /// For folders: `(bytes, items)` once the background walk finishes.
    pub folder_size: Option<(u64, u64)>,
    /// First lines of a text file.
    pub text: Option<String>,
    /// AppKit-rendered preview for images gpui can't decode (HEIC, PSD, PDF...).
    pub preview: Option<std::sync::Arc<gpui::Image>>,
    pub computing: bool,
    pub icon_key: Option<IconKey>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _task: Option<Task<()>>,
}

impl Drop for InfoState {
    fn drop(&mut self) {
        // Stop a folder-size walk for an item that's no longer selected.
        self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Why the current folder couldn't be listed.
pub struct LoadError {
    pub message: String,
    /// macOS privacy protection (TCC) blocked access, e.g. cloud drives.
    pub not_permitted: bool,
}


/// What to select once the next listing arrives; `name_new` then starts
/// naming it (a just-created item).
struct PendingSelect {
    paths: Vec<PathBuf>,
    name_new: bool,
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
    /// Nesting level of each row in `entries` (tree view).
    pub(crate) depths: Vec<usize>,
    /// Whether each row is a folder with something inside (shows a disclosure triangle).
    pub(crate) expandable: Vec<bool>,
    /// `(available, total)` bytes on the current folder's volume.
    pub(crate) volume: Option<(u64, u64)>,
    pub(crate) info: Option<InfoState>,
    pub(crate) search: Option<crate::browser_search::SearchUi>,
    pub(crate) index_generation: u64,
    /// Set when focus should return to the file list on the next frame.
    pub(crate) refocus_list: bool,
    /// Where the search-syntax popover is shown, if open.
    pub(crate) syntax_help: Option<Point<Pixels>>,
    volume_task: Option<Task<()>>,
    /// Folders expanded in place. Kept across navigation, like Finder.
    pub(crate) expanded: HashSet<PathBuf>,
    pub(crate) loading: bool,
    pub(crate) load_error: Option<LoadError>,
    /// A pending press on back/forward: a quick click navigates, a long press opens history.
    nav_press: Option<Task<()>>,
    nav_long_pressed: bool,
    /// Where the mouse went down on a selected row; moving far enough starts a drag.
    pub(crate) drag_origin: Option<Point<Pixels>>,
    load_task: Option<Task<()>>,
    pending_select: Option<PendingSelect>,
    /// Set when a just-created item should be named on the next frame.
    pub(crate) pending_name_new: bool,
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
    pub fn new(init: BrowserInit, window: &mut Window, cx: &mut Context<Self>) -> Self {
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

        let restore_recents = init.recents;
        let restore_info_panel = init.info_panel;
        let start = init.start.unwrap_or_else(|| config.start_path());

        let me = cx.weak_entity();
        let browsers = &mut cx.default_global::<OpenBrowsers>().0;
        browsers.retain(|b| b.upgrade().is_some());
        browsers.push(me);
        let mut subscriptions = subscriptions;
        subscriptions.push(cx.observe_global::<crate::search_service::IndexService>(|this, cx| this.on_index_updated(cx)));
        subscriptions.push(cx.on_release(|_, cx| {
            // Closing one of several windows drops it from the saved session.
            // Closing the last one (or quitting) keeps what was saved.
            if cx.try_global::<Quitting>().is_some_and(|q| q.0) {
                return;
            }
            let windows: Vec<WindowSession> = cx
                .try_global::<OpenBrowsers>()
                .map(|b| b.0.iter().filter_map(|w| w.upgrade()).map(|b| b.read(cx).window_session()).collect())
                .unwrap_or_default();
            if let Some(first) = cx.try_global::<OpenBrowsers>().and_then(|b| b.0.iter().find_map(|w| w.upgrade())) {
                let info_panel = Some(first.read(cx).config.general.info_panel);
                let _ = Session::new(windows, info_panel).save(&Session::path());
            }
        }));
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
            depths: Vec::new(),
            expandable: Vec::new(),
            volume: None,
            info: None,
            search: None,
            index_generation: 0,
            refocus_list: false,
            syntax_help: None,
            volume_task: None,
            expanded: init.expanded.into_iter().collect(),
            loading: false,
            load_error: None,
            nav_press: None,
            nav_long_pressed: false,
            drag_origin: None,
            load_task: None,
            pending_select: None,
            pending_name_new: false,
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
        if let Some(open) = restore_info_panel {
            this.config.general.info_panel = open;
        }
        if restore_recents {
            this.navigate(Location::Recents, false, cx);
        } else if start.is_dir() {
            this.navigate(Location::Dir(start), false, cx);
        } else {
            this.navigate(Location::Dir(home_dir()), false, cx);
            // A file was passed in: show it in its folder.
            this.navigate_to_path(start, cx);
        }
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
        let mut config = config;
        // The panel toggle is a view choice remembered per session; a config
        // reload shouldn't flip it.
        config.general.info_panel = self.config.general.info_panel;
        if config.search != self.config.search {
            gpui::BorrowAppContext::update_global::<crate::search_service::IndexService, _>(cx, |s, _| s.config = config.search.clone());
            crate::search_service::rebuild_now(cx);
        }
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
                && (path == dir || path.parent().is_some_and(|p| p == dir || self.expanded.contains(p)))
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
        // Going somewhere ends a search (e.g. opening a folder from the results).
        if self.search.take().is_some() {
            self.refocus_list = true;
        }
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
        self.discard_edit(cx);
        self.menu = None;
        self.entries.clear();
        self.depths.clear();
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.update_watches();
        self.save_session(cx);
        self.reload(cx);
    }

    /// Persists the open folder and expansions for the next launch.
    fn save_session(&self, cx: &mut Context<Self>) {
        let me = cx.entity_id();
        let browsers = cx.try_global::<OpenBrowsers>().map(|b| b.0.clone()).unwrap_or_default();
        let windows: Vec<WindowSession> = browsers
            .iter()
            .filter_map(|weak| {
                // This window is mid-update, so it can't be read through its handle.
                if weak.entity_id() == me { Some(self.window_session()) } else { weak.upgrade().map(|b| b.read(cx).window_session()) }
            })
            .collect();
        let session = Session::new(windows, Some(self.config.general.info_panel));
        cx.background_executor().spawn(async move { session.save(&Session::path()) }).detach();
    }

    pub fn window_session(&self) -> WindowSession {
        WindowSession {
            folder: self.current_dir(),
            recents: self.location == Location::Recents,
            expanded: self.expanded.iter().cloned().collect(),
        }
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        // While searching, refresh the results instead of the folder listing.
        if self.search_active(cx) {
            return self.run_search(cx);
        }
        let location = self.location.clone();
        let show_hidden = self.config.general.show_hidden;
        let order = self.config.general.sort_order();
        let recents: Vec<PathBuf> = self.recents.items.clone();
        let expanded = self.expanded.clone();
        self.loading = true;
        let listing = cx.background_executor().spawn(async move {
            match &location {
                Location::Dir(dir) => listing::read_tree(dir, &expanded, show_hidden, order),
                Location::Recents => Ok(recents
                    .iter()
                    .filter_map(|p| Entry::from_path(p).ok())
                    .map(|entry| listing::TreeRow { entry, depth: 0, has_children: false })
                    .collect()),
            }
        });
        // Replacing the task drops (cancels) any listing still in flight.
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = listing.await;
            this.update(cx, |this, cx| this.finish_load(result, cx)).ok();
        }));
        cx.notify();
    }

    fn finish_load(&mut self, result: std::io::Result<Vec<listing::TreeRow>>, cx: &mut Context<Self>) {
        self.loading = false;
        self.load_error = None;
        match result {
            Ok(rows) => {
                self.entries = Vec::with_capacity(rows.len());
                self.depths = Vec::with_capacity(rows.len());
                self.expandable = Vec::with_capacity(rows.len());
                for row in rows {
                    self.entries.push(row.entry);
                    self.depths.push(row.depth);
                    self.expandable.push(row.has_children);
                }
            }
            Err(e) => {
                self.entries.clear();
                self.depths.clear();
                self.expandable.clear();
                self.load_error = Some(LoadError {
                    not_permitted: matches!(e.raw_os_error(), Some(1 /* EPERM */ | 13 /* EACCES */)),
                    message: format!("Can't open {}: {e}", self.location_label()),
                });
            }
        }
        self.update_watches();
        self.refresh_volume(cx);
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
                if pending.name_new && found.len() == 1 {
                    self.pending_name_new = true;
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
    pub fn on_key_down(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = event.keystroke.modifiers;
        if self.edit.is_some() || self.search_focused(window, cx) || m.platform || m.control || m.alt || m.function {
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
        self.syntax_help = None;
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
            ClearSelection => {
                if self.selected.is_empty() && self.search.is_some() {
                    self.close_search(window, cx);
                } else {
                    self.clear_selection(cx);
                }
            }
            Find => self.open_search(window, cx),
            RebuildIndex => {
                crate::search_service::rebuild_now(cx);
                self.set_info("Rebuilding the search index…", cx);
            }
            Expand => self.expand_selection(cx),
            Collapse => self.collapse_selection(cx),
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
            EmptyTrash => self.empty_trash(window, cx),
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
            NewWindow => {
                let init = BrowserInit { start: self.current_dir(), info_panel: Some(self.config.general.info_panel), ..Default::default() };
                cx.defer(move |cx| {
                    crate::open_browser_window(init, cx);
                });
            }
            CloseWindow => window.remove_window(),
            ToggleInfo => {
                self.config.general.info_panel = !self.config.general.info_panel;
                self.save_session(cx);
                cx.notify();
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

    fn reload_and_select(&mut self, paths: Vec<PathBuf>, name_new: bool, cx: &mut Context<Self>) {
        self.pending_select = Some(PendingSelect { paths, name_new });
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
            self.pending_select = Some(PendingSelect { paths: vec![dir], name_new: false });
        }
    }

    pub fn navigate_to_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if path.is_dir() {
            self.navigate(Location::Dir(path), true, cx);
        } else if let Some(parent) = path.parent().filter(|_| path.exists()) {
            self.navigate(Location::Dir(parent.to_path_buf()), true, cx);
            self.pending_select = Some(PendingSelect { paths: vec![path], name_new: false });
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

    pub fn trash_dir(&self) -> PathBuf {
        locations::trash(&self.home)
    }

    fn empty_trash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items: Vec<PathBuf> = match std::fs::read_dir(self.trash_dir()) {
            Ok(read) => read.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| !p.ends_with(".DS_Store")).collect(),
            Err(e) => return self.set_error(format!("Can't read the Trash: {e} (Findest needs Full Disk Access)"), cx),
        };
        if items.is_empty() {
            return self.set_info("The Trash is empty", cx);
        }
        let message = format!("Permanently erase {} in the Trash?", count_label(items.len()));
        self.confirm_then(true, &message, Some("This can't be undone."), "Empty Trash", window, cx, move |this, cx| {
            let n = items.len();
            this.run_op(cx, move || ops::delete_permanently(&items), move |this, (), cx| {
                this.set_info(format!("Erased {}", count_label(n)), cx);
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

    /// Names a just-created item, starting from an empty field.
    pub fn start_name_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.single_selected_entry().cloned() else { return };
        let placeholder = if entry.is_dir { "Folder name" } else { "File name" };
        self.start_edit(EditKind::NameNew(entry.path), String::new(), 0..0, placeholder, window, cx);
    }

    fn start_go_to_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A path on the clipboard is probably where the user wants to go:
        // prefill it, selected, so Enter jumps there and typing replaces it.
        let clipboard = cx.read_from_clipboard().and_then(|item| item.text()).filter(|t| t.trim().lines().count() == 1);
        if let Some(text) = clipboard
            && let Some(path) = findest_core::paths::parse_user_path(&text)
            && path.exists()
        {
            let text = contract_tilde(&path);
            let len = text.len();
            return self.start_edit(EditKind::GoToFolder, text, 0..len, "Path, e.g. ~/Projects", window, cx);
        }
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
            // Re-render on every edit so the Go to bar's path check stays live.
            cx.observe(&input, |_, _, cx| cx.notify()),
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                TextInputEvent::Confirm => this.commit_edit(window, cx),
                TextInputEvent::Cancel => this.cancel_edit(window, cx),
                TextInputEvent::Up | TextInputEvent::Down => {}
            }),
            cx.on_blur(&input.focus_handle(cx), window, |this, window, cx| {
                // Clicking away commits a rename, but abandons go-to-folder.
                match this.edit.as_ref().map(|e| &e.kind) {
                    Some(EditKind::Rename(_) | EditKind::NameNew(_)) => this.commit_edit(window, cx),
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
        // Enter on a path that doesn't exist keeps the Go to bar open; it's already
        // showing the warning, so the user can fix the path.
        if let Some(Edit { kind: EditKind::GoToFolder, input, .. }) = &self.edit
            && go_to_target(input.read(cx).text()) == GoToTarget::Missing
        {
            return;
        }
        let Some(edit) = self.edit.take() else { return };
        let text = edit.input.read(cx).text().to_string();
        window.focus(&self.focus);
        match edit.kind {
            EditKind::NameNew(path) if text.trim().is_empty() => self.discard_new(path, cx),
            EditKind::Rename(path) | EditKind::NameNew(path) => {
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
                if let Some(path) = findest_core::paths::parse_user_path(&text) {
                    self.navigate_to_path(path, cx);
                }
            }
        }
        cx.notify();
    }

    pub fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.is_some() {
            self.discard_edit(cx);
            window.focus(&self.focus);
            cx.notify();
        }
    }

    /// Drops any edit in progress, removing a just-created item that never got a name.
    fn discard_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(Edit { kind: EditKind::NameNew(path), .. }) = self.edit.take() {
            self.discard_new(path, cx);
        }
    }

    fn discard_new(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let result = ops::discard_new(&path);
        self.report(result, cx);
        self.reload(cx);
    }

    pub fn renaming(&self) -> Option<(&Path, &Entity<TextInput>)> {
        match &self.edit {
            Some(Edit { kind: EditKind::Rename(path) | EditKind::NameNew(path), input, .. }) => Some((path, input)),
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
            if self.current_dir() == Some(self.trash_dir()) {
                entries.push(MenuEntry::Separator);
                entries.push(MenuEntry::Item {
                    label: "Empty Trash…".into(),
                    detail: command::shortcut_label(&self.keymap, "empty_trash"),
                    action: MenuAction::Command(EmptyTrash),
                    danger: true,
                });
            }
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
        let is_trash = path == self.trash_dir();
        entries.push(MenuEntry::Item {
            label: "Reveal in Finder".into(),
            detail: None,
            action: MenuAction::RevealPath(path),
            danger: false,
        });
        if is_trash {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item {
                label: "Empty Trash…".into(),
                detail: None,
                action: MenuAction::Command(Command::EmptyTrash),
                danger: true,
            });
        }
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

    /// Mouse down on back/forward starts the long-press timer. If it fires
    /// before the click completes, the history menu opens and the click is ignored.
    pub fn press_nav(&mut self, back: bool, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.nav_long_pressed = false;
        self.nav_press = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(450)).await;
            this.update(cx, |this, cx| {
                this.nav_long_pressed = true;
                this.open_history_menu(back, position, cx);
            })
            .ok();
        }));
    }

    /// A completed click on back/forward: navigate, unless it was a long press.
    pub fn click_nav(&mut self, back: bool, cx: &mut Context<Self>) {
        // Dropping the task cancels a pending long-press timer.
        self.nav_press = None;
        if std::mem::take(&mut self.nav_long_pressed) {
            return;
        }
        self.status = None;
        self.go_history(back, 1, cx);
    }

    /// Starts a native drag of the selection once the mouse has moved a few
    /// pixels with the button held since pressing on a selected row.
    pub fn maybe_start_drag(&mut self, event: &gpui::MouseMoveEvent, window: &mut Window) {
        let Some(origin) = self.drag_origin else { return };
        if event.pressed_button != Some(gpui::MouseButton::Left) || self.edit.is_some() {
            self.drag_origin = None;
            return;
        }
        let delta = event.position - origin;
        if f32::from(delta.x).hypot(f32::from(delta.y)) < 5.0 {
            return;
        }
        // Disarm first: AppKit swallows the mouse-up that ends the drag.
        self.drag_origin = None;
        let paths = self.selection();
        if !paths.is_empty() {
            crate::native_drag::start_file_drag(window, &paths);
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
        self.request_icon(key, cx)
    }

    /// Sidebar symbols are tinted with the theme's accent color.
    pub fn symbol_key(&self, name: &'static str) -> IconKey {
        self.symbol_key_colored(name, self.theme.accent)
    }

    /// An SF Symbol tinted `color`, blended over the sidebar background when translucent.
    pub fn symbol_key_colored(&self, name: &'static str, color: gpui::Hsla) -> IconKey {
        let (c, bg) = (gpui::Rgba::from(color), gpui::Rgba::from(self.theme.sidebar));
        let mix = |fg: f32, bg: f32| fg * c.a + bg * (1.0 - c.a);
        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
        IconKey::Symbol { name, rgb: byte(mix(c.r, bg.r)) << 16 | byte(mix(c.g, bg.g)) << 8 | byte(mix(c.b, bg.b)) }
    }

    /// Returns a cached icon, starting a background load the first time.
    pub fn request_icon(&mut self, key: IconKey, cx: &mut Context<Self>) -> Option<std::sync::Arc<gpui::Image>> {
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
        if dest == self.trash_dir() {
            // Go through the system Trash so Finder's "Put Back" keeps working.
            let n = paths.len();
            return self.run_op(cx, move || ops::move_to_trash(&paths), move |this, (), cx| {
                this.set_info(format!("Moved {} to the Trash", count_label(n)), cx);
                this.reload(cx);
            });
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
        if self.location != Location::Recents {
            // Re-list rather than sort in place: each tree level sorts separately.
            self.reload(cx);
        }
        cx.notify();
    }

    // ---- Tree view ----------------------------------------------------

    pub fn is_expandable(&self, ix: usize) -> bool {
        self.expandable.get(ix).copied().unwrap_or(false)
    }

    /// Re-reads free space for the current folder's volume in the background.
    fn refresh_volume(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.current_dir() else {
            self.volume = None;
            return;
        };
        let task = cx.background_executor().spawn(async move { crate::system::volume_capacity(&dir) });
        self.volume_task = Some(cx.spawn(async move |this, cx| {
            let volume = task.await;
            this.update(cx, |this, cx| {
                this.volume = volume;
                cx.notify();
            })
            .ok();
        }));
    }

    /// The item the info panel describes: the single selection, or the current
    /// folder when nothing is selected.
    fn info_target(&self) -> Option<PathBuf> {
        match self.selected.len() {
            0 => self.current_dir(),
            1 => self.selected.iter().next().cloned(),
            _ => None,
        }
    }

    /// Keeps `info` in step with the selection (called each frame; cheap when unchanged).
    pub(crate) fn sync_info(&mut self, cx: &mut Context<Self>) {
        if !self.config.general.info_panel {
            self.info = None;
            return;
        }
        let target = self.info_target();
        if self.info.as_ref().map(|i| &i.path) == target.as_ref() {
            return;
        }
        let Some(path) = target else {
            self.info = None;
            return;
        };
        let file = findest_core::info::file_info(&path).ok();
        // Folders and bundles (apps) get their total size measured.
        let is_dir = path.is_dir();
        let is_image = !is_dir && is_image_path(&path);
        let icon_key = Entry::from_path(&path).ok().map(|e| IconKey::for_entry(&e, &self.home));
        if let Some(key) = icon_key.clone() {
            self.request_icon(key, cx);
        }
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let kind = if is_dir {
            PreviewKind::Folder
        } else if is_image {
            if gpui_decodes(&path) { PreviewKind::Image } else { PreviewKind::RenderedImage }
        } else if is_pdf(&path) {
            PreviewKind::RenderedImage
        } else {
            PreviewKind::Text
        };
        let task = {
            let (path, cancel) = (path.clone(), cancel.clone());
            let work = cx.background_executor().spawn(async move { load_details(&path, kind, &cancel) });
            cx.spawn(async move |this, cx| {
                let details = work.await;
                this.update(cx, |this, cx| {
                    if let Some(info) = &mut this.info {
                        info.dimensions = details.dimensions;
                        info.folder_size = details.folder_size;
                        info.text = details.text;
                        info.preview = details.preview.map(|png| std::sync::Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, png)));
                        info.computing = false;
                        cx.notify();
                    }
                })
                .ok();
            })
        };
        self.info = Some(InfoState {
            path,
            file,
            dimensions: None,
            folder_size: None,
            text: None,
            preview: None,
            computing: true,
            icon_key,
            cancel,
            _task: Some(task),
        });
    }

    /// Total size of selected files (folders aren't measured).
    pub fn selection_size(&self) -> u64 {
        self.entries.iter().filter(|e| !e.is_dir && self.selected.contains(&e.path)).map(|e| e.size).sum()
    }

    pub fn depth(&self, ix: usize) -> usize {
        self.depths.get(ix).copied().unwrap_or(0)
    }

    pub fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    /// Watches the current folder plus every visible expanded subfolder.
    fn update_watches(&mut self) {
        let Some(watcher) = &mut self.watcher else { return };
        let mut dirs = HashSet::new();
        if let Location::Dir(root) = &self.location {
            dirs.insert(root.clone());
            dirs.extend(self.entries.iter().filter(|e| e.is_dir && self.expanded.contains(&e.path)).map(|e| e.path.clone()));
        }
        watcher.watch_dirs(dirs);
    }

    pub fn toggle_expanded(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.expanded.contains(&path) {
            self.collapse_dirs(vec![path], cx);
        } else {
            self.expanded.insert(path);
            self.save_session(cx);
            self.reload(cx);
        }
    }

    fn collapse_dirs(&mut self, dirs: Vec<PathBuf>, cx: &mut Context<Self>) {
        for dir in &dirs {
            self.expanded.remove(dir);
        }
        // If the cursor was inside a collapsed folder, move it to that folder.
        if let Some(cursor) = self.cursor.clone()
            && let Some(dir) = dirs.iter().find(|d| cursor.starts_with(d) && cursor != **d)
        {
            self.pending_select = Some(PendingSelect { paths: vec![dir.clone()], name_new: false });
        }
        self.save_session(cx);
        self.reload(cx);
    }

    fn expand_selection(&mut self, cx: &mut Context<Self>) {
        let dirs: Vec<PathBuf> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(ix, e)| self.is_expandable(*ix) && self.selected.contains(&e.path) && !self.expanded.contains(&e.path))
            .map(|(_, e)| e.path.clone())
            .collect();
        if dirs.is_empty() {
            return;
        }
        self.expanded.extend(dirs);
        self.save_session(cx);
        self.reload(cx);
    }

    /// ← collapses expanded selected folders; otherwise moves to the parent row.
    fn collapse_selection(&mut self, cx: &mut Context<Self>) {
        let dirs: Vec<PathBuf> =
            self.selection().into_iter().filter(|p| self.expanded.contains(p)).collect();
        if !dirs.is_empty() {
            return self.collapse_dirs(dirs, cx);
        }
        if let Some(ix) = self.cursor_index()
            && self.depth(ix) > 0
            && let Some(parent_ix) = self.entries[ix].path.parent().and_then(|p| self.index_of(p))
        {
            self.select_only(parent_ix, cx);
        }
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


#[derive(Debug, PartialEq, Eq)]
pub enum GoToTarget {
    Empty,
    Folder,
    File,
    Missing,
}

/// What the text in the Go to bar points at.
pub fn go_to_target(text: &str) -> GoToTarget {
    match findest_core::paths::parse_user_path(text) {
        None => GoToTarget::Empty,
        Some(path) if path.is_dir() => GoToTarget::Folder,
        Some(path) if path.exists() => GoToTarget::File,
        Some(_) => GoToTarget::Missing,
    }
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| extensions.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

pub fn is_image_path(path: &Path) -> bool {
    has_extension(path, &[
        "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "heic", "heif", "ico", "icns", "psd", "svg",
        "jp2", "tga", "cr2", "cr3", "nef", "arw", "dng", "raf", "orf", "rw2",
    ])
}

/// Formats gpui can decode itself; the info panel shows the file directly.
pub fn gpui_decodes(path: &Path) -> bool {
    has_extension(path, &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
}

fn is_pdf(path: &Path) -> bool {
    has_extension(path, &["pdf"])
}

#[derive(Clone, Copy)]
enum PreviewKind {
    Folder,
    /// gpui decodes the file itself; only dimensions are loaded.
    Image,
    /// Rendered to PNG by AppKit.
    RenderedImage,
    Text,
}

#[derive(Default)]
struct Details {
    dimensions: Option<(u32, u32)>,
    folder_size: Option<(u64, u64)>,
    text: Option<String>,
    preview: Option<Vec<u8>>,
}

/// The slow parts of the info panel, run on a background thread.
fn load_details(path: &Path, kind: PreviewKind, cancel: &std::sync::atomic::AtomicBool) -> Details {
    const TEXT_PREVIEW_LINES: usize = 200;
    const PREVIEW_PIXELS: f64 = 640.0;
    match kind {
        PreviewKind::Folder => Details { folder_size: findest_core::info::dir_size(path, cancel), ..Default::default() },
        PreviewKind::Image => Details { dimensions: crate::system::image_dimensions(path), ..Default::default() },
        PreviewKind::RenderedImage => Details {
            dimensions: if is_pdf(path) { None } else { crate::system::image_dimensions(path) },
            preview: crate::system::image_preview_png(path, PREVIEW_PIXELS),
            ..Default::default()
        },
        PreviewKind::Text => Details { text: findest_core::info::text_preview(path, TEXT_PREVIEW_LINES), ..Default::default() },
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_to_targets() {
        let tmp = std::env::temp_dir();
        let file = tmp.join(format!("findest-goto-{}", std::process::id()));
        std::fs::write(&file, "").unwrap();
        assert_eq!(go_to_target(" "), GoToTarget::Empty);
        assert_eq!(go_to_target(&format!("'{}'", tmp.display())), GoToTarget::Folder);
        assert_eq!(go_to_target(&file.display().to_string()), GoToTarget::File);
        assert_eq!(go_to_target("/definitely/not/here"), GoToTarget::Missing);
        std::fs::remove_file(file).unwrap();
    }
}
