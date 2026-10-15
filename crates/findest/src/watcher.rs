//! Bridges FSEvents (via `notify`) to the UI thread as a stream of changed paths.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

pub struct FsWatcher {
    watcher: RecommendedWatcher,
    /// Watched permanently (holds the config file).
    pinned: PathBuf,
    current: HashSet<PathBuf>,
}

impl FsWatcher {
    pub fn new(pinned_dir: &Path) -> notify::Result<(FsWatcher, UnboundedReceiver<PathBuf>)> {
        let (tx, rx) = unbounded();
        let mut watcher = notify::recommended_watcher(move |result| forward(&tx, result))?;
        watcher.watch(pinned_dir, RecursiveMode::NonRecursive)?;
        Ok((FsWatcher { watcher, pinned: pinned_dir.to_path_buf(), current: HashSet::new() }, rx))
    }

    /// Watches exactly `dirs` (plus the pinned folder): the current folder and
    /// any expanded subfolders.
    pub fn watch_dirs(&mut self, dirs: HashSet<PathBuf>) {
        for old in self.current.difference(&dirs) {
            if *old != self.pinned {
                let _ = self.watcher.unwatch(old);
            }
        }
        let mut watching = HashSet::new();
        for dir in dirs {
            let ok = self.current.contains(&dir)
                || dir == self.pinned
                || self.watcher.watch(&dir, RecursiveMode::NonRecursive).is_ok();
            if ok {
                watching.insert(dir);
            }
        }
        self.current = watching;
    }
}

fn forward(tx: &UnboundedSender<PathBuf>, result: notify::Result<notify::Event>) {
    let Ok(event) = result else { return };
    if matches!(event.kind, EventKind::Access(_)) {
        return;
    }
    for path in event.paths {
        let _ = tx.unbounded_send(path);
    }
}
