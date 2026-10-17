//! Searching without the index: walk the folders in parallel and test each
//! item as it's found, like `fd`. Slower than the index for big trees but
//! needs no index, sees changes the instant they happen, and works anywhere
//! (other volumes, network shares, folders the index excludes).
//!
//! Fast because most queries only need names: an item's type comes from the
//! directory listing itself, and size or date is looked up only when the
//! query asks for it (or for a match, to show it).

use std::cell::OnceCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;

use super::index::{Excludes, PACKAGE_EXTENSIONS};
use super::query::Query;
use super::run::{Ctx, Scratch, SearchOptions, Subject, Tri, read_text_file};
use crate::listing::Entry;

/// Live progress, shared with the UI.
#[derive(Default)]
pub struct LiveProgress {
    /// Items tested so far.
    pub checked: AtomicUsize,
    /// Matches so far, unsorted (the UI shows them while the walk runs).
    pub found: Mutex<Vec<Entry>>,
}

/// Searches everything under `roots` (not the roots themselves). Hidden
/// items (names starting with ".") and their contents are skipped unless
/// `opts.include_hidden` or the query asks for them; packages like apps are
/// matched but not entered, and `excludes` apply below the roots, all as in
/// the index. Returns every match sorted like indexed results, or `None` if
/// `cancel` was set.
pub fn live_search(
    roots: &[PathBuf],
    query: &Query,
    opts: &SearchOptions,
    excludes: &Excludes,
    progress: &LiveProgress,
    cancel: &AtomicBool,
) -> Option<Vec<Entry>> {
    let walk = Walk { query, opts, excludes, progress, cancel, include_hidden: opts.include_hidden || query.wants_hidden };
    roots.par_iter().for_each(|root| walk.dir(root));
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    let mut found = std::mem::take(&mut *progress.found.lock().unwrap());
    sort_like_index(&mut found);
    Some(found)
}

/// By lowercase name, then name, then path (as indexed results come out).
pub fn sort_like_index(entries: &mut [Entry]) {
    entries.sort_by_cached_key(|e| (e.name.to_lowercase(), e.name.clone(), e.path.clone()));
}

struct Walk<'a> {
    query: &'a Query,
    opts: &'a SearchOptions,
    excludes: &'a Excludes,
    progress: &'a LiveProgress,
    cancel: &'a AtomicBool,
    include_hidden: bool,
}

impl Walk<'_> {
    fn dir(&self, dir: &Path) {
        let Ok(read) = fs::read_dir(dir) else { return };
        let mut subdirs = Vec::new();
        let mut scratch = Scratch::default();
        let mut checked = 0;
        for item in read.flatten() {
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
            let name = item.file_name().to_string_lossy().into_owned();
            let path = item.path();
            if (!self.include_hidden && name.starts_with('.')) || self.excludes.excludes(&path, &name) {
                continue;
            }
            // From the directory listing: no extra lookup per item.
            let Ok(file_type) = item.file_type() else { continue };
            let package = file_type.is_dir()
                && Path::new(&name).extension().and_then(|e| e.to_str()).is_some_and(|ext| {
                    PACKAGE_EXTENSIONS.iter().any(|p| p.eq_ignore_ascii_case(ext))
                });
            let found = Found { lower: name.to_lowercase(), name, path, is_dir: file_type.is_dir() && !package, meta: OnceCell::new() };
            checked += 1;
            if self.matches(&found, &mut scratch)
                && let Ok(entry) = Entry::from_path(&found.path)
            {
                self.progress.found.lock().unwrap().push(entry);
            }
            if found.is_dir {
                subdirs.push(found.path);
            }
        }
        self.progress.checked.fetch_add(checked, Ordering::Relaxed);
        subdirs.par_iter().for_each(|d| self.dir(d));
    }

    fn matches(&self, found: &Found, scratch: &mut Scratch) -> bool {
        match (Ctx { subject: found, content: None }).test(&self.query.root, scratch) {
            Tri::Yes => true,
            Tri::No => false,
            // Depends on the contents: read the file (text files within the limit).
            Tri::Maybe => {
                if found.is_dir || found.size() > self.opts.max_content_bytes {
                    return false;
                }
                let Some(bytes) = read_text_file(&found.path, self.opts.max_content_bytes) else { return false };
                (Ctx { subject: found, content: Some(&bytes) }).test(&self.query.root, scratch) == Tri::Yes
            }
        }
    }
}

