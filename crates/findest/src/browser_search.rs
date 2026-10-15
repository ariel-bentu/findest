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
            cancel: Arc::new(AtomicBool::new(false)),
            _task: None,
            _ticker: None,
            _subscriptions: subscriptions,
        });
        cx.notify();
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
        let current_dir = self.current_dir();
        let include_hidden = self.config.general.show_hidden;
        let max_results = self.config.search.max_results.max(1);
        let max_content_bytes = self.config.search.content_max_mb * 1024 * 1024;
        let index = cx.try_global::<IndexService>().and_then(|s| s.index.clone());
        let Some(search) = &mut self.search else { return };
        search.cancel.store(true, Ordering::Relaxed);
        search.cancel = Arc::new(AtomicBool::new(false));
        let text = search.input.read(cx).text().trim().to_string();

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
            search.note = Some("Building the search index… results will appear when it's ready".into());
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
            this.update(cx, |this, cx| this.show_results(entries, total, cx)).ok();
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

    fn show_results(&mut self, entries: Vec<Entry>, total: usize, cx: &mut Context<Self>) {
        let Some(search) = &mut self.search else { return };
        search.running = false;
        search.total = total;
        self.depths = vec![0; entries.len()];
        self.expandable = vec![false; entries.len()];
        self.entries = entries;
        self.selected.clear();
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.loading = false;
        self.load_error = None;
        cx.notify();
    }

    /// The index was rebuilt: refresh results so they reflect the disk.
    pub fn on_index_updated(&mut self, cx: &mut Context<Self>) {
        let generation = cx.try_global::<IndexService>().map(|s| s.generation).unwrap_or(0);
        if generation != self.index_generation {
            self.index_generation = generation;
            if self.search_active(cx) {
                self.run_search(cx);
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
