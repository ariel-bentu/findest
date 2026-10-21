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
//! The index is saved after each full scan, and with the changes since at
//! most hourly, so `findest search` finds it nearly current.
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
use findest_core::search::saved::{ScanMark, remove_saved};
use findest_core::search::{Excludes, Index};
use gpui::{App, AsyncApp, BorrowAppContext, Global};

use findest_core::fsevents::{self, EventStream};

/// Rescan from scratch this often (Unix seconds).
const FULL_SCAN_INTERVAL: i64 = 24 * 60 * 60;
/// Apply reported changes at most this often.
const UPDATE_INTERVAL: Duration = Duration::from_secs(3);
/// Entries added or removed since the last full scan before one runs early.
const MAX_CHANGED: usize = 200_000;
/// Changed paths kept while paused before settling for a full rescan.
const MAX_PENDING: usize = 100_000;
/// Save the index with the changes applied since at most this often (seconds).
const SAVE_INTERVAL: i64 = 60 * 60;

pub struct IndexService {
    pub index: Option<Arc<Index>>,
    pub building: bool,
    /// Entries found by the scan in progress.
    pub progress: Arc<AtomicUsize>,
    /// Bumped whenever `index` is replaced, so searches can re-run.
    pub generation: u64,
    pub config: SearchConfig,
    pub pause: Option<Pause>,
    /// The FSEvents id `index` is current up to.
    through: u64,
    /// `updated_at` of the index last saved (or loaded).
    saved_at: i64,
    /// When changes may be saved next (Unix seconds): right after launch,
    /// then hourly.
    save_due_at: i64,
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
    /// The id of the last event noted in `paths`.
    last_id: u64,
    /// Changes made while findest wasn't running are still being replayed.
    replaying: bool,
    /// The id the replayed changes go up to, once they all arrived.
    history_end: u64,
}

enum Work {
    Scan,
    /// The changed paths, and the event id they go up to.
    Apply(HashSet<PathBuf>, u64),
}

pub fn init(cx: &mut App) {
    let config = Config::load(&Config::path()).unwrap_or_default().search;
    let enabled = config.indexing;
    let replay_from = enabled.then(|| ScanMark::replay_from(&config)).flatten();
    let pending = Arc::new(Mutex::new(Pending { replaying: replay_from.is_some(), ..Default::default() }));
    let watcher = enabled.then(|| watch_roots(&config, pending.clone(), replay_from)).flatten();
    cx.set_global(IndexService {
        index: None,
        building: false,
        progress: Arc::new(AtomicUsize::new(0)),
        generation: 0,
        config,
        pause: None,
        through: replay_from.unwrap_or(0),
        saved_at: 0,
        save_due_at: 0,
        scan_needed: enabled && replay_from.is_none(),
        cancel: Arc::default(),
        off: Arc::new(AtomicBool::new(!enabled)),
        pending,
        _watcher: watcher,
    });

    let cached = cx.background_executor().spawn(async move { enabled.then(|| Index::load(&Index::default_path()).ok()).flatten() });
    cx.spawn(async move |cx| {
        match cached.await {
            Some(index) => {
                let saved_at = index.updated_at;
                publish(cx, index);
                cx.update_global::<IndexService, _>(|s, _| s.saved_at = saved_at).ok();
            }
            None => {
                // Nothing to replay changes onto.
                cx.update_global::<IndexService, _>(|s, _| s.scan_needed = s.enabled()).ok();
            }
        }
        loop {
            match next_work(cx) {
                Some(Work::Scan) => rebuild(cx).await,
                Some(Work::Apply(paths, through)) => apply_changes(cx, paths, through).await,
                None => {}
            }
            save_if_due(cx);
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
            Some(Work::Apply(std::mem::take(&mut pending.paths), pending.last_id))
                .filter(|w| matches!(w, Work::Apply(p, _) if !p.is_empty()))
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
    let mark_id = mark.event_id;

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
    save(cx, index.clone(), mark, off);
    cx.update_global::<IndexService, _>(|s, _| {
        s.through = mark_id;
        s.saved_at = index.updated_at;
        s.save_due_at = chrono::Utc::now().timestamp() + SAVE_INTERVAL;
        s.index = Some(index);
        s.building = false;
        s.generation += 1;
    })
    .ok();
}

/// Saves the index if it changed since it was last saved: once caught up
/// after launch, then at most hourly (and not halfway through replaying changes: the CLI takes a saved index
/// as current as of when it was updated).
fn save_if_due(cx: &mut AsyncApp) {
    let Ok(Some((index, mark, off))) = cx.update_global::<IndexService, _>(|s, _| {
        let due = |i: &Arc<Index>| {
            let pending = s.pending.lock().unwrap();
            !s.building
                && !pending.replaying
                && s.through >= pending.history_end
                && i.updated_at > s.saved_at
                && chrono::Utc::now().timestamp() >= s.save_due_at
        };
        let index = s.index.clone().filter(due)?;
        s.saved_at = index.updated_at;
        s.save_due_at = chrono::Utc::now().timestamp() + SAVE_INTERVAL;
        Some((index, ScanMark::at(&s.config, s.through), s.off.clone()))
    }) else {
        return;
    };
    save(cx, index, mark, off);
}

fn save(cx: &mut AsyncApp, index: Arc<Index>, mark: ScanMark, off: Arc<AtomicBool>) {
    cx.background_executor()
        .spawn(async move {
            in_background(|| {
                let result = mark.save_with(&index);
                // Turned off while saving: don't leave it behind.
                if off.load(Ordering::Relaxed) {
                    remove_saved();
                }
                result
            })
        })
        .detach();
}

/// Re-reads the folders around `paths` and publishes the result.
async fn apply_changes(cx: &mut AsyncApp, paths: HashSet<PathBuf>, through: u64) {
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
        s.through = through;
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
    EventStream::start(&watched, replay_from, 0.5, move |events, last_id| {
        let relevant = |p: &PathBuf| !own_dir.as_ref().is_some_and(|d| p.starts_with(d)) && !excludes.skips(p, &roots);
        let mut pending = pending.lock().unwrap();
        pending.last_id = pending.last_id.max(last_id);
        for event in events {
            match event {
                fsevents::Event::Changed(path) if relevant(&path) => {
                    pending.paths.insert(path);
                }
                fsevents::Event::Changed(_) => {}
                fsevents::Event::HistoryDone => {
                    pending.replaying = false;
                    pending.history_end = pending.last_id;
                }
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
