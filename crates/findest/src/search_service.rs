//! The app-wide filename index behind search, shared by all windows.
//!
//! At launch the saved index is loaded (instant results), then rescanned in
//! the background, since changes made while findest was closed are unknown.
//! After that FSEvents reports changes as they happen and only the folders
//! involved are re-read. A full rescan runs once a day (or when FSEvents
//! drops events, or many changes have piled up), on few threads at
//! background priority.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use findest_core::config::{Config, SearchConfig};
use findest_core::search::index::in_background;
use findest_core::search::{Excludes, Index};
use gpui::{App, AsyncApp, Global};
use notify::{RecursiveMode, Watcher};

/// Rescan from scratch this often (Unix seconds).
const FULL_SCAN_INTERVAL: i64 = 24 * 60 * 60;
/// Apply reported changes at most this often.
const UPDATE_INTERVAL: Duration = Duration::from_secs(3);
/// Entries added or removed since the last full scan before one runs early.
const MAX_CHANGED: usize = 200_000;

pub struct IndexService {
    pub index: Option<Arc<Index>>,
    pub building: bool,
    /// Entries found by the scan in progress.
    pub progress: Arc<AtomicUsize>,
    /// Bumped whenever `index` is replaced, so searches can re-run.
    pub generation: u64,
    pub config: SearchConfig,
    pending: Arc<Mutex<Pending>>,
    _watcher: Option<notify::RecommendedWatcher>,
}

impl Global for IndexService {}

/// Changes reported by FSEvents, waiting to be applied.
#[derive(Default)]
struct Pending {
    paths: HashSet<PathBuf>,
    /// Events were dropped: only a full rescan can catch up.
    rescan: bool,
}

pub fn init(cx: &mut App) {
    let config = Config::load(&Config::path()).unwrap_or_default().search;
    let pending = Arc::new(Mutex::new(Pending::default()));
    let watcher = watch_roots(&config, pending.clone());
    cx.set_global(IndexService {
        index: None,
        building: false,
        progress: Arc::new(AtomicUsize::new(0)),
        generation: 0,
        config,
        pending,
        _watcher: watcher,
    });

    let cached = cx.background_executor().spawn(async { Index::load(&Index::default_path()).ok() });
    cx.spawn(async move |cx| {
        if let Some(index) = cached.await {
            publish(cx, index);
        }
        rebuild(cx).await;
        loop {
            cx.background_executor().timer(UPDATE_INTERVAL).await;
            let Ok(Some((full_scan_due, paths))) = cx.read_global::<IndexService, _>(|s, _| {
                if s.building {
                    return None;
                }
                let mut pending = s.pending.lock().unwrap();
                let stale = s.index.as_ref().is_none_or(|i| {
                    chrono::Utc::now().timestamp() - i.built_at >= FULL_SCAN_INTERVAL || i.changed_count() > MAX_CHANGED
                });
                Some((pending.rescan || stale, std::mem::take(&mut pending.paths)))
            }) else {
                continue;
            };
            if full_scan_due {
                rebuild(cx).await;
            } else if !paths.is_empty() {
                apply_changes(cx, paths).await;
            }
        }
    })
    .detach();
}

/// Rebuilds now (e.g. after the search config changed).
pub fn rebuild_now(cx: &mut App) {
    let Some(service) = cx.try_global::<IndexService>() else { return };
    if service.building {
        return;
    }
    // The roots or excludes may have changed.
    let watcher = watch_roots(&service.config, service.pending.clone());
    cx.global_mut::<IndexService>()._watcher = watcher;
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
    let Ok((config, progress)) = cx.update_global::<IndexService, _>(|s, _| {
        s.building = true;
        s.progress.store(0, Ordering::Relaxed);
        // The scan sees everything up to now; changes during it stay queued.
        *s.pending.lock().unwrap() = Pending::default();
        (s.config.clone(), s.progress.clone())
    }) else {
        return;
    };

    let scan = cx.background_executor().spawn({
        let progress = progress.clone();
        async move {
            let excludes = Excludes::new(&config.exclude);
            in_background(|| Index::build(&config.root_paths(), &excludes, &progress, &Default::default()))
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
    cx.background_executor().spawn(async move { in_background(|| saved.save(&Index::default_path())) }).detach();
    cx.update_global::<IndexService, _>(|s, _| {
        s.index = Some(index);
        s.building = false;
        s.generation += 1;
    })
    .ok();
}

/// Re-reads the folders around `paths` and publishes the result.
async fn apply_changes(cx: &mut AsyncApp, paths: HashSet<PathBuf>) {
    let Ok(Some((index, config))) = cx.read_global::<IndexService, _>(|s, _| Some((s.index.clone()?, s.config.clone())))
    else {
        return;
    };
    let paths: Vec<PathBuf> = paths.into_iter().collect();
    let work = cx.background_executor().spawn({
        let (index, paths) = (index.clone(), paths.clone());
        async move { in_background(|| index.with_changes(&paths, &Excludes::new(&config.exclude))) }
    });
    let Some(updated) = work.await else { return };
    cx.update_global::<IndexService, _>(|s, _| {
        // A rescan started or finished meanwhile: apply these to its result instead.
        if s.building || !s.index.as_ref().is_some_and(|i| i.same_scan(&index)) {
            s.pending.lock().unwrap().paths.extend(paths);
            return;
        }
        s.index = Some(Arc::new(updated));
        s.generation += 1;
    })
    .ok();
}

/// Queues changes under the roots that a scan would pick up (FSEvents is
/// recursive and cheap, so this watches whole trees). Excluded paths and
/// findest's own data folder are ignored: busy folders like ~/Library, and
/// saving the index itself, would otherwise keep it updating.
fn watch_roots(config: &SearchConfig, pending: Arc<Mutex<Pending>>) -> Option<notify::RecommendedWatcher> {
    let roots = config.root_paths();
    let excludes = Excludes::new(&config.exclude);
    let own_dir = Index::default_path().parent().map(|p| p.to_path_buf());
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        let rescan = event.attrs.flag() == Some(notify::event::Flag::Rescan);
        let unscoped = event.paths.is_empty();
        let relevant: Vec<PathBuf> = event
            .paths
            .into_iter()
            .filter(|p| !own_dir.as_ref().is_some_and(|d| p.starts_with(d)) && !excludes.skips(p, &roots))
            .collect();
        let mut pending = pending.lock().unwrap();
        // FSEvents lost track of changes (under these paths): rescan everything.
        pending.rescan |= rescan && (unscoped || !relevant.is_empty());
        pending.paths.extend(relevant);
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
            let count = index.live_count();
            let count = if count >= 1_000_000 {
                format!("{:.1}M", count as f64 / 1e6)
            } else if count >= 10_000 {
                format!("{}k", count / 1000)
            } else {
                count.to_string()
            };
            let age = chrono::Utc::now().timestamp() - index.updated_at;
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
