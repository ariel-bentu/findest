//! The app-wide filename index behind search, shared by all windows.
//!
//! At launch the saved index is loaded (instant results), then rebuilt in the
//! background. FSEvents marks it stale when files change under the indexed
//! roots, and it is rebuilt at most once a minute while changes keep coming.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use findest_core::config::{Config, SearchConfig};
use findest_core::search::{Excludes, Index};
use gpui::{App, AsyncApp, Global};
use notify::{RecursiveMode, Watcher};

/// How often to rebuild while files keep changing.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

pub struct IndexService {
    pub index: Option<Arc<Index>>,
    pub building: bool,
    /// Entries found by the scan in progress.
    pub progress: Arc<AtomicUsize>,
    /// Bumped whenever `index` is replaced, so searches can re-run.
    pub generation: u64,
    pub config: SearchConfig,
    dirty: Arc<AtomicBool>,
    last_build: Option<Instant>,
    _watcher: Option<notify::RecommendedWatcher>,
}

impl Global for IndexService {}

pub fn init(cx: &mut App) {
    let config = Config::load(&Config::path()).unwrap_or_default().search;
    let dirty = Arc::new(AtomicBool::new(false));
    let watcher = watch_roots(&config, dirty.clone());
    cx.set_global(IndexService {
        index: None,
        building: false,
        progress: Arc::new(AtomicUsize::new(0)),
        generation: 0,
        config,
        dirty,
        last_build: None,
        _watcher: watcher,
    });

    let cached = cx.background_executor().spawn(async { Index::load(&Index::default_path()).ok() });
    cx.spawn(async move |cx| {
        if let Some(index) = cached.await {
            publish(cx, index);
        }
        rebuild(cx).await;
        loop {
            cx.background_executor().timer(Duration::from_secs(15)).await;
            let due = cx
                .read_global::<IndexService, _>(|s, _| {
                    !s.building
                        && s.dirty.load(Ordering::Relaxed)
                        && s.last_build.is_none_or(|t| t.elapsed() >= REFRESH_INTERVAL)
                })
                .unwrap_or(false);
            if due {
                rebuild(cx).await;
            }
        }
    })
    .detach();
}

/// Rebuilds now (e.g. after the search config changed).
pub fn rebuild_now(cx: &mut App) {
    if cx.try_global::<IndexService>().is_some_and(|s| s.building) {
        return;
    }
    cx.spawn(async move |cx| rebuild(cx).await).detach();
}

fn publish(cx: &mut AsyncApp, index: Index) {
    cx.update_global::<IndexService, _>(|service, _| {
        service.index = Some(Arc::new(index));
        service.generation += 1;
    })
    .ok();
}

async fn rebuild(cx: &mut AsyncApp) {
    let Ok((config, progress, dirty)) = cx.update_global::<IndexService, _>(|s, _| {
        s.building = true;
        s.progress.store(0, Ordering::Relaxed);
        (s.config.clone(), s.progress.clone(), s.dirty.clone())
    }) else {
        return;
    };
    dirty.store(false, Ordering::Relaxed);

    let scan = cx.background_executor().spawn({
        let progress = progress.clone();
        async move {
            let excludes = Excludes::new(&config.exclude);
            Index::build(&config.root_paths(), &excludes, &progress, &AtomicBool::new(false))
        }
    });
    // Tick while scanning so windows can show progress.
    let ticker = cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_millis(500)).await;
            if cx.update_global::<IndexService, _>(|_, _| ()).is_err() {
                break;
            }
        }
    });
    let index = Arc::new(scan.await);
    drop(ticker);

    let saved = index.clone();
    cx.background_executor().spawn(async move { saved.save(&Index::default_path()) }).detach();
    cx.update_global::<IndexService, _>(|s, _| {
        s.index = Some(index);
        s.building = false;
        s.generation += 1;
        s.last_build = Some(Instant::now());
    })
    .ok();
}

/// Marks the index stale on any change under the roots (FSEvents is
/// recursive and cheap, so this watches whole trees).
fn watch_roots(config: &SearchConfig, dirty: Arc<AtomicBool>) -> Option<notify::RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok_and(|e| !matches!(e.kind, notify::EventKind::Access(_))) {
            dirty.store(true, Ordering::Relaxed);
        }
    })
    .ok()?;
    for root in config.root_paths() {
        let _ = watcher.watch(&root, RecursiveMode::Recursive);
    }
    Some(watcher)
}

/// "1.2M items · updated 3 min ago" / "Indexing… 340,000 items".
pub fn status_text(service: &IndexService) -> String {
    if service.building {
        let n = service.progress.load(Ordering::Relaxed) as u64;
        return format!("Indexing… {} items", findest_core::info::group_digits(n));
    }
    match &service.index {
        None => "Index not built yet".into(),
        Some(index) => {
            let count = index.len();
            let count = if count >= 1_000_000 {
                format!("{:.1}M", count as f64 / 1e6)
            } else if count >= 10_000 {
                format!("{}k", count / 1000)
            } else {
                count.to_string()
            };
            let age = chrono::Utc::now().timestamp() - index.built_at;
            let age = match age {
                a if a < 60 => "just now".to_string(),
                a if a < 3600 => format!("{} min ago", a / 60),
                a if a < 86_400 => format!("{} h ago", a / 3600),
                a => format!("{} days ago", a / 86_400),
            };
            format!("{count} items indexed · updated {age}")
        }
    }
}
