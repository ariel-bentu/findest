//! Searching a source (see [`crate::sources`]). The adapter's `search` verb
//! does the work when it has one: it gets hints from the query and may
//! return more than matches, since every result is tested against the full
//! query here. Adapters without `search` are walked folder by folder with
//! `list`, within limits that are reported when reached.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use rayon::prelude::*;
use serde_json::json;

use super::live::{LiveProgress, sort_like_index};
use super::query::{Node, Pred, Query, Text};
use super::run::{Ctx, Scratch, SearchOptions, Subject, Tri, read_text_file};
use crate::listing::Entry;
use crate::sources::{SourceError, Sources};

/// Pages of `search` results followed before stopping.
const MAX_SEARCH_PAGES: usize = 50;
/// Limits for walking a source without `search`.
const MAX_WALK_FOLDERS: usize = 1000;
const MAX_WALK_TIME: Duration = Duration::from_secs(60);
/// `content:` fetches files to read them; at most this many per search.
const MAX_CONTENT_FETCHES: usize = 200;

pub struct SourceSearch {
    /// Matches, sorted like indexed results.
    pub entries: Vec<Entry>,
    /// How it was searched, e.g. "Searched by URM canary".
    pub how: String,
    /// Limits reached and problems met; the results may be incomplete.
    pub warnings: Vec<String>,
}

/// Searches `dir` (a folder in a source) and everything under it. Matches
/// are also added to `progress.found` as they're found. `None` if cancelled.
pub fn search_source(
    sources: &Sources,
    dir: &Path,
    query: &Query,
    text: &str,
    opts: &SearchOptions,
    progress: &LiveProgress,
    cancel: &AtomicBool,
) -> Result<Option<SourceSearch>, SourceError> {
    let source = sources.source_at(dir).ok_or_else(|| SourceError::Failed(format!("{} isn't in a source", dir.display())))?;
    let tester = Tester {
        sources,
        query,
        opts,
        include_hidden: opts.include_hidden || query.wants_hidden,
        fetches: AtomicUsize::new(0),
        content_capped: AtomicBool::new(false),
    };
    let mut warnings = Vec::new();
    let how = match by_adapter(sources, dir, &tester, &hints(query, text), progress, cancel, &mut warnings) {
        Ok(()) => format!("Searched by {}", source.name),
        Err(SourceError::NotSupported { .. }) => {
            warnings.clear();
            walk(sources, dir, &tester, progress, cancel, &mut warnings)?;
            format!("{} has no search, so its folders were listed one by one", source.name)
        }
        Err(e) => return Err(e),
    };
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    if tester.content_capped.load(Ordering::Relaxed) {
        warnings.push(format!("content: read only the first {MAX_CONTENT_FETCHES} files; narrow the search to check the rest"));
    }
    let mut entries = std::mem::take(&mut *progress.found.lock().unwrap());
    sort_like_index(&mut entries);
    Ok(Some(SourceSearch { entries, how, warnings }))
}

/// What an adapter can use to narrow its search; it may ignore any of it.
/// `names` are lowercase substrings every match's name contains.
fn hints(query: &Query, text: &str) -> String {
    let leaves: Vec<&Node> = match &query.root {
        Node::And(nodes) => nodes.iter().collect(),
        node => vec![node],
    };
    let mut names = Vec::new();
    let mut extensions = Vec::new();
    let mut kind = None;
    for node in leaves {
        match node {
            Node::Leaf(Pred::Name(Text::Sub { needle, .. })) => names.push(needle.to_lowercase()),
            Node::Leaf(Pred::Ext(exts)) if extensions.is_empty() => extensions = exts.clone(),
            Node::Leaf(Pred::IsDir(dir)) => kind = Some(if *dir { "folder" } else { "file" }),
            _ => {}
        }
    }
    json!({ "query": text, "names": names, "extensions": extensions, "kind": kind }).to_string()
}