/// An item met during the walk; size and date are looked up on first use.
struct Found {
    name: String,
    lower: String,
    path: PathBuf,
    is_dir: bool,
    meta: OnceCell<Option<fs::Metadata>>,
}

impl Found {
    fn meta(&self) -> Option<&fs::Metadata> {
        self.meta.get_or_init(|| fs::symlink_metadata(&self.path).ok()).as_ref()
    }
}

impl Subject for Found {
    fn name(&self) -> &str {
        &self.name
    }
    fn lower_name(&self) -> &str {
        &self.lower
    }
    fn write_path(&self, _: &mut Vec<u32>, out: &mut String) {
        out.push_str(&self.path.to_string_lossy());
    }
    fn is_dir(&self) -> bool {
        self.is_dir
    }
    fn size(&self) -> u64 {
        // Like the index: folders and packages have no size of their own.
        self.meta().filter(|m| !m.is_dir()).map_or(0, |m| m.len())
    }
    fn mtime(&self) -> i64 {
        self.meta()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::index::tests::{sample_index, sample_tree};
    use crate::search::{parse, search};

    /// Paths matched by a live search and by an indexed search scoped to the
    /// same folder, in result order.
    fn both(root: &Path, q: &str, include_hidden: bool) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let query = parse(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        let ex = Excludes::new(&["**/node_modules".to_string()]);
        let opts = SearchOptions { include_hidden, ..Default::default() };
        let live = live_search(&[root.to_path_buf()], &query, &opts, &ex, &LiveProgress::default(), &AtomicBool::new(false))
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        let index = sample_index(root);
        let scoped = SearchOptions { under: index.find(root), ..opts };
        let indexed = search(&index, &query, &scoped, &AtomicUsize::new(0), &AtomicBool::new(false)).unwrap();
        (live, indexed.ids.iter().map(|&id| index.path(id)).collect())
    }

    #[test]
    fn matches_indexed_search() {
        let tmp = sample_tree();
        let root = tmp.path();
        let queries = [
            "", "img", "IMG", "*.jpg", "ext:png;jpg", "size:>1mb", "folder:find", "file:find", "findest/src",
            "regex:^img_\\d+\\.", "ext:jpg | ext:md", "ext:png;jpg !IMG", "dm:today ext:txt", "settings", "Tool",
            "content:todo", "content:TODO ext:txt", "!content:todo ext:rs;txt;md", "hidden: settings", "Info.plist",
        ];
        for q in queries {
            let (live, indexed) = both(root, q, false);
            assert_eq!(live, indexed, "query {q:?}");
        }
        let (live, indexed) = both(root, "", true);
        assert_eq!(live, indexed, "everything, hidden included");
        assert!(live.iter().any(|p| p.ends_with(".config/settings.toml")));
    }

    #[test]
    fn counts_items_and_can_be_cancelled() {
        let tmp = sample_tree();
        let query = parse("img").unwrap();
        let progress = LiveProgress::default();
        let ex = Excludes::new(&[]);
        let found = live_search(&[tmp.path().to_path_buf()], &query, &SearchOptions::default(), &ex, &progress, &AtomicBool::new(false));
        assert_eq!(found.unwrap().len(), 2);
        assert!(progress.checked.load(Ordering::Relaxed) >= 10);

        let cancelled = live_search(&[tmp.path().to_path_buf()], &query, &SearchOptions::default(), &ex, &LiveProgress::default(), &AtomicBool::new(true));
        assert!(cancelled.is_none());
    }

    #[test]
    fn searches_inside_an_excluded_folder_when_asked_to() {
        // E.g. searching "This folder" in ~/Library, which the index leaves out.
        let tmp = sample_tree();
        let photos = tmp.path().join("Photos");
        let ex = Excludes::new(&[photos.to_string_lossy().into_owned()]);
        let query = parse("img").unwrap();
        let found = live_search(&[photos], &query, &SearchOptions::default(), &ex, &LiveProgress::default(), &AtomicBool::new(false));
        assert_eq!(found.unwrap().len(), 2);
    }

    #[test]
    fn sees_new_files_at_once() {
        let tmp = sample_tree();
        std::fs::write(tmp.path().join("Photos/just-now.txt"), "x").unwrap();
        let query = parse("just-now").unwrap();
        let found = live_search(&[tmp.path().to_path_buf()], &query, &SearchOptions::default(), &Excludes::new(&[]), &LiveProgress::default(), &AtomicBool::new(false));
        assert_eq!(found.unwrap().len(), 1);
    }
}
