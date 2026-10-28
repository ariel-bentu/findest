//! The index as the app saves it, and catching a saved copy up with what
//! changed on disk since (for the command line, which has no watcher).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{Excludes, Index};
use crate::config::SearchConfig;
use crate::fsevents::{self, Event, EventStream};

/// Changed paths replayed at most before settling for a rescan.
const MAX_REPLAYED: usize = 100_000;

/// Saved next to the index: where it stands in the FSEvents history, so the
/// changes made since can be replayed instead of rescanning.
#[derive(PartialEq, Serialize, Deserialize)]
pub struct ScanMark {
    /// Every change after this event id may be missing from the index.
    pub event_id: u64,
    /// Each root's volume history (event ids only make sense within it).
    histories: Vec<Option<String>>,
    roots: Vec<String>,
    exclude: Vec<String>,
}

impl ScanMark {
    /// A mark for an index holding every change up to `event_id`.
    pub fn at(config: &SearchConfig, event_id: u64) -> ScanMark {
        ScanMark {
            event_id,
            histories: config.root_paths().iter().map(|r| fsevents::history_id(r)).collect(),
            roots: config.roots.clone(),
            exclude: config.exclude.clone(),
        }
    }

    pub fn now(config: &SearchConfig) -> ScanMark {
        Self::at(config, fsevents::current_event_id())
    }

    pub fn path() -> PathBuf {
        Index::default_path().with_file_name("index-mark.json")
    }

    /// The event id to replay from, if the saved index covers the same
    /// roots and its history is still there.
    pub fn replay_from(config: &SearchConfig) -> Option<u64> {
        if !fsevents::REPLAY_SUPPORTED {
            return None;
        }
        let saved: ScanMark = serde_json::from_slice(&std::fs::read(Self::path()).ok()?).ok()?;
        saved.replay_id(&ScanMark::now(config))
    }

    /// The event id to replay from, if `self` was saved for the same roots
    /// and history as `current`. Without a history to replay (no FSEvents),
    /// there's never one: only a rescan catches an old index up.
    fn replay_id(&self, current: &ScanMark) -> Option<u64> {
        (fsevents::REPLAY_SUPPORTED
            && self.roots == current.roots
            && self.exclude == current.exclude
            && self.histories == current.histories)
            .then_some(self.event_id)
    }

    /// Saves `index` with this mark. The mark goes first and comes back
    /// last, so a mark never claims an index newer than the one saved.
    pub fn save_with(&self, index: &Index) -> std::io::Result<()> {
        let _ = std::fs::remove_file(Self::path());
        index.save(&Index::default_path())?;
        let json = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        std::fs::write(Self::path(), json)
    }
}

/// Removes the saved index and its mark.
pub fn remove_saved() {
    let _ = std::fs::remove_file(ScanMark::path());
    let _ = std::fs::remove_file(Index::default_path());
}

/// Paths under `config`'s roots changed since event `since`, as FSEvents
/// recorded them. `None` if the history can't tell (events were dropped,
/// too many changed, or it didn't answer in time): only a rescan can catch up.
pub fn changes_since(config: &SearchConfig, since: u64) -> Option<Vec<PathBuf>> {
    let roots = config.root_paths();
    let excludes = Excludes::new(&config.exclude);
    let (tx, rx) = mpsc::channel();
    let _stream = EventStream::start(&roots, Some(since), 0.0, move |events, _| {
        for event in events {
            let _ = tx.send(event);
        }
    })?;
    let own_dir = Index::default_path().parent().map(|p| p.to_path_buf());
    let mut changed = HashSet::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(10)).ok()? {
            Event::HistoryDone => return Some(changed.into_iter().collect()),
            Event::Rescan(_) => return None,
            Event::Changed(path) => {
                if !own_dir.as_ref().is_some_and(|d| path.starts_with(d)) && !excludes.skips(&path, &roots) {
                    changed.insert(path);
                }
                if changed.len() > MAX_REPLAYED {
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_only_where_the_platform_keeps_history() {
        let config = SearchConfig::default();
        let saved = ScanMark::at(&config, 42);
        let same = ScanMark::at(&config, 99);
        // Linux has no change history: a saved mark must never claim one, or
        // the app would skip the rescan and miss everything changed since.
        assert_eq!(saved.replay_id(&same), fsevents::REPLAY_SUPPORTED.then_some(42));
        let mut other = SearchConfig::default();
        other.roots.push("/elsewhere".into());
        assert_eq!(saved.replay_id(&ScanMark::at(&other, 99)), None);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn no_replay_without_fsevents() {
        assert!(!fsevents::REPLAY_SUPPORTED);
        assert!(EventStream::start(&[], Some(1), 0.0, |_, _| {}).is_none());
    }
}
