//! The app-wide filename index behind search, shared by all windows.
//!
//! At launch the saved index is loaded (instant results) and FSEvents
//! replays every change since its scan started, even those made while
//! findest wasn't running; only the folders involved are re-read. After that
//! changes are applied the same way as they happen. When the history can't
//! be replayed (it was reset, or the scan settings changed) the index is
//! rescanned instead. A full rescan runs once a day (or when FSEvents
//! drops events, or many changes have piled up), on few threads at
//! background priority.
//!
//! Indexing can be paused for a while (not remembered across launches) or
//! turned off in the config (`search.indexing`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use findest_core::config::{Config, SearchConfig};
use findest_core::search::index::in_background;
use findest_core::search::{Excludes, Index};
use gpui::{App, AsyncApp, BorrowAppContext, Global};
use serde::{Deserialize, Serialize};

use crate::fsevents::{self, EventStream};

/// Rescan from scratch this often (Unix seconds).
const FULL_SCAN_INTERVAL: i64 = 24 * 60 * 60;
/// Apply reported changes at most this often.
const UPDATE_INTERVAL: Duration = Duration::from_secs(3);
/// Entries added or removed since the last full scan before one runs early.
const MAX_CHANGED: usize = 200_000;
/// Changed paths kept while paused before settling for a full rescan.
const MAX_PENDING: usize = 100_000;

pub struct IndexService {
    pub index: Option<Arc<Index>>,
    pub building: bool,
    /// Entries found by the scan in progress.
    pub progress: Arc<AtomicUsize>,
    /// Bumped whenever `index` is replaced, so searches can re-run.
    pub generation: u64,
    pub config: SearchConfig,
    pub pause: Option<Pause>,
    /// A full scan should run as soon as indexing may (at launch, or after
    /// one was interrupted).
    scan_needed: bool,
    /// Stops the scan in progress.
    cancel: Arc<AtomicBool>,
    /// Set while indexing is off, so a save finishing late removes its file.
    off: Arc<AtomicBool>,
    pending: Arc<Mutex<Pending>>,
    _watcher: Option<EventStream>,
}

impl Global for IndexService {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pause {
    /// Until resumed (or findest quits).
    UntilResumed,
    Until(Instant),
}

impl IndexService {
    pub fn enabled(&self) -> bool {
        self.config.indexing
    }

    pub fn is_paused(&self) -> bool {
        match self.pause {
            None => false,
            Some(Pause::UntilResumed) => true,
            Some(Pause::Until(t)) => Instant::now() < t,
        }
    }
}

/// Changes reported by FSEvents, waiting to be applied.
#[derive(Default)]
struct Pending {
    paths: HashSet<PathBuf>,
    /// Events were dropped: only a full rescan can catch up.
    rescan: bool,
}

/// Saved next to the index: where its scan stood in the FSEvents history,
/// so the next launch can replay what changed since instead of rescanning.
#[derive(PartialEq, Serialize, Deserialize)]
struct ScanMark {
    /// Every change after this event id happened after the scan started.
    event_id: u64,
    /// Each root's volume history (event ids only make sense within it).
    histories: Vec<Option<String>>,
    roots: Vec<String>,
    exclude: Vec<String>,
}

impl ScanMark {
    fn now(config: &SearchConfig) -> ScanMark {
        ScanMark {
            event_id: fsevents::current_event_id(),
            histories: config.root_paths().iter().map(|r| fsevents::history_id(r)).collect(),
            roots: config.roots.clone(),
            exclude: config.exclude.clone(),
        }
    }

    fn path() -> PathBuf {
        Index::default_path().with_file_name("index-mark.json")
    }

