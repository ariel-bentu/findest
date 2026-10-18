//! Search mode for a browser window: the query field, scope, and running
//! queries against the shared index. Results replace the folder listing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use findest_core::listing::Entry;
use findest_core::search::{self, Excludes, LiveProgress, SearchOptions};
use findest_core::sources;
use gpui::{AppContext, Context, Entity, Focusable, ScrollStrategy, Subscription, Task, Window};

use crate::browser::{Browser, Location};
use crate::search_service::IndexService;
use crate::text_input::{TextInput, TextInputEvent};

pub struct SearchUi {
    pub input: Entity<TextInput>,
    /// Search all indexed folders rather than just the current one.
    pub everywhere: bool,
    pub error: Option<String>,
    /// The error is "not signed in" to this source (by id): offer Sign In.
    pub sign_in: Option<String>,
    /// Extra context, e.g. why the scope was widened.
    pub note: Option<String>,
    pub running: bool,
    /// Matches found (results shown may be capped).
    pub total: usize,
    /// Files read so far for `content:`.
    pub scanned: Arc<AtomicUsize>,
    pub has_content: bool,
    /// The results shown are for the current query.
    has_results: bool,
    /// Always search the disk directly instead of the index.
    pub live: bool,
    /// The running (or last) search walks the disk; its progress.
    pub live_progress: Option<Arc<LiveProgress>>,
    cancel: Arc<AtomicBool>,
    _task: Option<Task<()>>,
    _ticker: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Drop for SearchUi {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Browser {
    /// Search is open and has a query, so the list shows results.
    pub fn search_active(&self, cx: &gpui::App) -> bool {
        self.search.as_ref().is_some_and(|s| !s.input.read(cx).text().trim().is_empty())
    }

    pub fn search_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        self.search.as_ref().is_some_and(|s| s.input.focus_handle(cx).is_focused(window))
    }

    /// ⌘F: opens the search field (or focuses it), selecting any existing query.
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(search) = &self.search {
            let input = search.input.clone();
            input.update(cx, |input, cx| {
                let len = input.text().len();
                input.set_text(input.text().to_string(), 0..len, cx);
            });
            window.focus(&input.focus_handle(cx));
            return;
        }
        let theme = self.theme.clone();
        let input = cx.new(|cx| {
            let mut input = TextInput::new(cx);
            input.cursor_color = theme.accent;
            input.selection_color = theme.accent.opacity(0.35);
            input.placeholder_color = theme.text_muted;
            input.emit_vertical = true;
            input.set_placeholder("Search names, ext:, size:, content:…");
            input
        });
        let subscriptions = vec![
            cx.observe(&input, |this, _, cx| this.run_search(cx)),
            cx.subscribe_in(&input, window, |this, input, event, window, cx| match event {
                TextInputEvent::Confirm | TextInputEvent::Down => this.focus_results(window, cx),
                TextInputEvent::Cancel => {
                    if input.read(cx).text().is_empty() {
                        this.close_search(window, cx);
                    } else {
                        input.update(cx, |input, cx| input.set_text("", 0..0, cx));
                    }
                }
                TextInputEvent::Up | TextInputEvent::Tab => {}
            }),
        ];
        window.focus(&input.focus_handle(cx));
        self.search = Some(SearchUi {
            input,
            everywhere: self.current_dir().is_none(),
            error: None,
            sign_in: None,
            note: None,
            running: false,
            total: 0,
            scanned: Arc::new(AtomicUsize::new(0)),
            has_content: false,
            has_results: false,
            live: false,
            live_progress: None,
            cancel: Arc::new(AtomicBool::new(false)),
            _task: None,
            _ticker: None,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    /// Puts `query` in the search box (opening it if needed) and runs it.
    pub fn use_example_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.syntax_help = None;
        self.open_search(window, cx);
        if let Some(search) = &self.search {
            let len = query.len();
            search.input.update(cx, |input, cx| input.set_text(query.to_string(), len..len, cx));
        }
    }

    /// Empties the search box (the folder's contents come back) and keeps typing in it.
    pub fn clear_search_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(search) = &self.search {
            let input = search.input.clone();
            input.update(cx, |input, cx| input.set_text("", 0..0, cx));
            window.focus(&input.focus_handle(cx));
        }
    }

    pub fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.take().is_some() {
            window.focus(&self.focus);
            self.reload(cx);
        }
    }

