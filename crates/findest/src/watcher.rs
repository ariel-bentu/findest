//! Bridges FSEvents (via `notify`) to the UI thread as a stream of changed paths.

use std::path::{Path, PathBuf};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

pub struct FsWatcher {
    watcher: RecommendedWatcher,
    /// Watched permanently (holds the config file).
    pinned: PathBuf,
    current: Option<PathBuf>,
}

impl FsWatcher {
    pub fn new(pinned_dir: &Path) -> notify::Result<(FsWatcher, UnboundedReceiver<PathBuf>)> {
        let (tx, rx) = unbounded();
        let mut watcher = notify::recommended_watcher(move |result| forward(&tx, result))?;
        watcher.watch(pinned_dir, RecursiveMode::NonRecursive)?;
        Ok((FsWatcher { watcher, pinned: pinned_dir.to_path_buf(), current: None }, rx))
    }

    /// Switches the non-pinned watch to `dir` (or to nothing).
    pub fn watch_dir(&mut self, dir: Option<&Path>) {
        if self.current.as_deref() == dir {
            return;
        }
        if let Some(old) = self.current.take()
            && old != self.pinned
        {
            let _ = self.watcher.unwatch(&old);
        }
        if let Some(dir) = dir {
            if dir != self.pinned && self.watcher.watch(dir, RecursiveMode::NonRecursive).is_err() {
                return;
            }
            self.current = Some(dir.to_path_buf());
        }
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