    /// The event id to replay from, if a saved scan of the same roots can
    /// be caught up that way.
    fn replay_from(config: &SearchConfig) -> Option<u64> {
        let saved: ScanMark = serde_json::from_slice(&std::fs::read(Self::path()).ok()?).ok()?;
        let current = ScanMark::now(config);
        (saved.roots == current.roots && saved.exclude == current.exclude && saved.histories == current.histories)
            .then_some(saved.event_id)
    }
}

/// Removes the saved index and its mark.
fn remove_saved() {
    let _ = std::fs::remove_file(ScanMark::path());
    let _ = std::fs::remove_file(Index::default_path());
}

enum Work {
    Scan,
    Apply(HashSet<PathBuf>),
}

pub fn init(cx: &mut App) {
    let config = Config::load(&Config::path()).unwrap_or_default().search;
    let pending = Arc::new(Mutex::new(Pending::default()));
    let enabled = config.indexing;
    let replay_from = enabled.then(|| ScanMark::replay_from(&config)).flatten();
    let watcher = enabled.then(|| watch_roots(&config, pending.clone(), replay_from)).flatten();
    cx.set_global(IndexService {
        index: None,
        building: false,
        progress: Arc::new(AtomicUsize::new(0)),
        generation: 0,
        config,
        pause: None,
        scan_needed: enabled && replay_from.is_none(),
        cancel: Arc::default(),
        off: Arc::new(AtomicBool::new(!enabled)),
        pending,
        _watcher: watcher,
    });

    let cached = cx.background_executor().spawn(async move { enabled.then(|| Index::load(&Index::default_path()).ok()).flatten() });
    cx.spawn(async move |cx| {
        match cached.await {
            Some(index) => publish(cx, index),
            None => {
                // Nothing to replay changes onto.
                cx.update_global::<IndexService, _>(|s, _| s.scan_needed = s.enabled()).ok();
            }
        }
        loop {
            match next_work(cx) {
                Some(Work::Scan) => rebuild(cx).await,
                Some(Work::Apply(paths)) => apply_changes(cx, paths).await,
                None => {}
            }
            cx.background_executor().timer(UPDATE_INTERVAL).await;
        }
    })
    .detach();
}

/// What the index needs now, if indexing is on and not paused.
fn next_work(cx: &mut AsyncApp) -> Option<Work> {
    let expired = cx.read_global::<IndexService, _>(|s, _| s.pause.is_some() && !s.is_paused()).ok()?;
    if expired {
        // Notifies windows, so they stop showing the pause.
        cx.update_global::<IndexService, _>(|s, _| s.pause = None).ok()?;
    }
    cx.read_global::<IndexService, _>(|s, _| {
        if s.building || !s.enabled() || s.is_paused() {
            return None;
        }
        let mut pending = s.pending.lock().unwrap();
        let stale = s.scan_needed
            || pending.rescan
            || s.index.as_ref().is_none_or(|i| {
                chrono::Utc::now().timestamp() - i.built_at >= FULL_SCAN_INTERVAL || i.changed_count() > MAX_CHANGED
            });
        if stale {
            Some(Work::Scan)
        } else {
            Some(Work::Apply(std::mem::take(&mut pending.paths))).filter(|w| matches!(w, Work::Apply(p) if !p.is_empty()))
        }
    })
    .ok()
    .flatten()
}

/// Rebuilds now (the user asked for it). Lifts a pause.
pub fn rebuild_now(cx: &mut App) {
    let Some(service) = cx.try_global::<IndexService>() else { return };
    if service.building || !service.enabled() {
        return;
    }
    cx.update_global::<IndexService, _>(|s, _| s.pause = None);
    cx.spawn(async move |cx| rebuild(cx).await).detach();
}

/// Holds off scans and updates for `duration`, or until resumed. A scan in
/// progress stops and runs again afterwards; changes keep being noted.
pub fn pause(duration: Option<Duration>, cx: &mut App) {
    cx.update_global::<IndexService, _>(|s, _| {
        s.pause = Some(duration.map_or(Pause::UntilResumed, |d| Pause::Until(Instant::now() + d)));
        s.cancel.store(true, Ordering::Relaxed);
    });
}

pub fn resume(cx: &mut App) {
    cx.update_global::<IndexService, _>(|s, _| s.pause = None);
}

/// Turns indexing on or off for good, by saving it in the config file.
pub fn set_enabled(on: bool, cx: &mut App) -> Result<()> {
    Config::save_search_indexing(&Config::path(), on)?;
    let mut config = cx.global::<IndexService>().config.clone();
    config.indexing = on;
    set_config(config, cx);
    Ok(())
}

/// Takes in a changed `[search]` config (every window reports the same
/// change, so repeats are ignored).
pub fn set_config(config: SearchConfig, cx: &mut App) {
    let Some(service) = cx.try_global::<IndexService>() else { return };
    if service.config == config {
        return;
    }
    let was_enabled = service.enabled();
    let scope_changed = service.config.roots != config.roots || service.config.exclude != config.exclude;
    cx.update_global::<IndexService, _>(|s, cx| {
        s.config = config;
        if !s.enabled() {
            // Off: stop everything and drop the index, saved copy included.
            s.cancel.store(true, Ordering::Relaxed);
            s.off.store(true, Ordering::Relaxed);
            s._watcher = None;
            s.index = None;
            s.pause = None;
            s.generation += 1;
            *s.pending.lock().unwrap() = Pending::default();
            cx.background_executor().spawn(async { remove_saved() }).detach();
        } else if !was_enabled || scope_changed {
            s.off.store(false, Ordering::Relaxed);
            s._watcher = watch_roots(&s.config, s.pending.clone(), None);
            s.scan_needed = true;
            s.cancel.store(true, Ordering::Relaxed); // a scan in progress has the old scope
        }
    });
}

fn publish(cx: &mut AsyncApp, index: Index) {
    cx.update_global::<IndexService, _>(|service, _| {
        service.index = Some(Arc::new(index));
        service.generation += 1;
    })
    .ok();
}

async fn rebuild(cx: &mut AsyncApp) {
    let cancel = Arc::new(AtomicBool::new(false));
    let Ok((config, progress, off, mark)) = cx.update_global::<IndexService, _>(|s, _| {
        // Taken before scanning: changes during the scan get replayed later.
        let mark = ScanMark::now(&s.config);
        s.building = true;
        s.scan_needed = false;
        s.cancel = cancel.clone();
        s.progress.store(0, Ordering::Relaxed);
        // The scan sees everything up to now; changes during it stay queued.
        *s.pending.lock().unwrap() = Pending::default();
        (s.config.clone(), s.progress.clone(), s.off.clone(), mark)
    }) else {
        return;
    };

    let scan = cx.background_executor().spawn({
        let (progress, cancel) = (progress.clone(), cancel.clone());
        async move {
            let excludes = Excludes::new(&config.exclude);
            in_background(|| Index::build(&config.root_paths(), &excludes, &progress, &cancel))
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

    if cancel.load(Ordering::Relaxed) {
        // Paused, turned off or rescoped: keep the previous index; scan again when allowed.
        cx.update_global::<IndexService, _>(|s, _| {
            s.building = false;
            s.scan_needed = s.enabled();
        })
        .ok();
        return;
    }
    let saved = index.clone();
    cx.background_executor()
        .spawn(async move {
            in_background(|| {
                // The mark goes first and comes back last, so a mark never
                // claims an index newer than the one saved.
                let _ = std::fs::remove_file(ScanMark::path());
                let result = saved.save(&Index::default_path());
                if result.is_ok() {
                    let _ = serde_json::to_vec(&mark).map(|json| std::fs::write(ScanMark::path(), json));
                }
                // Turned off while saving: don't leave it behind.
                if off.load(Ordering::Relaxed) {
                    remove_saved();
                }
                result
            })
        })
        .detach();
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
/// recursive and cheap, so this watches whole trees), first replaying those
/// since `replay_from`. Excluded paths and findest's own data folder are
/// ignored: busy folders like ~/Library, and saving the index itself, would
/// otherwise keep it updating.
fn watch_roots(config: &SearchConfig, pending: Arc<Mutex<Pending>>, replay_from: Option<u64>) -> Option<EventStream> {
    let roots = config.root_paths();
    let excludes = Excludes::new(&config.exclude);
    let own_dir = Index::default_path().parent().map(|p| p.to_path_buf());
    let watched = roots.clone();
    EventStream::start(&watched, replay_from, 0.5, move |events| {
        let relevant = |p: &PathBuf| !own_dir.as_ref().is_some_and(|d| p.starts_with(d)) && !excludes.skips(p, &roots);
        let mut pending = pending.lock().unwrap();
        for event in events {
            match event {
                fsevents::Event::Changed(path) if relevant(&path) => {
                    pending.paths.insert(path);
                }
                fsevents::Event::Changed(_) => {}
                fsevents::Event::Rescan(path) => pending.rescan |= path.is_none_or(|p| relevant(&p)),
            }
        }
        if pending.rescan || pending.paths.len() > MAX_PENDING {
            // A full rescan is coming anyway (e.g. after a long pause).
            pending.rescan = true;
            pending.paths.clear();
        }
    })
}

/// "1.2M items · updated 3 min ago" / "Indexing… 340,000 items" /
/// "Indexing paused" / "Search index off".
pub fn status_text(service: &IndexService) -> String {
    if !service.enabled() {
        return "Search index off".into();
    }
    if service.is_paused() {
        return match service.pause {
            Some(Pause::Until(t)) => {
                let mins = t.saturating_duration_since(Instant::now()).as_secs().div_ceil(60);
                format!("Indexing paused · resumes in {mins} min")
            }
            _ => "Indexing paused".into(),
        };
    }
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