    /// Leaves the search field for the results, selecting the first one.
    fn focus_results(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        if !self.entries.is_empty() && self.selected.is_empty() {
            self.select_only(0, cx);
        }
    }

    pub fn set_search_scope(&mut self, everywhere: bool, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            search.everywhere = everywhere;
        }
        self.run_search(cx);
    }

    /// Searches the disk directly (no index) when on.
    pub fn toggle_live_search(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            search.live = !search.live;
        }
        self.run_search(cx);
    }

    /// (Re)runs the current query in the background; results replace `entries`.
    pub fn run_search(&mut self, cx: &mut Context<Self>) {
        self.start_search(false, cx);
    }

    /// `refresh`: the index changed under an unchanged query, so keep the
    /// selection and scroll position.
    fn start_search(&mut self, refresh: bool, cx: &mut Context<Self>) {
        let current_dir = self.current_dir();
        let include_hidden = self.config.general.show_hidden;
        let max_results = self.config.search.max_results.max(1);
        let max_content_bytes = self.config.search.content_max_mb * 1024 * 1024;
        let index = cx.try_global::<IndexService>().and_then(|s| s.index.clone());
        let Some(search) = &mut self.search else { return };
        search.cancel.store(true, Ordering::Relaxed);
        search.cancel = Arc::new(AtomicBool::new(false));
        let text = search.input.read(cx).text().trim().to_string();
        if !refresh {
            search.has_results = false;
        }

        if text.is_empty() {
            search.error = None;
            search.sign_in = None;
            search.note = None;
            search.running = false;
            search._task = None;
            return self.reload(cx);
        }
        let query = match search::parse(&text) {
            Ok(query) => query,
            Err(e) => {
                search.error = Some(e);
                return cx.notify();
            }
        };
        search.error = None;
        search.sign_in = None;
        // Search the disk directly when asked to, when there's no index (off,
        // or not built yet), or when "This folder" isn't in it.
        let under = match (&index, search.everywhere, current_dir.as_deref()) {
            (Some(index), false, Some(dir)) => index.find(dir).map(Some),
            (Some(_), _, _) => Some(None),
            (None, _, _) => None,
        };
        search.has_content = query.has_content;
        search.scanned.store(0, Ordering::Relaxed);
        search.running = true;
        let cancel = search.cancel.clone();
        // A source is searched through its adapter: the cache folder only
        // holds what's been opened, so neither the index nor the disk will do.
        let source_dir = current_dir.clone().filter(|d| !search.everywhere && sources::global().contains(d));
        match (index, under, source_dir) {
            (_, _, Some(dir)) => {
                let name = sources::global().source_at(&dir).map(|s| s.name).unwrap_or_default();
                search.note = Some(format!("Searching {name}…"));
                let progress = Arc::new(LiveProgress::default());
                search.live_progress = Some(progress.clone());
                let opts = SearchOptions { under: None, include_hidden, max_content_bytes };
                let work = cx.background_executor().spawn(async move {
                    search::source::search_source(sources::global(), &dir, &query, &text, &opts, &progress, &cancel)
                });
                search._task = Some(cx.spawn(async move |this, cx| {
                    let result = work.await;
                    this.update(cx, |this, cx| match result {
                        Ok(Some(mut found)) => {
                            let total = found.entries.len();
                            found.entries.truncate(max_results);
                            if let Some(search) = &mut this.search {
                                search.note = Some(found.how);
                            }
                            this.show_results(found.entries, total, refresh, cx);
                            if let Some(first) = found.warnings.first() {
                                let more = found.warnings.len() - 1;
                                this.set_error(if more == 0 { first.clone() } else { format!("{first} (and {more} more)") }, cx);
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            if let Some(search) = &mut this.search {
                                search.running = false;
                                search.error = Some(e.to_string());
                                if let findest_core::sources::SourceError::NotSignedIn { id, .. } = e {
                                    search.sign_in = Some(id);
                                }
                            }
                            cx.notify();
                        }
                    })
                    .ok();
                }));
            }
            (Some(index), Some(under), None) if !search.live => {
                search.note = None;
                search.live_progress = None;
                let scanned = search.scanned.clone();
                let opts = SearchOptions { under, include_hidden, max_content_bytes };
                let work = cx.background_executor().spawn(async move {
                    let result = search::search(&index, &query, &opts, &scanned, &cancel)?;
                    let entries: Vec<Entry> = result.ids.iter().take(max_results).map(|&id| index.entry(id)).collect();
                    Some((entries, result.ids.len()))
                });
                search._task = Some(cx.spawn(async move |this, cx| {
                    let Some((entries, total)) = work.await else { return };
                    this.update(cx, |this, cx| this.show_results(entries, total, refresh, cx)).ok();
                }));
            }
            (index, _, None) => {
                let roots = match current_dir.filter(|_| !search.everywhere) {
                    Some(dir) => vec![dir],
                    None => self.config.search.root_paths(),
                };
                search.note = Some(match (&index, search.live) {
                    (_, true) => "Searching the disk directly".into(),
                    (None, _) => "Searching the disk directly (no search index yet)".into(),
                    (Some(_), _) => "This folder isn't indexed, so it's searched directly".into(),
                });
                let progress = Arc::new(LiveProgress::default());
                search.live_progress = Some(progress.clone());
                let excludes = Excludes::new(&self.config.search.exclude);
                let opts = SearchOptions { under: None, include_hidden, max_content_bytes };
                let work = cx.background_executor().spawn(async move {
                    let mut found = search::live_search(&roots, &query, &opts, &excludes, &progress, &cancel)?;
                    let total = found.len();
                    found.truncate(max_results);
                    Some((found, total))
                });
                search._task = Some(cx.spawn(async move |this, cx| {
                    let Some((entries, total)) = work.await else { return };
                    this.update(cx, |this, cx| this.show_results(entries, total, refresh, cx)).ok();
                }));
            }
        }
        // Slow searches: refresh the progress line, and show live matches
        // as they're found.
        search._ticker = query_is_slow(search).then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_millis(250)).await;
                    let running = this
                        .update(cx, |this, cx| {
                            this.show_live_matches_so_far(max_results, cx);
                            cx.notify();
                            this.search.as_ref().is_some_and(|s| s.running)
                        })
                        .unwrap_or(false);
                    if !running {
                        break;
                    }
                }
            })
        });
        cx.notify();
    }

    /// While a live search runs, shows what it found so far (sorted),
    /// keeping the selection and scroll position.
    fn show_live_matches_so_far(&mut self, max_results: usize, cx: &mut Context<Self>) {
        let Some(search) = &self.search else { return };
        let Some(progress) = search.live_progress.clone().filter(|_| search.running) else { return };
        let mut entries = progress.found.lock().unwrap().clone();
        if entries.len() == self.entries.len() {
            return;
        }
        search::live::sort_like_index(&mut entries);
        let total = entries.len();
        entries.truncate(max_results);
        self.show_results(entries, total, true, cx);
        if let Some(search) = &mut self.search {
            search.running = true; // not done yet
        }
    }

    fn show_results(&mut self, entries: Vec<Entry>, total: usize, refresh: bool, cx: &mut Context<Self>) {
        let Some(search) = &mut self.search else { return };
        search.running = false;
        search.has_results = true;
        search.total = total;
        self.depths = vec![0; entries.len()];
        self.expandable = vec![false; entries.len()];
        self.entries = entries;
        if refresh {
            let shown: std::collections::HashSet<&std::path::Path> = self.entries.iter().map(|e| e.path.as_path()).collect();
            self.selected.retain(|p| shown.contains(p.as_path()));
        } else {
            self.selected.clear();
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        self.loading = false;
        self.load_error = None;
        cx.notify();
    }

    /// The index changed: refresh results so they reflect the disk. Content
    /// searches read files, so they aren't repeated on every change (only
    /// when they never got to run).
    pub fn on_index_updated(&mut self, cx: &mut Context<Self>) {
        let generation = cx.try_global::<IndexService>().map(|s| s.generation).unwrap_or(0);
        if generation != self.index_generation {
            self.index_generation = generation;
            let Some(search) = &self.search else { return cx.notify() };
            // Live searches don't use the index; others refresh unless they read files.
            let live = search.live_progress.is_some();
            let (rerun, refresh) =
                (!live && (!search.has_content || (!search.running && !search.has_results)), search.has_results);
            if rerun && self.search_active(cx) {
                self.start_search(refresh, cx);
            }
        }
        cx.notify();
    }

    /// One-line search status for the search bar.
    pub fn search_summary(&self) -> Option<(String, bool)> {
        let search = self.search.as_ref()?;
        if let Some(error) = &search.error {
            return Some((error.clone(), true));
        }
        if search.running {
            let scanned = search.scanned.load(Ordering::Relaxed);
            // A source search: its note names the source ("Searching URM canary…").
            if let Some(note) = search.note.as_ref().filter(|_| !search.everywhere && self.in_source()) {
                return Some((note.clone(), false));
            }
            if let Some(progress) = &search.live_progress {
                let checked = progress.checked.load(Ordering::Relaxed) as u64;
                let found = progress.found.lock().unwrap().len();
                return Some((
                    format!(
                        "Searching the disk… {} checked · {} found",
                        findest_core::info::group_digits(checked),
                        findest_core::info::group_digits(found as u64)
                    ),
                    false,
                ));
            }
            return Some((
                if search.has_content && scanned > 0 {
                    format!("Searching contents… {} files read", findest_core::info::group_digits(scanned as u64))
                } else {
                    "Searching…".into()
                },
                false,
            ));
        }
        if let Some(note) = search.note.as_ref().filter(|_| search.live_progress.is_none()) {
            return Some((note.clone(), false));
        }
        let shown = self.entries.len();
        let total = search.total;
        let text = match total {
            0 => "No results".into(),
            1 => "1 result".into(),
            n if n > shown => format!(
                "{} results (showing the first {})",
                findest_core::info::group_digits(n as u64),
                findest_core::info::group_digits(shown as u64)
            ),
            n => format!("{} results", findest_core::info::group_digits(n as u64)),
        };
        // Say why results came from the disk rather than the index.
        let text = match &search.note {
            Some(note) if search.live_progress.is_some() => format!("{text} · {note}"),
            _ => text,
        };
        Some((text, false))
    }

    pub fn scope_label(&self) -> String {
        match &self.location {
            Location::Dir(dir) => findest_core::locations::friendly_name(&self.home, dir),
            Location::Recents => "Recents".into(),
        }
    }
}

