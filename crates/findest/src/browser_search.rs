//! Search mode for a browser window: the query field, scope, and running
//! queries against the shared index. Results replace the folder listing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use findest_core::listing::Entry;
use findest_core::search::{self, SearchOptions};
use gpui::{AppContext, Context, Entity, Focusable, ScrollStrategy, Subscription, Task, Window};

use crate::browser::{Browser, Location};
use crate::search_service::IndexService;
use crate::text_input::{TextInput, TextInputEvent};

pub struct SearchUi {
    pub input: Entity<TextInput>,
    /// Search all indexed folders rather than just the current one.
    pub everywhere: bool,
    pub error: Option<String>,
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
                TextInputEvent::Up => {}
            }),
        ];
        window.focus(&input.focus_handle(cx));
        self.search = Some(SearchUi {
            input,
            everywhere: self.current_dir().is_none(),
            error: None,
            note: None,
            running: false,
            total: 0,
            scanned: Arc::new(AtomicUsize::new(0)),
            has_content: false,
            has_results: false,
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
        let Some(index) = index else {
            let enabled = cx.try_global::<IndexService>().is_some_and(|s| s.enabled());
            search.note = Some(if enabled {
                "Building the search index… results will appear when it's ready".into()
            } else {
                "Search indexing is off. Turn it on from “Search index off” at the right".into()
            });
            return cx.notify();
        };
        let under = if search.everywhere { None } else { current_dir.as_deref().and_then(|d| index.find(d)) };
        search.note = (!search.everywhere && under.is_none() && current_dir.is_some())
            .then(|| "This folder isn't indexed, so results are from everywhere".into());

        search.running = true;
        search.has_content = query.has_content;
        search.scanned.store(0, Ordering::Relaxed);
        let (cancel, scanned) = (search.cancel.clone(), search.scanned.clone());
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
        // Content searches can take a while: refresh the progress line.
        search._ticker = query_is_slow(search).then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_millis(250)).await;
                    let running = this
                        .update(cx, |this, cx| {
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
            let (rerun, refresh) = (!search.has_content || (!search.running && !search.has_results), search.has_results);
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
            return Some((
                if search.has_content && scanned > 0 {
                    format!("Searching contents… {} files read", findest_core::info::group_digits(scanned as u64))
                } else {
                    "Searching…".into()
                },
                false,
            ));
        }
        if let Some(note) = &search.note {
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
    search.has_content
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
