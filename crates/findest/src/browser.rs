//! The main window: state, navigation, selection and command handling.
//! Rendering lives in `render.rs`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result};
use findest_core::config::{Config, Favorite, ViewMode};
use findest_core::listing::{self, Entry};
use findest_core::locations::{self, Place};
use findest_core::paths::{contract_tilde, display_name, home_dir};
use findest_core::recents::Recents;
use findest_core::session::{ColumnWidths, Session, WindowSession};
use findest_core::sources::{self, SourceError};
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
    pub view: Option<ViewMode>,
    pub tile_size: Option<f32>,
    pub columns: Option<ColumnWidths>,
}

/// Paste's destination given the single selected item (if exactly one),
/// what's on the clipboard, and the folder being shown.
fn paste_destination(selected: Option<&Entry>, clipboard: &[PathBuf], current: Option<PathBuf>) -> Option<PathBuf> {
    match selected {
        Some(entry) if entry.is_dir && !clipboard.iter().any(|src| entry.path.starts_with(src)) => Some(entry.path.clone()),
        Some(entry) => entry.path.parent().map(Path::to_path_buf),
        None => current,
    }
}

/// A resizable list column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    Name,
    Modified,
    Size,
    Kind,
}

/// Dragging a column's right edge: the columns after it move along.
pub struct ColumnDrag {
    column: Column,
    start_x: Pixels,
    start_width: f32,
}

/// Tile size range (points), and the step for ⌘= / ⌘-.
pub const MIN_TILE: f32 = 32.0;
pub const MAX_TILE: f32 = 256.0;
const TILE_ZOOM: f32 = 1.25;
/// Images up to this size get a thumbnail tile.
const MAX_THUMBNAIL_BYTES: u64 = 64 * 1024 * 1024;

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

/// A file operation ⌘Z can reverse.
enum UndoOp {
    /// Items created by a copy, with their modification time right after, so
    /// undo can refuse if one was edited since.
    Copy(Vec<(PathBuf, Option<SystemTime>)>),
    /// `(original, current)` locations of moved items.
    Move(Vec<(PathBuf, PathBuf)>),
}

/// How many operations ⌘Z can step back through.
const MAX_UNDO: usize = 20;

/// Typing pauses longer than this start a new type-ahead query.
const TYPE_AHEAD_RESET: Duration = Duration::from_millis(1000);

/// Jump-to-name state: what was typed since the last pause.
#[derive(Default)]
pub(crate) struct TypeAhead {
    pub(crate) query: String,
    /// Nothing matches `query`.
    pub(crate) missed: bool,
    reset: Option<Task<()>>,
}

impl TypeAhead {
    pub(crate) fn is_active(&self) -> bool {
        !self.query.is_empty()
    }
}

/// The entry to jump to for a typed `query`, given lowercased display names
/// with their tree depth. Items of the current folder (depth 0) beat ones in
/// expanded subfolders, and a name prefix beats a match inside the name.
/// Repeating one letter ("ppp") cycles through the names starting with it.
fn type_ahead_match(query: &str, names: &[(String, usize)], cursor: Option<usize>) -> Option<usize> {
    let best = |hit: &dyn Fn(&str) -> bool| {
        let top = names.iter().position(|(name, depth)| *depth == 0 && hit(name));
        top.or_else(|| names.iter().position(|(name, _)| hit(name)))
    };
    if let Some(ix) = best(&|name| name.starts_with(query)) {
        return Some(ix);
    }
    let mut chars = query.chars();
    if let Some(first) = chars.next()
        && chars.clone().next().is_some()
        && chars.all(|c| c == first)
    {
        let hits: Vec<usize> = (0..names.len()).filter(|&ix| names[ix].0.starts_with(first)).collect();
        let min_depth = hits.iter().map(|&ix| names[ix].1).min()?;
        let hits: Vec<usize> = hits.into_iter().filter(|&ix| names[ix].1 == min_depth).collect();
        return hits.iter().copied().find(|&ix| Some(ix) > cursor).or(hits.first().copied());
    }
    best(&|name| name.contains(query))
}

/// Continues a transfer that stopped.
type Retry = Box<dyn FnOnce(&mut Browser, &mut Context<Browser>)>;

/// A copy or move running in the background, shown in the status bar.
pub struct Transfer {
    /// "Copying" or "Moving".
    pub verb: &'static str,
    pub progress: Arc<ops::Progress>,
    pub started: Instant,
    /// Why it stopped; it waits for Retry or discarding.
    pub failed: Option<String>,
    retry: Option<Retry>,
    /// Recent (time, bytes done) readings, for the speed.
    pub samples: std::collections::VecDeque<(Instant, u64)>,
}

impl Transfer {
    /// Bytes per second over the last few seconds, once there's enough to tell.
    pub fn speed(&self) -> Option<f64> {
        let (first, last) = (self.samples.front()?, self.samples.back()?);
        let secs = last.0.duration_since(first.0).as_secs_f64();
        (secs >= 1.0 && last.1 > first.1).then(|| (last.1 - first.1) as f64 / secs)
    }
}

pub struct Status {
    pub message: String,
    pub is_error: bool,
    /// More to see on click (e.g. files that couldn't be copied).
    pub details: Vec<String>,
    /// A source (by id) the user needs to sign in to: shows a Sign In button.
    pub sign_in: Option<String>,
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
    /// A row of color dots for the selected folders; `current` is ringed.
    Colors { current: Option<listing::TagColor> },
}