fn query_is_slow(search: &SearchUi) -> bool {
    search.has_content || search.live_progress.is_some()
}

/// Examples shown in the "?" popover: (query, what it finds). Grouped by blank entries.
pub const SYNTAX_EXAMPLES: &[(&str, &str)] = &[
    ("report", "name contains \"report\" (any capital makes it case-sensitive)"),
    ("*.pdf", "wildcards: * any text, ? one character"),
    ("\"q3 report\"", "exact text, spaces included"),
    ("", ""),
    ("invoice 2024", "both words (AND)"),
    ("jpg | png", "either one (OR)"),
    ("report !draft", "exclude a word (also -draft)"),
    ("(jpg | png) vacation", "group with parentheses"),
    ("", ""),
    ("ext:jpg;png;heic", "by extension"),
    ("size:>10mb", "larger than 10 MB (also <, >=, 1mb..5mb, empty)"),
    ("dm:today", "modified today (yesterday, week, month, year)"),
    ("dm:>2024-05-01", "modified after a date (2024, 2024-05, a..b)"),
    ("file: size:>1gb", "only files (folder: for folders)"),
    ("", ""),
    ("path:projects", "match the full path (or any term with a /)"),
    ("regex:^IMG_\\d{4}", "regular expression on the name"),
    ("case:README", "force case-sensitive (nocase: for insensitive)"),
    ("ext:md content:TODO", "text inside files (add filters to keep it fast)"),
    ("status:Ready", "in a source: by one of its columns"),
    ("hidden: .zshrc", "include hidden files"),
];

#[cfg(test)]
mod tests {
    #[test]
    fn syntax_examples_parse() {
        for (query, _) in super::SYNTAX_EXAMPLES.iter().filter(|(q, _)| !q.is_empty()) {
            findest_core::search::parse(query).unwrap_or_else(|e| panic!("{query}: {e}"));
        }
    }
}