fn by_adapter(
    sources: &Sources,
    dir: &Path,
    tester: &Tester,
    hints: &str,
    progress: &LiveProgress,
    cancel: &AtomicBool,
    warnings: &mut Vec<String>,
) -> Result<(), SourceError> {
    let mut seen = HashSet::new();
    let mut next = None;
    for page in 0.. {
        if page == MAX_SEARCH_PAGES {
            warnings.push(format!("Stopped after {MAX_SEARCH_PAGES} pages of results; narrow the search"));
            break;
        }
        let result = sources.search_page(dir, hints, next.take(), &mut seen)?;
        warnings.extend(result.warnings);
        let rows: Vec<Entry> = result.rows.into_iter().map(|(e, _)| e).collect();
        progress.checked.fetch_add(rows.len(), Ordering::Relaxed);
        let matches: Vec<Entry> = rows.into_par_iter().filter(|e| tester.matches(e)).collect();
        progress.found.lock().unwrap().extend(matches);
        match result.next {
            Some(token) if !cancel.load(Ordering::Relaxed) => next = Some(token),
            _ => break,
        }
    }
    Ok(())
}

/// Lists `dir` and its subfolders level by level (each level in parallel).
fn walk(
    sources: &Sources,
    dir: &Path,
    tester: &Tester,
    progress: &LiveProgress,
    cancel: &AtomicBool,
    warnings: &mut Vec<String>,
) -> Result<(), SourceError> {
    let started = Instant::now();
    let mut frontier = vec![dir.to_path_buf()];
    let mut listed = 0;
    let mut paged = 0;
    while !frontier.is_empty() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if listed + frontier.len() > MAX_WALK_FOLDERS || started.elapsed() > MAX_WALK_TIME {
            warnings.push(format!(
                "Stopped after listing {listed} folders ({} s); search a subfolder to go deeper",
                started.elapsed().as_secs()
            ));
            break;
        }
        let results: Vec<(PathBuf, _)> =
            frontier.par_iter().map(|d| (d.clone(), sources.listing_for_search(d))).collect();
        listed += frontier.len();
        let mut next = Vec::new();
        for (folder, result) in results {
            let listing = match result {
                Ok(listing) => listing,
                // The folder searched must be listable; below it, report and go on.
                Err(e) if folder == dir => return Err(e),
                Err(e) => {
                    warnings.push(format!("Can't list {}: {e}", crate::paths::display_name(&folder)));
                    continue;
                }
            };
            warnings.extend(listing.warnings);
            paged += usize::from(listing.next.is_some());
            let rows: Vec<Entry> = listing
                .rows
                .into_iter()
                .map(|(e, _)| e)
                .filter(|e| tester.include_hidden || !e.is_hidden)
                .collect();
            progress.checked.fetch_add(rows.len(), Ordering::Relaxed);
            next.extend(rows.iter().filter(|e| e.is_dir).map(|e| e.path.clone()));
            let matches: Vec<Entry> = rows.into_par_iter().filter(|e| tester.matches(e)).collect();
            progress.found.lock().unwrap().extend(matches);
        }
        frontier = next;
    }
    if paged > 0 {
        warnings.push(format!(
            "{paged} {} more items than were searched (only their first page is listed)",
            if paged == 1 { "folder has" } else { "folders have" }
        ));
    }
    Ok(())
}

struct Tester<'a> {
    sources: &'a Sources,
    query: &'a Query,
    opts: &'a SearchOptions,
    include_hidden: bool,
    fetches: AtomicUsize,
    content_capped: AtomicBool,
}

impl Tester<'_> {
    fn matches(&self, entry: &Entry) -> bool {
        if !self.include_hidden && entry.is_hidden {
            return false;
        }
        let item = Item {
            entry,
            lower: entry.name.to_lowercase(),
            path: self.sources.source_path(&entry.path).unwrap_or_else(|| entry.path.to_string_lossy().into_owned()),
        };
        let mut scratch = Scratch::default();
        match (Ctx { subject: &item, content: None }).test(&self.query.root, &mut scratch) {
            Tri::Yes => true,
            Tri::No => false,
            // Depends on the contents: fetch the file, within a budget.
            Tri::Maybe => {
                if entry.is_dir {
                    return false;
                }
                if self.fetches.fetch_add(1, Ordering::Relaxed) >= MAX_CONTENT_FETCHES {
                    self.content_capped.store(true, Ordering::Relaxed);
                    return false;
                }
                if self.sources.needs_fetch(&entry.path) && self.sources.fetch(&entry.path).is_err() {
                    return false;
                }
                let Some(bytes) = read_text_file(&entry.path, self.opts.max_content_bytes) else { return false };
                (Ctx { subject: &item, content: Some(&bytes) }).test(&self.query.root, &mut scratch) == Tri::Yes
            }
        }
    }
}