#[derive(Clone)]
pub enum MenuAction {
    Command(Command),
    RemoveFavorite(usize),
    OpenPathWith(PathBuf, String),
    RevealPath(PathBuf),
    Navigate(PathBuf),
    CopyPath(PathBuf),
    AddFavoritePath(PathBuf),
    NewWindowAt(PathBuf),
    /// Jump `steps` entries back (or forward) through history.
    History { back: bool, steps: usize },
    /// Color the selected folders (`None` clears it).
    SetColor(Option<listing::TagColor>),
    /// Create a folder (or file) inside `dir`, expand it and name the new item.
    NewIn { dir: PathBuf, folder: bool },
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
    /// The source (by id) whose adapter asked to sign in.
    pub sign_in: Option<String>,
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
    pub(crate) view: ViewMode,
    pub(crate) tile_size: f32,
    pub(crate) columns: ColumnWidths,
    /// A list column's edge is being dragged.
    pub(crate) column_drag: Option<ColumnDrag>,
    /// Tiles per row, as last laid out; arrow keys move by this much vertically.
    pub(crate) tile_columns: usize,
    /// The tile size slider is being dragged.
    pub(crate) tile_slider_drag: bool,
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
    /// Items copied (or cut) with ⌘C / ⌘X, waiting for Paste.
    pub(crate) file_clipboard: Vec<PathBuf>,
    /// The clipboard was filled by Cut: Paste moves instead of copying.
    pub(crate) clipboard_cut: bool,
    pub(crate) transfers: Vec<Transfer>,
    undo: Vec<UndoOp>,
    /// Redraws the status bar while transfers run.
    transfer_ticker: Option<Task<()>>,
    /// When drag auto-scrolling last moved the list; `None` when it isn't running.
    autoscroll_since: Option<Instant>,
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
    pub(crate) type_ahead: TypeAhead,
    pub(crate) last_title: String,
    /// Source files being fetched for the selection; kept after a failure so
    /// the same files aren't retried every frame.
    fetching: Option<(Vec<PathBuf>, Task<()>)>,
    /// Runs once the selection's source files are fetched (Open, Quick Look…).
    pub(crate) after_fetch: Option<Command>,
    /// Resolved preview paths for v2 source files (cache path → preview path).
    /// Populated eagerly after fetch completes; used by info panel and Quick Look.
    pub(crate) preview_paths_cache: HashMap<PathBuf, PathBuf>,
    /// Findest's own preview for YAML and other text Quick Look can't show.
    pub(crate) text_preview: Option<crate::text_preview::TextPreview>,
    /// The source being signed in to, and the flag that cancels it.
    pub(crate) signing_in: Option<(String, Arc<AtomicBool>)>,
    /// Go to's Tab matches, for the text they were found for.
    pub(crate) go_to_matches: Option<(String, Vec<String>)>,
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
        errors.extend(sources::global().set(config.sources.clone()).into_iter().map(|p| format!("Config: {p}")));
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
        let view = init.view.unwrap_or(config.general.view);
        let tile_size = init.tile_size.unwrap_or(config.general.tile_size).clamp(MIN_TILE, MAX_TILE);
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
            view,
            tile_size,
            columns: init.columns.unwrap_or_default(),
            column_drag: None,
            tile_columns: 1,
            tile_slider_drag: false,
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
            clipboard_cut: false,
            transfers: Vec::new(),
            undo: Vec::new(),
            transfer_ticker: None,
            autoscroll_since: None,
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
            type_ahead: TypeAhead::default(),
            last_title: String::new(),
            fetching: None,
            after_fetch: None,
            preview_paths_cache: HashMap::new(),
            text_preview: None,
            signing_in: None,
            go_to_matches: None,
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
            crate::search_service::set_config(config.search.clone(), cx);
        }
        let listing_changed = config.general != self.config.general || config.sources != self.config.sources;
        errors.extend(sources::global().set(config.sources.clone()));
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
        self.text_preview = None;
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
        self.discard_edit(cx);
        self.menu = None;
        self.entries.clear();
        self.depths.clear();
        self.icons.clear_thumbnails();
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
            view: Some(self.view),
            tile_size: Some(self.tile_size),
            columns: Some(self.columns),
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
        // Tiles show the folder's own items only; expansions are kept for the list.
        let expanded = if self.view == ViewMode::Tiles { HashSet::new() } else { self.expanded.clone() };
        let label = self.location_label();
        self.loading = true;
        let listing = cx.background_executor().spawn(async move {
            match &location {
                Location::Dir(dir) if let Some(id) = sources::global().orphan(dir) => Err(LoadError {
                    message: format!("There's no source \"{id}\" in the config any more"),
                    not_permitted: false,
                    sign_in: None,
                }),
                Location::Dir(dir) if sources::global().contains(dir) => {
                    let tree = sources::global().read_tree(dir, &expanded, show_hidden, order);
                    tree.map(|t| (t.rows, t.warnings)).map_err(|e| match e {
                        SourceError::NotSignedIn { id, name } => LoadError {
                            message: format!("Sign in to {name} to see this folder"),
                            not_permitted: false,
                            sign_in: Some(id),
                        },
                        SourceError::Failed(_) | SourceError::NotSupported { .. } | SourceError::Cancelled { .. } => {
                            LoadError { message: e.to_string(), not_permitted: false, sign_in: None }
                        }
                    })
                }
                Location::Dir(dir) => listing::read_tree(dir, &expanded, show_hidden, order).map(|rows| (rows, Vec::new())).map_err(|e| LoadError {
                    not_permitted: matches!(e.raw_os_error(), Some(1 /* EPERM */ | 13 /* EACCES */)),
                    message: format!("Can't open {label}: {e}"),
                    sign_in: None,
                }),
                Location::Recents => Ok((
                    recents
                        .iter()
                        .filter_map(|p| Entry::from_path(p).ok())
                        .map(|entry| listing::TreeRow { entry, depth: 0, has_children: false })
                        .collect(),
                    Vec::new(),
                )),
            }
        });
        // Replacing the task drops (cancels) any listing still in flight.
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = listing.await;
            this.update(cx, |this, cx| this.finish_load(result, cx)).ok();
        }));
        cx.notify();
    }

    fn finish_load(&mut self, result: Result<(Vec<listing::TreeRow>, Vec<String>), LoadError>, cx: &mut Context<Self>) {
        self.loading = false;
        self.load_error = None;
        self.preview_paths_cache.clear();
        match result {
            Ok((rows, warnings)) => {
                if let Some(first) = warnings.first() {
                    let more = warnings.len() - 1;
                    self.set_error(if more == 0 { first.clone() } else { format!("{first} (and {more} more)") }, cx);
                }
                self.entries = Vec::with_capacity(rows.len());
                self.depths = Vec::with_capacity(rows.len());
                self.expandable = Vec::with_capacity(rows.len());
                for row in rows {
                    self.entries.push(row.entry);
                    self.depths.push(row.depth);
                    self.expandable.push(row.has_children);
                }
            }
            Err(error) => {
                self.entries.clear();
                self.depths.clear();
                self.expandable.clear();
                self.load_error = Some(error);
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
            self.scroll.scroll_to_item(self.list_row(ix), ScrollStrategy::Top);
        }
        self.cursor = path;
    }

    /// The `uniform_list` row showing entry `ix` (tiles pack several per row).
    fn list_row(&self, ix: usize) -> usize {
        if self.view == ViewMode::Tiles { ix / self.tile_columns.max(1) } else { ix }
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

    /// Where Paste puts things: into the selected folder, next to the
    /// selected file, else the current folder. A selected folder that is
    /// being pasted (copied, still selected) gets its copy next to it.
    fn paste_destination(&self) -> Option<PathBuf> {
        paste_destination(self.single_selected_entry(), &self.file_clipboard, self.current_dir())
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
        self.type_ahead.query.push_str(&ch.to_lowercase());
        self.type_ahead_find(cx);
    }

    /// Selects the best match for the typed query and (re)starts the timer
    /// that clears it after a pause.
    fn type_ahead_find(&mut self, cx: &mut Context<Self>) {
        if !self.type_ahead.query.is_empty() {
            let names: Vec<(String, usize)> =
                self.entries.iter().enumerate().map(|(ix, e)| (e.display_name().to_lowercase(), self.depth(ix))).collect();
            let found = type_ahead_match(&self.type_ahead.query, &names, self.cursor_index());
            self.type_ahead.missed = found.is_none();
            if let Some(ix) = found {
                self.select_only(ix, cx);
            }
        }
        self.type_ahead.reset = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TYPE_AHEAD_RESET).await;
            this.update(cx, |this, cx| this.clear_type_ahead(cx)).ok();
        }));
        cx.notify();
    }

    fn clear_type_ahead(&mut self, cx: &mut Context<Self>) {
        if self.type_ahead.is_active() {
            self.type_ahead = TypeAhead::default();
            cx.notify();
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
            && !(ch == " " && !self.type_ahead.is_active())
        {
            self.type_ahead(ch, cx);
            cx.stop_propagation();
        }
    }

    // ---- Status -------------------------------------------------------

    pub fn set_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.status = Some(Status { message: message.into(), is_error: true, details: Vec::new(), sign_in: None });
        cx.notify();
    }

    pub fn set_info(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.status = Some(Status { message: message.into(), is_error: false, details: Vec::new(), sign_in: None });
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
        if self.type_ahead.is_active() {
            match command {
                // Backspace edits the query instead of trashing the match.
                TrashConfirm => {
                    self.type_ahead.query.pop();
                    self.type_ahead_find(cx);
                    return;
                }
                // Space continues the name being typed instead of previewing.
                QuickLook => return self.type_ahead(" ", cx),
                ClearSelection => return self.clear_type_ahead(cx),
                _ => self.clear_type_ahead(cx),
            }
        }
        if !matches!(command, SelectPrev | SelectNext | ExtendPrev | ExtendNext) {
            self.status = None;
        }
        if writes_files(&command) && self.in_source() {
            return self.set_error("Sources are read-only", cx);
        }
        if matches!(command, Open | QuickLook | OpenWith(_) | Copy) && !self.fetched(&self.selection(), cx) {
            self.after_fetch = Some(command);
            return self.set_info(format!("Fetching {}…", describe(&self.selection())), cx);
        }
        let tiles = self.view == ViewMode::Tiles;
        // In tiles, ↑/↓ move a whole row and ←/→ one tile.
        let row = if tiles { self.tile_columns.max(1) as isize } else { 1 };
        match command {
            SelectPrev => self.move_cursor(-row, false, cx),
            SelectNext => self.move_cursor(row, false, cx),
            ExtendPrev => self.move_cursor(-row, true, cx),
            ExtendNext => self.move_cursor(row, true, cx),
            Expand if tiles => self.move_cursor(1, false, cx),
            Collapse if tiles => self.move_cursor(-1, false, cx),
            ViewList => self.set_view(ViewMode::List, cx),
            ViewTiles => self.set_view(ViewMode::Tiles, cx),
            ZoomIn => self.zoom_tiles(TILE_ZOOM, cx),
            ZoomOut => self.zoom_tiles(1.0 / TILE_ZOOM, cx),
            SelectFirst => self.select_only(0, cx),
            SelectLast => self.select_only(self.entries.len().saturating_sub(1), cx),
            SelectAll => {
                self.selected = self.entries.iter().map(|e| e.path.clone()).collect();
                cx.notify();
            }
            ClearSelection if self.text_preview.is_some() => {
                self.text_preview = None;
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
            PauseIndexing => {
                crate::search_service::pause(None, cx);
                self.set_info("Indexing paused until you resume it or quit Findest", cx);
            }
            PauseIndexingHour => {
                crate::search_service::pause(Some(std::time::Duration::from_secs(60 * 60)), cx);
                self.set_info("Indexing paused for an hour", cx);
            }
            ResumeIndexing => {
                crate::search_service::resume(cx);
                self.set_info("Indexing resumed", cx);
            }
            UseFindestAsDefault => match crate::default_viewer::use_findest() {
                Ok(()) => self.set_info("“Show in Finder” in other apps now opens Findest. Undo: Findest menu › Use Finder as Default File Viewer", cx),
                Err(e) => self.set_error(format!("{e:#}"), cx),
            },
            UseFinderAsDefault => {
                let was = crate::default_viewer::is_default();
                match crate::default_viewer::use_finder() {
                    Ok(()) if was => self.set_info("Finder is the default file viewer again", cx),
                    Ok(()) => self.set_info("Finder is already the default file viewer", cx),
                    Err(e) => self.set_error(format!("{e:#}"), cx),
                }
            }
            ToggleIndexing => {
                let on = !cx.try_global::<crate::search_service::IndexService>().is_some_and(|s| s.enabled());
                match crate::search_service::set_enabled(on, cx) {
                    Ok(()) if on => self.set_info("Search indexing is on. Building the index…", cx),
                    Ok(()) => self.set_info("Search indexing is off. Turn it back on from the Findest menu", cx),
                    Err(e) => self.set_error(format!("Couldn't save the setting: {e:#}"), cx),
                }
            }
            QuickLook => {
                if !self.toggle_text_preview(cx) {
                    let selection = self.selection();
                    if self.in_source() && selection.len() == 1 {
                        let path = &selection[0];
                        let preview_path = self.preview_paths_cache.get(path).cloned().unwrap_or_else(|| path.clone());
                        crate::quick_look::toggle(window, vec![preview_path], cx);
                    } else {
                        crate::quick_look::toggle(window, selection, cx);
                    }
                }
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
                    |this, created, cx| {
                        this.set_info(format!("Duplicated {}", count_label(created.len())), cx);
                        this.record_copy(&created);
                        this.reload_and_select(created, false, cx);
                    },
                );
            }
            Compress => {
                let paths = self.selection();
                if paths.is_empty() {
                    return;
                }
                self.set_info(format!("Compressing {}…", describe(&paths)), cx);
                self.run_op(cx, move || ops::compress(&paths), |this, zip, cx| {
                    this.set_info(format!("Compressed to {}", display_name(&zip)), cx);
                    this.record_copy(std::slice::from_ref(&zip));
                    this.reload_and_select(vec![zip], false, cx);
                });
            }
            Extract => {
                let zips: Vec<PathBuf> = self.selection().into_iter().filter(|p| ops::is_zip(p)).collect();
                if zips.is_empty() {
                    return self.set_error("Select a .zip file to uncompress", cx);
                }
                self.extract(zips, cx);
            }
            Trash => self.trash(self.config.general.confirm_trash, window, cx),
            TrashConfirm => self.trash(true, window, cx),
            TrashNow => self.trash(false, window, cx),
            DeletePermanently => self.delete_permanently(window, cx),
            EmptyTrash => self.empty_trash(window, cx),
            Copy | Cut => {
                let paths = self.selection();
                if paths.is_empty() {
                    return;
                }
                // Nothing happens to the items until Paste; the status bar shows what's pending.
                self.file_clipboard = paths;
                self.clipboard_cut = command == Cut;
                cx.notify();
            }
            ClearClipboard => {
                self.file_clipboard.clear();
                self.clipboard_cut = false;
                cx.notify();
            }
            Paste | MoveHere => {
                let Some(dir) = self.paste_destination() else {
                    return self.set_error("Open a folder to paste into", cx);
                };
                if self.file_clipboard.is_empty() {
                    return self.set_error("Nothing to paste — copy (⌘C) or cut (⌘X) items first", cx);
                }
                let moving = command == MoveHere || self.clipboard_cut;
                let sources = self.file_clipboard.clone();
                self.transfer_into(sources, dir.clone(), moving, window, cx, move |this, created, cx| {
                    if moving {
                        // Moved items aren't where they were any more.
                        this.file_clipboard.clear();
                        this.clipboard_cut = false;
                    }
                    let n = count_label(created.len());
                    let here = this.current_dir().as_deref() == Some(dir.as_path());
                    let place = if here { "here".to_string() } else { format!("into “{}”", display_name(&dir)) };
                    this.set_info(format!("{} {n} {place}", if moving { "Moved" } else { "Pasted" }), cx);
                    // Pasted into a collapsed folder: keep that folder selected.
                    let visible = here || this.expanded.contains(&dir);
                    this.reload_and_select(if visible { created } else { vec![dir] }, false, cx);
                });
            }
            Undo => self.undo(cx),
            CopyPath => {
                let paths = self.selection_or_current();
                let text = paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.set_info(format!("Copied {}", if paths.len() == 1 { "path".into() } else { format!("{} paths", paths.len()) }), cx);
            }
            NewWindow => {
                let init = BrowserInit {
                    start: self.current_dir(),
                    info_panel: Some(self.config.general.info_panel),
                    view: Some(self.view),
                    tile_size: Some(self.tile_size),
                    columns: Some(self.columns),
                    ..Default::default()
                };
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
                if let Some(dir) = self.current_dir() {
                    sources::global().invalidate(&dir);
                }
                self.volumes = locations::volumes();
                self.cloud = locations::cloud_places(&self.home);
                self.icons.clear_paths();
                self.reload(cx);
            }
            AddFavorite => self.add_favorite(cx),
            ShowRecents => self.navigate(Location::Recents, true, cx),
            RevealInFinder => {
                for path in self.selection_or_current() {
                    self.report(crate::default_viewer::reveal_in_finder(&path), cx);
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
            MenuAction::NewIn { dir, folder } => self.new_in(dir, folder, cx),
            MenuAction::SetColor(color) => self.set_folder_color(color, cx),
            MenuAction::RemoveFavorite(ix) => {
                if ix < self.config.favorites.len() {
                    let mut favorites = self.config.favorites.clone();
                    favorites.remove(ix);
                    self.save_favorites(favorites, cx);
                }
            }
            MenuAction::OpenPathWith(path, opener) => self.open_with(&opener, &[path], cx),
            MenuAction::Navigate(path) => self.navigate_to_path(path, cx),
            MenuAction::CopyPath(path) => {
                cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
                self.set_info("Copied path", cx);
            }
            MenuAction::AddFavoritePath(path) => self.add_favorite_path(path, cx),
            MenuAction::NewWindowAt(path) => {
                let init = BrowserInit {
                    start: Some(path),
                    info_panel: Some(self.config.general.info_panel),
                    view: Some(self.view),
                    tile_size: Some(self.tile_size),
                    columns: Some(self.columns),
                    ..Default::default()
                };
                cx.defer(move |cx| {
                    crate::open_browser_window(init, cx);
                });
            }
            MenuAction::History { back, steps } => self.go_history(back, steps, cx),
            MenuAction::RevealPath(path) => {
                let result = crate::default_viewer::reveal_in_finder(&path);
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

    /// Runs a copy/move off the UI thread with progress in the status bar.
    fn run_transfer<T: Send + 'static>(
        &mut self,
        verb: &'static str,
        cx: &mut Context<Self>,
        op: impl Fn(&ops::Progress, bool) -> Result<T> + Send + Sync + 'static,
        done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        let progress = Arc::new(ops::Progress::default());
        self.transfers.push(Transfer {
            verb,
            progress: progress.clone(),
            started: Instant::now(),
            failed: None,
            retry: None,
            samples: Default::default(),
        });
        if self.transfer_ticker.is_none() {
            self.transfer_ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_millis(100)).await;
                    let running = this.update(cx, |this, cx| {
                        this.sample_transfers();
                        cx.notify();
                        !this.transfers.is_empty()
                    });
                    if !running.unwrap_or(false) {
                        break;
                    }
                }
                this.update(cx, |this, _| this.transfer_ticker = None).ok();
            }));
        }
        self.start_transfer(verb, Arc::new(op), progress, false, Box::new(done), cx);
    }

    /// Runs (or, with `resume`, continues) a transfer. If it fails for a
    /// reason other than cancelling, it stays in the status bar with Retry.
    #[allow(clippy::type_complexity)]
    fn start_transfer<T: Send + 'static>(
        &mut self,
        verb: &'static str,
        op: Arc<dyn Fn(&ops::Progress, bool) -> Result<T> + Send + Sync>,
        progress: Arc<ops::Progress>,
        resume: bool,
        done: Box<dyn FnOnce(&mut Self, T, &mut Context<Self>)>,
        cx: &mut Context<Self>,
    ) {
        let task = cx.background_executor().spawn({
            let (op, progress) = (op.clone(), progress.clone());
            async move { op(&progress, resume) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(value) => {
                        this.transfers.retain(|t| !Arc::ptr_eq(&t.progress, &progress));
                        done(this, value, cx);
                        this.report_skipped(&progress, cx);
                    }
                    Err(_) if progress.is_cancelled() => {
                        this.transfers.retain(|t| !Arc::ptr_eq(&t.progress, &progress));
                        this.set_info(if verb == "Moving" { "Move cancelled" } else { "Copy cancelled" }, cx);
                        this.reload(cx);
                    }
                    Err(e) => {
                        let retry_progress = progress.clone();
                        let retry: Retry = Box::new(move |this, cx| {
                            this.start_transfer(verb, op, retry_progress, true, done, cx);
                        });
                        if let Some(t) = this.transfers.iter_mut().find(|t| Arc::ptr_eq(&t.progress, &progress)) {
                            t.failed = Some(format!("{e:#}"));
                            t.retry = Some(retry);
                            t.samples.clear();
                        }
                        this.reload(cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Notes bytes done for each running transfer (keeps the last 5 s).
    fn sample_transfers(&mut self) {
        let now = Instant::now();
        for t in &mut self.transfers {
            if t.failed.is_some() || t.progress.is_paused() {
                t.samples.clear();
                continue;
            }
            t.samples.push_back((now, t.progress.done_bytes.load(std::sync::atomic::Ordering::Relaxed)));
            while t.samples.front().is_some_and(|(at, _)| now.duration_since(*at) > Duration::from_secs(5)) {
                t.samples.pop_front();
            }
        }
    }

    /// Adds files that couldn't be read to the finished transfer's message;
    /// clicking it lists them.
    fn report_skipped(&mut self, progress: &ops::Progress, cx: &mut Context<Self>) {
        let skipped = std::mem::take(&mut *progress.skipped.lock().unwrap());
        if skipped.is_empty() {
            return;
        }
        let message = self.status.take().map(|s| s.message).unwrap_or_default();
        let n = skipped.len();
        self.status = Some(Status {
            message: format!("{message} · {n} couldn't be read (click for details)"),
            is_error: true,
            details: skipped.iter().map(|(path, why)| format!("{}: {why}", contract_tilde(path))).collect(),
            sign_in: None,
        });
        cx.notify();
    }

    pub fn toggle_pause_transfers(&mut self, cx: &mut Context<Self>) {
        let pause = !self.transfers.iter().any(|t| t.progress.is_paused());
        for t in &self.transfers {
            t.progress.set_paused(pause);
        }
        cx.notify();
    }

    /// Continues the transfer that stopped.
    pub fn retry_transfer(&mut self, cx: &mut Context<Self>) {
        let Some(t) = self.transfers.iter_mut().find(|t| t.failed.is_some()) else { return };
        t.failed = None;
        if let Some(retry) = t.retry.take() {
            retry(self, cx);
        }
        cx.notify();
    }

    /// Gives up on the transfer that stopped, removing its partly copied item.
    pub fn discard_failed_transfer(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.transfers.iter().position(|t| t.failed.is_some()) else { return };
        let t = self.transfers.remove(ix);
        let progress = t.progress.clone();
        let verb = if t.verb == "Moving" { "Move" } else { "Copy" };
        cx.background_executor().spawn(async move { progress.discard_partial() }).detach();
        self.set_info(format!("{verb} abandoned"), cx);
        self.reload(cx);
    }

    fn push_undo(&mut self, op: UndoOp) {
        if self.undo.len() == MAX_UNDO {
            self.undo.remove(0);
        }
        self.undo.push(op);
    }

    fn record_copy(&mut self, created: &[PathBuf]) {
        let items = created.iter().map(|p| (p.clone(), std::fs::symlink_metadata(p).and_then(|m| m.modified()).ok())).collect();
        self.push_undo(UndoOp::Copy(items));
    }

    /// `moved` is `(from, to)` per item, as `ops::move_into` returns it.
    fn record_move(&mut self, moved: Vec<(PathBuf, PathBuf)>) {
        let pairs: Vec<(PathBuf, PathBuf)> = moved.into_iter().filter(|(from, to)| from != to).collect();
        if !pairs.is_empty() {
            self.push_undo(UndoOp::Move(pairs));
        }
    }

    /// Reverses the last copy (copies go to the Trash) or move (items go back).
    /// Refuses, changing nothing, if things changed since.
    fn undo(&mut self, cx: &mut Context<Self>) {
        let Some(op) = self.undo.pop() else {
            return self.set_info("Nothing to undo", cx);
        };
        let exists = |p: &Path| std::fs::symlink_metadata(p).is_ok();
        match op {
            UndoOp::Copy(items) => {
                for (path, modified) in &items {
                    match std::fs::symlink_metadata(path) {
                        Err(_) => return self.set_error(format!("Can't undo: “{}” is gone", display_name(path)), cx),
                        Ok(meta) if meta.modified().ok() != *modified => {
                            return self.set_error(format!("Can't undo: “{}” changed after it was copied", display_name(path)), cx);
                        }
                        Ok(_) => {}
                    }
                }
                let paths: Vec<PathBuf> = items.into_iter().map(|(p, _)| p).collect();
                let n = paths.len();
                self.run_op(cx, move || ops::move_to_trash(&paths), move |this, (), cx| {
                    this.set_info(format!("Undid copy: moved {} to the Trash", count_label(n)), cx);
                    this.reload(cx);
                });
            }
            UndoOp::Move(pairs) => {
                for (original, current) in &pairs {
                    if !exists(current) {
                        return self.set_error(format!("Can't undo: “{}” was moved or renamed since", display_name(current)), cx);
                    }
                    if exists(original) {
                        return self.set_error(
                            format!("Can't undo: something named “{}” is in its old folder", display_name(original)),
                            cx,
                        );
                    }
                }
                let n = pairs.len();
                let back: Vec<PathBuf> = pairs.iter().map(|(original, _)| original.clone()).collect();
                self.run_transfer(
                    "Moving",
                    cx,
                    move |progress, _resume| {
                        for (original, current) in &pairs {
                            if !std::fs::symlink_metadata(current).is_ok() && std::fs::symlink_metadata(original).is_ok() {
                                continue; // put back before a retry
                            }
                            let dir = original.parent().context("no parent folder")?;
                            ops::move_into(std::slice::from_ref(current), dir, ops::OnConflict::Fail, progress)?;
                        }
                        Ok(())
                    },
                    move |this, (), cx| {
                        this.set_info(format!("Undid move: put {} back", count_label(n)), cx);
                        // Still showing where they were moved from: select them again.
                        this.reload_and_select(back, false, cx);
                    },
                );
            }
        }
    }

    /// Cut items are shown dimmed until they're pasted.
    pub fn is_cut(&self, path: &Path) -> bool {
        self.clipboard_cut && self.file_clipboard.iter().any(|p| p == path)
    }

    pub fn cancel_transfers(&mut self, cx: &mut Context<Self>) {
        for transfer in &self.transfers {
            transfer.progress.cancel();
        }
        cx.notify();
    }

    /// While a drag hovers near the list's top or bottom edge, keeps scrolling
    /// it (faster closer to the edge) until the pointer moves away or the drag
    /// ends. Runs once per frame, scaled by elapsed time, so it stays smooth.
    pub fn drag_autoscroll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.autoscroll_since.is_some() || self.autoscroll_speed(window.mouse_position()) == 0.0 {
            return;
        }
        self.autoscroll_since = Some(Instant::now());
        cx.on_next_frame(window, Self::autoscroll_frame);
        cx.notify();
    }

    fn autoscroll_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let speed = self.autoscroll_speed(window.mouse_position());
        let Some(last) = self.autoscroll_since.filter(|_| cx.has_active_drag() && speed != 0.0) else {
            self.autoscroll_since = None;
            return;
        };
        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f32().min(0.05);
        self.autoscroll_since = Some(now);
        let handle = self.scroll.0.borrow().base_handle.clone();
        let mut offset = handle.offset();
        offset.y = (offset.y - gpui::px(speed * dt)).clamp(-handle.max_offset().height, gpui::px(0.));
        handle.set_offset(offset);
        cx.on_next_frame(window, Self::autoscroll_frame);
        cx.notify();
    }

    /// Points per second to scroll for a drag at `position`: negative scrolls up.
    fn autoscroll_speed(&self, position: Point<Pixels>) -> f32 {
        const EDGE: f32 = 40.0;
        let bounds = self.scroll.0.borrow().base_handle.bounds();
        if !bounds.contains(&position) {
            return 0.0;
        }
        let from_top = f32::from(position.y - bounds.top());
        let from_bottom = f32::from(bounds.bottom() - position.y);
        let ramp = |d: f32| 150.0 + 1350.0 * (1.0 - d / EDGE);
        if from_top < EDGE {
            -ramp(from_top)
        } else if from_bottom < EDGE {
            ramp(from_bottom)
        } else {
            0.0
        }
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
            && sources::global().is_more_row(&entry.path)
            && let Some(dir) = entry.path.parent()
        {
            return self.load_more(dir.to_path_buf(), cx);
        }
        if let [entry] = entries.as_slice()
            && entry.is_dir
        {
            return self.navigate(Location::Dir(entry.path.clone()), true, cx);
        }
        // Zips are unpacked here rather than handed to Archive Utility (which shows Finder).
        let zips: Vec<PathBuf> = entries.iter().filter(|e| ops::is_zip(&e.path)).map(|e| e.path.clone()).collect();
        if !zips.is_empty() {
            self.extract(zips, cx);
        }
        for entry in entries.iter().filter(|e| !e.is_dir && !ops::is_zip(&e.path)) {
            let result = launch::open_default(&entry.path);
            if result.is_ok() {
                self.remember(&entry.path);
            }
            self.report(result, cx);
        }
        self.save_recents(cx);
    }

    fn extract(&mut self, zips: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.set_info(format!("Uncompressing {}…", describe(&zips)), cx);
        self.run_op(cx, move || zips.iter().map(|z| ops::extract(z)).collect::<Result<Vec<_>>>(), |this, created, cx| {
            let names = describe(&created);
            this.set_info(format!("Uncompressed to {names}"), cx);
            this.record_copy(&created);
            this.reload_and_select(created, false, cx);
        });
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

    /// Shows a folder, or a file (or app bundle) selected in its folder, as
    /// for "Show in Finder" from other apps.
    pub fn navigate_to_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let package = Entry::from_path(&path).is_ok_and(|e| e.is_package);
        if path.is_dir() && !package {
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
        self.add_favorite_path(target, cx);
    }

    fn add_favorite_path(&mut self, target: PathBuf, cx: &mut Context<Self>) {
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
        // Selected, so pasting or typing a path replaces it (→ extends it instead).
        let len = text.len();
        self.start_edit(EditKind::GoToFolder, text, 0..len, "Path, e.g. ~/Projects", window, cx);
    }

    /// Tab in Go to: completes the path like a terminal.
    fn complete_go_to(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = &self.edit else { return };
        let input = edit.input.clone();
        let done = findest_core::paths::complete_path(input.read(cx).text());
        let len = done.text.len();
        self.go_to_matches = done.candidates.map(|c| (done.text.clone(), c));
        input.update(cx, |input, cx| input.set_text(done.text, len..len, cx));
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
                TextInputEvent::Tab if matches!(this.edit.as_ref().map(|e| &e.kind), Some(EditKind::GoToFolder)) => {
                    this.complete_go_to(cx)
                }
                TextInputEvent::Up | TextInputEvent::Down | TextInputEvent::Tab => {}
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
    /// Colors the selected folders (Finder's color label / tag).
    fn set_folder_color(&mut self, color: Option<listing::TagColor>, cx: &mut Context<Self>) {
        let folders: Vec<PathBuf> =
            self.entries.iter().filter(|e| e.is_dir && self.selected.contains(&e.path)).map(|e| e.path.clone()).collect();
        let errors: Vec<String> = folders
            .iter()
            .filter_map(|p| crate::system::set_color(p, color).err().map(|e| format!("{}: {e:#}", display_name(p))))
            .collect();
        if errors.is_empty() {
            let what = if folders.len() == 1 { format!("“{}”", display_name(&folders[0])) } else { count_label(folders.len()) };
            match color {
                Some(c) => self.set_info(format!("Colored {what} {}", c.name().to_lowercase()), cx),
                None => self.set_info(format!("Removed the color from {what}"), cx),
            }
        } else {
            self.set_error(format!("Couldn't set the color: {}", errors.join("; ")), cx);
        }
        self.reload(cx);
    }

    /// The icon shown next to the folder name in the toolbar.
    pub fn title_icon_key(&self) -> Option<IconKey> {
        match &self.location {
            Location::Dir(dir) => Entry::from_path(dir).ok().map(|e| IconKey::for_entry(&e, &self.home)),
            Location::Recents => Some(self.symbol_key("clock")),
        }
    }

    /// The folders above the current one, nearest first, to jump to.
    pub fn open_enclosing_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(dir) = self.current_dir() else { return };
        let entries: Vec<MenuEntry> = locations::breadcrumbs(&self.home, &dir)
            .into_iter()
            .rev()
            .skip(1)
            .map(|place| MenuEntry::Item {
                label: place.label,
                detail: Some(contract_tilde(&place.path)),
                action: MenuAction::Navigate(place.path),
                danger: false,
            })
            .collect();
        if entries.is_empty() {
            return;
        }
        self.menu = Some(ContextMenu { position, entries });
        cx.notify();
    }

    /// Lists the status message's details (e.g. files that couldn't be copied).
    pub fn open_status_details(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(status) = &self.status else { return };
        const SHOWN: usize = 30;
        let mut entries: Vec<MenuEntry> = status.details.iter().take(SHOWN).map(|d| MenuEntry::Header(d.clone())).collect();
        if status.details.len() > SHOWN {
            entries.push(MenuEntry::Header(format!("and {} more", status.details.len() - SHOWN)));
        }
        self.menu = Some(ContextMenu { position, entries });
        cx.notify();
    }

    /// Pause / resume / rebuild / turn off the search index.
    pub fn open_index_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        use Command::*;
        let Some(service) = cx.try_global::<crate::search_service::IndexService>() else { return };
        let mut entries = Vec::new();
        if !service.enabled() {
            entries.push(self.item("Turn On Search Indexing", ToggleIndexing, "toggle_indexing"));
        } else {
            entries.push(MenuEntry::Header(crate::search_service::status_text(service)));
            if service.is_paused() {
                entries.push(self.item("Resume Indexing", ResumeIndexing, "resume_indexing"));
            } else {
                entries.push(self.item("Pause Indexing for an Hour", PauseIndexingHour, "pause_indexing_hour"));
                entries.push(self.item("Pause Indexing", PauseIndexing, "pause_indexing"));
            }
            if !service.building {
                entries.push(self.item("Rebuild Index Now", RebuildIndex, "rebuild_index"));
            }
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item {
                label: "Turn Off Search Indexing".into(),
                detail: None,
                action: MenuAction::Command(ToggleIndexing),
                danger: true,
            });
        }
        self.menu = Some(ContextMenu { position, entries });
        cx.notify();
    }

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
            if let [folder] = &selection[..]
                && folder.is_dir
                && !sources::global().contains(&folder.path)
            {
                for (label, is_folder) in [("New Folder", true), ("New File", false)] {
                    let action = MenuAction::NewIn { dir: folder.path.clone(), folder: is_folder };
                    entries.push(MenuEntry::Item { label: label.into(), detail: None, action, danger: false });
                }
                entries.push(MenuEntry::Separator);
            }
            if single {
                entries.push(self.item("Rename", Rename, "rename"));
            }
            entries.push(self.item("Duplicate", Duplicate, "duplicate"));
            let compress = match &selection[..] {
                [one] => format!("Compress “{}”", one.display_name()),
                many => format!("Compress {}", count_label(many.len())),
            };
            entries.push(self.item(compress, Compress, "compress"));
            if selection.iter().any(|e| ops::is_zip(&e.path)) {
                entries.push(self.item("Uncompress", Extract, "extract"));
            }
            entries.push(self.item("Cut", Cut, "cut"));
            entries.push(self.item("Copy", Copy, "copy"));
            if let Some(folder) = self.paste_destination().filter(|_| single && all_dirs && !self.file_clipboard.is_empty()) {
                let (n, verb) = (count_label(self.file_clipboard.len()), if self.clipboard_cut { "Move" } else { "Paste" });
                if self.single_selected_entry().is_some_and(|e| e.path == folder) {
                    entries.push(self.item(format!("{verb} {n} Into “{}”", display_name(&folder)), Paste, "paste"));
                }
            }
            entries.push(self.item(if single { "Copy Path" } else { "Copy Paths" }, CopyPath, "copy_path"));
            if single && all_dirs {
                entries.push(self.item("Add to Favorites", AddFavorite, "add_favorite"));
            }
            entries.push(self.item("Reveal in Finder", RevealInFinder, "reveal_in_finder"));
            if all_dirs {
                // Ring the color they share, if any.
                let colors: HashSet<Option<listing::TagColor>> = selection.iter().map(|e| e.color).collect();
                let current = if colors.len() == 1 { colors.into_iter().next().flatten() } else { None };
                entries.push(MenuEntry::Separator);
                entries.push(MenuEntry::Colors { current });
            }
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
                    if self.clipboard_cut {
                        entries.push(self.item(format!("Move {n} Here"), Paste, "paste"));
                    } else {
                        entries.push(self.item(format!("Paste {n}"), Paste, "paste"));
                        entries.push(self.item(format!("Move {n} Here"), MoveHere, "move_here"));
                    }
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

    /// Menu for a folder in the path bar.
    pub fn open_path_menu(&mut self, position: Point<Pixels>, path: PathBuf, cx: &mut Context<Self>) {
        let item = |label: &str, action: MenuAction| MenuEntry::Item { label: label.into(), detail: None, action, danger: false };
        let mut entries: Vec<MenuEntry> = self
            .config
            .openers
            .iter()
            .filter(|o| o.applies_to(true))
            .map(|o| item(&format!("Open in {}", o.name), MenuAction::OpenPathWith(path.clone(), o.name.clone())))
            .collect();
        if !entries.is_empty() {
            entries.push(MenuEntry::Separator);
        }
        entries.push(item("Open in New Window", MenuAction::NewWindowAt(path.clone())));
        entries.push(item("Copy Path", MenuAction::CopyPath(path.clone())));
        if !self.config.favorites.iter().any(|f| f.resolved_path() == path) {
            entries.push(item("Add to Favorites", MenuAction::AddFavoritePath(path.clone())));
        }
        entries.push(item("Reveal in Finder", MenuAction::RevealPath(path)));
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
    pub fn maybe_start_drag(&mut self, event: &gpui::MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
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
        if paths.is_empty() {
            return;
        }
        let handle = self.scroll.0.borrow().base_handle.clone();
        let start_offset = handle.offset();
        if let Some(ended) = crate::native_drag::start_file_drag(window, &paths) {
            // Cancelled (Esc) or dropped nowhere: scroll back to where the dragged items are.
            cx.spawn(async move |this, cx| {
                if ended.await == Ok(false) {
                    this.update(cx, |_, cx| {
                        handle.set_offset(start_offset);
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
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

    pub fn set_view(&mut self, view: ViewMode, cx: &mut Context<Self>) {
        if self.view == view {
            return;
        }
        self.view = view;
        // Keep the selection in view once the new layout arrives.
        self.pending_select = Some(PendingSelect { paths: self.selection(), name_new: false });
        self.save_session(cx);
        self.reload(cx);
    }

    fn zoom_tiles(&mut self, factor: f32, cx: &mut Context<Self>) {
        if self.view != ViewMode::Tiles {
            return self.set_view(ViewMode::Tiles, cx);
        }
        self.set_tile_size(self.tile_size * factor, cx);
        self.save_session(cx);
    }

    /// Resizes tiles (slider drags call this per move; the session is saved on release).
    pub fn set_tile_size(&mut self, size: f32, cx: &mut Context<Self>) {
        let size = size.clamp(MIN_TILE, MAX_TILE).round();
        if size != self.tile_size {
            self.tile_size = size;
            if let Some(ix) = self.cursor_index() {
                self.scroll.scroll_to_item(self.list_row(ix), ScrollStrategy::Center);
            }
            cx.notify();
        }
    }

    pub fn tile_size_committed(&mut self, cx: &mut Context<Self>) {
        self.save_session(cx);
    }

    pub fn column_width(&self, column: Column) -> f32 {
        match column {
            Column::Name => self.columns.name,
            Column::Modified => self.columns.modified,
            Column::Size => self.columns.size,
            Column::Kind => self.columns.kind,
        }
    }

    fn set_column_width(&mut self, column: Column, width: f32, cx: &mut Context<Self>) {
        let width = width.clamp(ColumnWidths::MIN, ColumnWidths::MAX).round();
        let slot = match column {
            Column::Name => &mut self.columns.name,
            Column::Modified => &mut self.columns.modified,
            Column::Size => &mut self.columns.size,
            Column::Kind => &mut self.columns.kind,
        };
        if *slot != width {
            *slot = width;
            cx.notify();
        }
    }

    pub fn start_column_drag(&mut self, column: Column, x: Pixels) {
        self.column_drag = Some(ColumnDrag { column, start_x: x, start_width: self.column_width(column) });
    }

    /// Follows the mouse while a column edge is dragged (the session is saved on release).
    pub fn drag_column(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let Some(drag) = &self.column_drag else { return };
        let (column, width) = (drag.column, drag.start_width + f32::from(x - drag.start_x));
        self.set_column_width(column, width, cx);
    }

    pub fn end_column_drag(&mut self, cx: &mut Context<Self>) {
        if self.column_drag.take().is_some() {
            self.save_session(cx);
        }
    }

    /// Double-clicking a column edge restores its default width.
    pub fn reset_column(&mut self, column: Column, cx: &mut Context<Self>) {
        let defaults = ColumnWidths::default();
        let width = match column {
            Column::Name => defaults.name,
            Column::Modified => defaults.modified,
            Column::Size => defaults.size,
            Column::Kind => defaults.kind,
        };
        self.set_column_width(column, width, cx);
        self.save_session(cx);
    }

    /// The picture for tile `ix`: the image itself for image files (once its
    /// thumbnail is ready), else its icon, sharper for big tiles.
    pub fn tile_image(&mut self, ix: usize, cx: &mut Context<Self>) -> Option<std::sync::Arc<gpui::Image>> {
        let entry = self.entries.get(ix)?;
        let icon = IconKey::for_entry(entry, &self.home);
        let icon = if self.tile_size > 64.0 { IconKey::Large(Box::new(icon)) } else { icon };
        let thumbnail = !entry.is_dir && entry.size <= MAX_THUMBNAIL_BYTES && is_image_path(&entry.path);
        let (path, modified) = (entry.path.clone(), entry.modified);
        if thumbnail {
            // Retina pixels, in a few buckets so resizing doesn't re-render every step.
            const BUCKETS: [u32; 3] = [128, 256, 512];
            let pixels = BUCKETS.into_iter().find(|&p| p as f32 >= self.tile_size * 2.0).unwrap_or(512);
            let key = |pixels| IconKey::Thumbnail { path: path.clone(), modified, pixels };
            // While resizing, show another size until this one is ready.
            let fallback = BUCKETS.into_iter().rev().find_map(|p| self.icons.peek(&key(p)));
            if let Some(thumbnail) = self.request_icon(key(pixels), cx).or(fallback) {
                return Some(thumbnail);
            }
        }
        self.request_icon(icon, cx)
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
    pub fn drop_paths(&mut self, paths: Vec<PathBuf>, dest: PathBuf, force_copy: bool, window: &mut Window, cx: &mut Context<Self>) {
        if sources::global().contains(&dest) {
            return self.set_error("Sources are read-only", cx);
        }
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
        self.transfer_into(paths, dest, !copy, window, cx, move |this, created, cx| {
            let verb = if copy { "Copied" } else { "Moved" };
            let skipped = n - created.len();
            let skipped = if skipped > 0 { format!(" ({skipped} skipped)") } else { String::new() };
            this.set_info(format!("{verb} {} to {dest_name}{skipped}", count_label(created.len())), cx);
            this.reload(cx);
        });
    }

    /// Copies or moves `sources` into `dest`, first asking what to do about
    /// names already taken there (copying an item into its own folder never
    /// asks: the copy is named "… copy"). Records it for Undo, then calls
    /// `done` with what was created.
    fn transfer_into(
        &mut self,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        moving: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, Vec<PathBuf>, &mut Context<Self>) + 'static,
    ) {
        let clashes = ops::conflicts(&sources, &dest);
        let total = sources.len();
        let dest_name = display_name(&dest);
        let start = move |this: &mut Self, on_conflict: ops::OnConflict, cx: &mut Context<Self>| {
            // Planned once; a retry continues the same plan.
            let planned: std::sync::Mutex<Option<Vec<ops::Planned>>> = Default::default();
            this.run_transfer(
                if moving { "Moving" } else { "Copying" },
                cx,
                move |progress, resume| {
                    let mut planned = planned.lock().unwrap();
                    if planned.is_none() {
                        *planned = Some(ops::plan(&sources, &dest, !moving, on_conflict)?);
                    }
                    let items = planned.as_ref().expect("just planned");
                    ops::transfer(items, moving, resume, progress)?;
                    // Copies have no "from" to undo back to.
                    Ok(items.iter().map(|(from, to)| (if moving { from.clone() } else { PathBuf::new() }, to.clone())).collect())
                },
                move |this, moved: Vec<(PathBuf, PathBuf)>, cx| {
                    let created: Vec<PathBuf> = moved.iter().map(|(_, to)| to.clone()).collect();
                    if moving { this.record_move(moved) } else { this.record_copy(&created) }
                    done(this, created, cx);
                },
            );
        };
        if clashes.is_empty() {
            return start(self, ops::OnConflict::Fail, cx);
        }

        let message = match clashes.as_slice() {
            [one] => format!("“{}” already exists in “{dest_name}”", display_name(one)),
            many => format!("{} items already exist in “{dest_name}”", many.len()),
        };
        let mut detail = if clashes.len() == 1 {
            "Replace moves the existing item to the Trash.".to_string()
        } else {
            let mut names: Vec<String> = clashes.iter().take(5).map(|p| display_name(p)).collect();
            if clashes.len() > 5 {
                names.push(format!("and {} more", clashes.len() - 5));
            }
            format!("{}\n\nReplace moves the existing items to the Trash.", names.join(", "))
        };
        if !moving {
            detail.push_str(" Keep Both names the copies “… 2”.");
        }
        // Skipping only makes sense when other items can still go.
        let answers: Vec<&'static str> =
            if total > clashes.len() { vec!["Replace", "Keep Both", "Skip These", "Stop"] } else { vec!["Replace", "Keep Both", "Stop"] };
        let answer = window.prompt(PromptLevel::Warning, &message, Some(&detail), &answers, cx);
        cx.spawn(async move |this, cx| {
            let Ok(i) = answer.await else { return };
            let on_conflict = match answers.get(i).copied() {
                Some("Replace") => ops::OnConflict::Replace,
                Some("Keep Both") => ops::OnConflict::KeepBoth,
                Some("Skip These") => ops::OnConflict::Skip,
                _ => return,
            };
            this.update(cx, |this, cx| start(this, on_conflict, cx)).ok();
        })
        .detach();
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
        let Some(dir) = self.current_dir().filter(|d| !sources::global().contains(d)) else {
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
        // For v2 source files use the resolved preview path for thumbnail/preview rendering.
        let preview_path = self.preview_paths_cache.get(&path).cloned().unwrap_or_else(|| path.clone());
        // Folders and bundles (apps) get their total size measured.
        let is_dir = path.is_dir();
        let is_image = !is_dir && is_image_path(&preview_path);
        let icon_key = Entry::from_path(&path).ok().map(|e| IconKey::for_entry(&e, &self.home));
        if let Some(key) = icon_key.clone() {
            self.request_icon(key, cx);
        }
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let kind = if is_dir {
            PreviewKind::Folder
        } else if is_image {
            if gpui_decodes(&preview_path) { PreviewKind::Image } else { PreviewKind::RenderedImage }
        } else if is_pdf(&preview_path) {
            PreviewKind::RenderedImage
        } else {
            PreviewKind::Text
        };
        let task = {
            let (path, cancel) = (preview_path.clone(), cancel.clone());
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
        if let Location::Dir(root) = &self.location
            && !sources::global().contains(root)
        {
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

impl Browser {
    /// True when showing a folder in a source.
    pub(crate) fn in_source(&self) -> bool {
        self.current_dir().is_some_and(|d| sources::global().contains(&d))
    }

    /// True when every source file in `paths` is fetched to its cache path;
    /// otherwise starts fetching them and redraws when done.
    pub(crate) fn fetched(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) -> bool {
        let wanted: Vec<PathBuf> = paths.iter().filter(|p| sources::global().needs_fetch(p)).cloned().collect();
        if wanted.is_empty() {
            return true;
        }
        if self.fetching.as_ref().is_some_and(|(p, _)| *p == wanted) {
            return false;
        }
        let job = wanted.clone();
        let task = cx.background_executor().spawn(async move { sources::global().fetch_all(&job) });
        let watch = cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.fetching = None;
                        // For v2 source files, eagerly resolve preview paths so the
                        // info panel thumbnail and Quick Look both use the right file.
                        if this.in_source() {
                            let paths = this.selection();
                            let v2_paths: Vec<PathBuf> = paths.into_iter()
                                .filter(|p| !p.is_dir() && sources::global().source_at(p).is_some_and(|s| s.protocol_version == 2))
                                .collect();
                            if !v2_paths.is_empty() {
                                let task = cx.background_executor().spawn(async move {
                                    v2_paths.into_iter().filter_map(|p| {
                                        let preview = sources::global().preview_path(&p).ok()?;
                                        Some((p, preview))
                                    }).collect::<Vec<_>>()
                                });
                                cx.spawn(async move |this, cx| {
                                    let resolved = task.await;
                                    this.update(cx, |this, cx| {
                                        for (cache, preview) in resolved {
                                            this.preview_paths_cache.insert(cache, preview);
                                        }
                                        this.info = None;
                                        cx.notify();
                                    }).ok();
                                }).detach();
                            }
                        }
                    }
                    Err(e) => {
                        this.after_fetch = None;
                        this.report_source_error(e, cx);
                    }
                }
                // The info panel read the file before it arrived.
                this.info = None;
                cx.notify();
            })
            .ok();
        });
        self.fetching = Some((wanted, watch));
        false
    }

    /// New folder or file inside `dir` (not the current folder): `dir` is
    /// expanded (opened, in tiles) and the new item is named, as with ⇧⌘N.
    fn new_in(&mut self, dir: PathBuf, folder: bool, cx: &mut Context<Self>) {
        let target = dir.clone();
        self.run_op(
            cx,
            move || if folder { ops::create_folder(&target) } else { ops::create_file(&target) },
            move |this, path, cx| {
                if this.view == ViewMode::Tiles {
                    this.navigate(Location::Dir(dir), true, cx);
                    this.pending_select = Some(PendingSelect { paths: vec![path], name_new: true });
                } else {
                    this.expanded.insert(dir);
                    this.save_session(cx);
                    this.reload_and_select(vec![path], true, cx);
                }
            },
        );
    }

    /// Lists the next page of a paged source folder ("Load more…").
    fn load_more(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        self.set_info("Loading more…", cx);
        let task = cx.background_executor().spawn(async move { sources::global().load_more(&dir) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.status = None,
                    Err(e) => this.report_source_error(e, cx),
                }
                this.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Runs the adapter's sign-in, then lists the folder again.
    pub(crate) fn sign_in(&mut self, id: String, cx: &mut Context<Self>) {
        if self.signing_in.is_some() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.signing_in = Some((id.clone(), cancel.clone()));
        self.status = None;
        cx.notify();
        let task = cx.background_executor().spawn(async move { sources::global().login(&id, &cancel) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.signing_in = None;
                match result {
                    Ok(()) => this.set_info("Signed in", cx),
                    Err(e) => this.report_source_error(e, cx),
                }
                // Lists the folder (or runs the search) again.
                this.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Stops a sign-in that's waiting (e.g. for the browser).
    pub(crate) fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        if let Some((_, cancel)) = &self.signing_in {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        cx.notify();
    }

    /// Shows a source's error in the status bar; "not signed in" gets a
    /// Sign In button.
    pub(crate) fn report_source_error(&mut self, e: SourceError, cx: &mut Context<Self>) {
        match e {
            SourceError::NotSignedIn { ref id, .. } => {
                let sign_in = Some(id.clone());
                self.status = Some(Status { message: e.to_string(), is_error: true, details: Vec::new(), sign_in });
                cx.notify();
            }
            SourceError::Cancelled { .. } => self.set_info(e.to_string(), cx),
            _ => self.set_error(e.to_string(), cx),
        }
    }
}

/// Commands that change files, refused inside a (read-only) source.
fn writes_files(command: &Command) -> bool {
    use Command::*;
    matches!(
        command,
        NewFolder | NewFile | Duplicate | Compress | Extract | Trash | TrashConfirm | TrashNow | DeletePermanently | Cut | Paste | MoveHere | Rename
    )
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

    fn names(list: &[(&str, usize)]) -> Vec<(String, usize)> {
        list.iter().map(|(n, d)| (n.to_string(), *d)).collect()
    }

    fn entry(path: &str, is_dir: bool) -> Entry {
        Entry {
            name: display_name(Path::new(path)),
            path: PathBuf::from(path),
            is_dir,
            is_package: false,
            is_symlink: false,
            is_hidden: false,
            size: 0,
            modified: None,
            color: None,
            label: None,
            mime_type: None,
        }
    }

    #[test]
    fn transfer_speed_uses_recent_samples() {
        let start = Instant::now();
        let mut t = Transfer {
            verb: "Copying",
            progress: Arc::default(),
            started: start,
            failed: None,
            retry: None,
            samples: Default::default(),
        };
        assert_eq!(t.speed(), None);
        t.samples.push_back((start, 0));
        t.samples.push_back((start + Duration::from_millis(500), 50));
        assert_eq!(t.speed(), None, "under a second: too early to tell");
        t.samples.push_back((start + Duration::from_secs(2), 2_000_000));
        assert_eq!(t.speed(), Some(1_000_000.0));
    }

    #[test]
    fn paste_goes_into_a_selected_folder_and_next_to_a_selected_file() {
        let here = Some(PathBuf::from("/w"));
        let clip = [PathBuf::from("/elsewhere/a.txt")];
        let dest = |selected: Option<&Entry>, clip: &[PathBuf]| paste_destination(selected, clip, here.clone());
        assert_eq!(dest(None, &clip), here, "nothing selected: the current folder");
        assert_eq!(dest(Some(&entry("/w/Docs", true)), &clip), Some(PathBuf::from("/w/Docs")), "into the folder");
        assert_eq!(dest(Some(&entry("/w/notes.txt", false)), &clip), here, "next to the file");
        assert_eq!(
            dest(Some(&entry("/w/Docs/inner.txt", false)), &clip),
            Some(PathBuf::from("/w/Docs")),
            "next to a file inside an expanded folder"
        );
        assert_eq!(dest(Some(&entry("/w/Tool.app", false)), &clip), here, "packages count as files");
        let copied_folder = [PathBuf::from("/w/Docs")];
        assert_eq!(dest(Some(&entry("/w/Docs", true)), &copied_folder), here, "a copied folder still selected: next to it");
        assert_eq!(paste_destination(None, &clip, None), None, "search results with nothing selected");
    }

    #[test]
    fn type_ahead_prefers_prefix_in_current_folder() {
        let names = names(&[("apps", 0), ("photos", 1), ("pictures", 0), ("zip", 0)]);
        assert_eq!(type_ahead_match("p", &names, None), Some(2));
        assert_eq!(type_ahead_match("pho", &names, None), Some(1));
        assert_eq!(type_ahead_match("ip", &names, None), Some(3));
        assert_eq!(type_ahead_match("q", &names, None), None);
    }

    #[test]
    fn type_ahead_repeated_letter_cycles() {
        let names = names(&[("pa", 0), ("pb", 0), ("pc", 0)]);
        assert_eq!(type_ahead_match("pp", &names, Some(0)), Some(1));
        assert_eq!(type_ahead_match("ppp", &names, Some(2)), Some(0));
        // A real name with the repeated letters wins over cycling.
        let names = self::names(&[("pa", 0), ("ppt", 0)]);
        assert_eq!(type_ahead_match("pp", &names, Some(0)), Some(1));
    }

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