/// A source entry, as the query sees it: its path is the one inside the source.
struct Item<'a> {
    entry: &'a Entry,
    lower: String,
    path: String,
}

impl Subject for Item<'_> {
    fn name(&self) -> &str {
        &self.entry.name
    }
    fn lower_name(&self) -> &str {
        &self.lower
    }
    fn write_path(&self, _: &mut Vec<u32>, out: &mut String) {
        out.push_str(&self.path);
    }
    fn is_dir(&self) -> bool {
        self.entry.is_dir
    }
    fn size(&self) -> u64 {
        self.entry.size
    }
    fn mtime(&self) -> i64 {
        self.entry.modified.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::parse;
    use crate::sources::Source;
    use std::os::unix::fs::PermissionsExt;

    fn mount(command: Vec<String>) -> (tempfile::TempDir, Sources, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let sources = Sources::new(tmp.path().join("mounts"));
        let source = Source { id: "s".into(), name: "S".into(), command, root: "/".into(), protocol_version: 1 };
        assert!(sources.set(vec![source.clone()]).is_empty());
        let root = sources.root_path(&source);
        (tmp, sources, root)
    }

    fn find(sources: &Sources, dir: &Path, q: &str) -> SourceSearch {
        let query = parse(q).unwrap();
        let opts = SearchOptions::default();
        search_source(sources, dir, &query, q, &opts, &LiveProgress::default(), &AtomicBool::new(false)).unwrap().unwrap()
    }

    fn names(found: &SourceSearch) -> Vec<&str> {
        found.entries.iter().map(|e| e.name.as_str()).collect()
    }

    fn example() -> Vec<String> {
        vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/static-source").to_string_lossy().into()]
    }

    #[test]
    fn uses_the_adapters_search() {
        let (_tmp, sources, root) = mount(example());
        let found = find(&sources, &root, "item ext:txt");
        // The example returns every name containing "item"; ext: is checked here.
        assert_eq!(names(&found), vec!["item-1.txt", "item-2.txt", "item-3.txt"]);
        assert_eq!(found.how, "Searched by S");
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
        assert!(found.entries[0].path.ends_with("big/item-1.txt"), "placed in their folder");

        // Results can be opened: they're known to the source.
        assert!(sources.needs_fetch(&found.entries[0].path));
        assert_eq!(names(&find(&sources, &root, "content:\"Item 2\"")), vec!["item-2.txt"]);
        assert_eq!(names(&find(&sources, &root.join("docs"), "e")), vec!["notes.md", "readme.txt"]);
    }

    #[test]
    fn walks_adapters_without_search() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("adapter");
        std::fs::write(
            &script,
            r#"#!/bin/sh
case "$1 $2" in
  "list /") echo '{"entries":[{"name":"a","kind":"folder","ref":"/a"},{"name":"broken","kind":"folder","ref":"/broken"},{"name":"x.txt","kind":"file","ref":"x"}]}' ;;
  "list /a") echo '{"entries":[{"name":"ax.txt","kind":"file","ref":"ax"}],"continue":"2"}' ;;
  "search "*) echo "search isn't supported" >&2; exit 64 ;;
  *) echo "nope: $*" >&2; exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (_mount, sources, root) = mount(vec![script.to_string_lossy().into()]);

        let found = find(&sources, &root, "x");
        assert_eq!(names(&found), vec!["ax.txt", "x.txt"]);
        assert_eq!(found.how, "S has no search, so its folders were listed one by one");
        assert_eq!(
            found.warnings,
            vec![
                "Can't list broken: S: nope: list /broken".to_string(),
                "1 folder has more items than were searched (only their first page is listed)".to_string(),
            ]
        );
        assert_eq!(names(&find(&sources, &root, "path:/a/")), vec!["ax.txt"]);
    }

    #[test]
    fn hints_name_what_adapters_can_use() {
        let hint = |q: &str| serde_json::from_str::<serde_json::Value>(&hints(&parse(q).unwrap(), q)).unwrap();
        assert_eq!(hint("Deploy ext:yaml"), json!({"query": "Deploy ext:yaml", "names": ["deploy"], "extensions": ["yaml"], "kind": null}));
        assert_eq!(hint("file: web | api")["names"], json!([]), "alternatives can't narrow by name");
        assert_eq!(hint("folder: prod")["kind"], json!("folder"));
    }
}
