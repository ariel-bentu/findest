//! Sources: hierarchies listed by a program the user configures (an adapter),
//! browsed like folders. The protocol is documented in `docs/sources.md`.
//!
//! Findest runs `command… <verb> <args>` once per request (`list`, `read`,
//! `login`). A source is mounted at a folder in the cache
//! (`~/Library/Caches/findest/sources/<id>`): entries get real paths there, so
//! selection, history and sessions work as for any folder, and `read` fetches
//! a file to its own path before it's opened or previewed.
//!
//! Nothing fails silently: errors come back as [`SourceError`], and problems
//! that don't stop a listing (a bad entry, a subfolder that can't be listed)
//! come back as warnings next to the rows.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::listing::{Entry, SortOrder, TreeRow};

/// Exit code for "not signed in" (BSD `EX_NOPERM`).
pub const NOT_SIGNED_IN: i32 = 77;

/// Listings older than this are fetched again when a folder is shown.
const LISTING_TTL: Duration = Duration::from_secs(60);
/// Fetched files older than this are fetched again when opened.
const FILE_TTL: Duration = Duration::from_secs(30);
/// How long `list` and `read` may take before the adapter is stopped.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// `login` usually waits for the user in a browser.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_TREE_DEPTH: usize = 32;
/// File name of the "Load more…" row at the end of a paged folder.
const MORE_NAME: &str = ".findest-load-more";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// Stable; names the cache folder, so sessions and history survive restarts.
    pub id: String,
    /// Shown in the sidebar and the path bar.
    pub name: String,
    /// The adapter, plus its own arguments; the verb is appended.
    pub command: Vec<String>,
    /// The folder shown first.
    #[serde(default = "default_root")]
    pub root: String,
}

fn default_root() -> String {
    "/".into()
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceError {
    /// The adapter asked to sign in (exit 77).
    NotSignedIn { id: String, name: String },
    Failed(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::NotSignedIn { name, .. } => write!(f, "Not signed in to {name}"),
            SourceError::Failed(message) => f.write_str(message),
        }
    }
}

#[derive(Deserialize)]
struct WireListing {
    entries: Vec<WireEntry>,
    #[serde(rename = "continue")]
    next: Option<String>,
}

#[derive(Deserialize)]
struct WireEntry {
    name: String,
    kind: String,
    #[serde(rename = "ref")]
    reference: String,
    label: Option<String>,
    size: Option<u64>,
    modified: Option<String>,
    has_children: Option<bool>,
}

/// A folder's listing as far as it's been fetched.
#[derive(Clone)]
struct Listing {
    at: Instant,
    /// Entries, and whether each folder has something inside.
    rows: Vec<(Entry, bool)>,
    /// The adapter's token for the next page, if there's more.
    next: Option<String>,
    /// Problems with entries the adapter sent.
    warnings: Vec<String>,
}

#[derive(Default)]
struct State {
    sources: Vec<Source>,
    /// Each listed item's ref, by its path in the cache.
    refs: HashMap<PathBuf, String>,
    /// Listed files (as opposed to folders).
    files: HashSet<PathBuf>,
    listings: HashMap<PathBuf, Listing>,
    fetched: HashMap<PathBuf, Instant>,
}

pub struct Sources {
    mounts: PathBuf,
    state: Mutex<State>,
}

static GLOBAL: LazyLock<Sources> = LazyLock::new(|| {
    let caches = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    Sources::new(caches.join("findest/sources"))
});

/// The sources from the config, shared by all windows.
pub fn global() -> &'static Sources {
    &GLOBAL
}

/// Rows of a source folder, plus problems that didn't stop the listing.
pub struct SourceTree {
    pub rows: Vec<TreeRow>,
    pub warnings: Vec<String>,
}

impl Sources {
    pub fn new(mounts: PathBuf) -> Sources {
        Sources { mounts, state: Mutex::new(State::default()) }
    }

    /// Replaces the configured sources (on config load) and returns problems
    /// with them. Invalid sources are left out.
    pub fn set(&self, sources: Vec<Source>) -> Vec<String> {
        let mut problems = Vec::new();
        let mut ids = HashSet::new();
        let valid: Vec<Source> = sources
            .into_iter()
            .filter(|s| {
                let problem = if s.id.is_empty() || s.id.contains('/') || s.id.starts_with('.') {
                    Some(format!("source \"{}\": id must be a non-empty name without \"/\"", s.name))
                } else if !ids.insert(s.id.clone()) {
                    Some(format!("source id \"{}\" is used twice", s.id))
                } else if s.command.is_empty() {
                    Some(format!("source \"{}\": command is empty", s.id))
                } else {
                    None
                };
                problems.extend(problem.clone());
                problem.is_none()
            })
            .collect();
        let mut state = self.state.lock().unwrap();
        if state.sources == valid {
            return problems;
        }
        for source in &valid {
            let root = self.root_path(source);
            if let Err(e) = std::fs::create_dir_all(&root) {
                problems.push(format!("source \"{}\": can't create {}: {e}", source.id, root.display()));
            }
        }
        *state = State { sources: valid, ..State::default() };
        problems
    }

    pub fn sources(&self) -> Vec<Source> {
        self.state.lock().unwrap().sources.clone()
    }

    pub fn mount_path(&self, source: &Source) -> PathBuf {
        self.mounts.join(&source.id)
    }

    /// Where the sidebar opens a source.
    pub fn root_path(&self, source: &Source) -> PathBuf {
        let mut path = self.mount_path(source);
        path.extend(source.root.split('/').filter(|s| !s.is_empty()));
        path
    }

    /// The source `path` is in, if any.
    pub fn source_at(&self, path: &Path) -> Option<Source> {
        let id = self.mount_id(path)?;
        self.state.lock().unwrap().sources.iter().find(|s| s.id == id).cloned()
    }

    fn mount_id<'a>(&self, path: &'a Path) -> Option<&'a str> {
        path.strip_prefix(&self.mounts).ok()?.components().next()?.as_os_str().to_str()
    }

    /// True for paths inside a configured source.
    pub fn contains(&self, path: &Path) -> bool {
        self.source_at(path).is_some()
    }

    /// For a path in the cache of a source that's no longer configured (a
    /// restored session, history): that source's id.
    pub fn orphan(&self, path: &Path) -> Option<String> {
        let id = self.mount_id(path)?;
        (!self.contains(path)).then(|| id.to_string())
    }

    /// True for the "Load more…" row of a paged folder.
    pub fn is_more_row(&self, path: &Path) -> bool {
        path.file_name().is_some_and(|n| n == MORE_NAME) && self.contains(path)
    }

    /// A folder's ref: the path inside the mount, `/`-separated.
    fn folder_ref(&self, source: &Source, dir: &Path) -> String {
        let rel = dir.strip_prefix(self.mount_path(source)).unwrap_or(Path::new(""));
        let parts: Vec<&str> = rel.iter().filter_map(|p| p.to_str()).collect();
        format!("/{}", parts.join("/"))
    }

    /// Drops cached listings and files at and under `dir` (⌘R).
    pub fn invalidate(&self, dir: &Path) {
        let mut state = self.state.lock().unwrap();
        state.listings.retain(|path, _| !path.starts_with(dir));
        state.fetched.retain(|path, _| !path.starts_with(dir));
    }

    fn source_for(&self, path: &Path) -> Result<Source, SourceError> {
        self.source_at(path).ok_or_else(|| SourceError::Failed(format!("{} isn't in a source", path.display())))
    }

    /// A folder's listing: from the cache if it's recent, else its first page.
    fn listing(&self, dir: &Path) -> Result<Listing, SourceError> {
        if let Some(listing) = self.state.lock().unwrap().listings.get(dir)
            && listing.at.elapsed() < LISTING_TTL
        {
            return Ok(listing.clone());
        }
        let mut listing = Listing { at: Instant::now(), rows: Vec::new(), next: None, warnings: Vec::new() };
        self.fetch_page(dir, &mut listing)?;
        // Folders exist in the cache so they can be navigated to like any other.
        if let Err(e) = std::fs::create_dir_all(dir) {
            return Err(SourceError::Failed(format!("Can't create {}: {e}", dir.display())));
        }
        self.state.lock().unwrap().listings.insert(dir.to_path_buf(), listing.clone());
        Ok(listing)
    }

    /// Lists a folder (its first page, or as many as loaded with "Load more").
    pub fn list(&self, dir: &Path) -> Result<Vec<(Entry, bool)>, SourceError> {
        self.listing(dir).map(|l| l.rows)
    }

    /// Fetches the next page of `dir` into its cached listing ("Load more…").
    pub fn load_more(&self, dir: &Path) -> Result<(), SourceError> {
        let mut listing = self.listing(dir)?;
        if listing.next.is_none() {
            return Ok(());
        }
        self.fetch_page(dir, &mut listing)?;
        self.state.lock().unwrap().listings.insert(dir.to_path_buf(), listing);
        Ok(())
    }

    /// Runs `list` for the page after `listing.next` (or the first) and adds it.
    fn fetch_page(&self, dir: &Path, listing: &mut Listing) -> Result<(), SourceError> {
        let source = self.source_for(dir)?;
        let mut args = vec![self.folder_ref(&source, dir)];
        args.extend(listing.next.take());
        let out = run(&source, "list", &args, REQUEST_TIMEOUT)?;
        let page: WireListing = serde_json::from_slice(&out).map_err(|e| {
            SourceError::Failed(format!("{}: `list` printed something that isn't a listing: {e}", source.name))
        })?;
        let mut names: HashSet<String> = listing.rows.iter().map(|(e, _)| e.name.clone()).collect();
        let mut refs = Vec::new();
        let mut files = Vec::new();
        for wire in page.entries {
            let problem = if wire.name.is_empty() || wire.name.contains('/') || wire.name == "." || wire.name == ".." || wire.name == MORE_NAME {
                Some(format!("bad name {:?}", wire.name))
            } else if wire.kind != "folder" && wire.kind != "file" {
                Some(format!("{:?}: kind must be \"folder\" or \"file\", not {:?}", wire.name, wire.kind))
            } else if !names.insert(wire.name.clone()) {
                Some(format!("{:?} is listed twice", wire.name))
            } else {
                None
            };
            if let Some(problem) = problem {
                listing.warnings.push(format!("{}: skipped an entry: {problem}", source.name));
                continue;
            }
            let modified = wire.modified.as_deref().and_then(|text| {
                let time = parse_time(text);
                if time.is_none() {
                    listing.warnings.push(format!("{}: {:?}: modified isn't an RFC 3339 time: {text:?}", source.name, wire.name));
                }
                time
            });
            let is_dir = wire.kind == "folder";
            let path = dir.join(&wire.name);
            refs.push((path.clone(), wire.reference));
            if !is_dir {
                files.push(path.clone());
            }
            let entry = Entry {
                is_hidden: wire.name.starts_with('.'),
                name: wire.name,
                path,
                is_dir,
                is_package: false,
                is_symlink: false,
                size: wire.size.unwrap_or(0),
                modified,
                color: None,
                label: wire.label,
            };
            listing.rows.push((entry, is_dir && wire.has_children.unwrap_or(true)));
        }
        listing.next = page.next.filter(|t| !t.is_empty());
        let mut state = self.state.lock().unwrap();
        state.refs.extend(refs);
        state.files.extend(files);
        Ok(())
    }

    /// Like [`crate::listing::read_tree`]: `root`, with every expanded folder's
    /// contents inlined after its row. A paged folder ends with a "Load more…"
    /// row. Subfolders that can't be listed are reported as warnings.
    pub fn read_tree(
        &self,
        root: &Path,
        expanded: &HashSet<PathBuf>,
        show_hidden: bool,
        order: SortOrder,
    ) -> Result<SourceTree, SourceError> {
        // Expanded folders are listed in parallel first (each is a round trip);
        // the walk below reads them from the cache, and reports their errors.
        std::thread::scope(|scope| {
            for dir in expanded.iter().filter(|d| d.starts_with(root) && *d != root) {
                scope.spawn(move || self.listing(dir));
            }
            self.listing(root)
        })?;
        let mut tree = SourceTree { rows: Vec::new(), warnings: Vec::new() };
        self.walk(root, 0, expanded, show_hidden, order, &mut tree)?;
        Ok(tree)
    }

    fn walk(
        &self,
        dir: &Path,
        depth: usize,
        expanded: &HashSet<PathBuf>,
        show_hidden: bool,
        order: SortOrder,
        tree: &mut SourceTree,
    ) -> Result<(), SourceError> {
        let listing = self.listing(dir)?;
        tree.warnings.extend(listing.warnings);
        let children: HashMap<PathBuf, bool> = listing.rows.iter().map(|(e, c)| (e.path.clone(), *c)).collect();
        let mut entries: Vec<Entry> = listing.rows.into_iter().map(|(e, _)| e).filter(|e| show_hidden || !e.is_hidden).collect();
        crate::listing::sort_entries(&mut entries, order);
        for entry in entries {
            let has_children = children.get(&entry.path).copied().unwrap_or(false);
            let recurse = has_children && depth < MAX_TREE_DEPTH && expanded.contains(&entry.path);
            let path = entry.path.clone();
            tree.rows.push(TreeRow { entry, depth, has_children });
            if recurse && let Err(e) = self.walk(&path, depth + 1, expanded, show_hidden, order, tree) {
                tree.warnings.push(format!("Can't list {}: {e}", crate::paths::display_name(&path)));
            }
        }
        if listing.next.is_some() {
            tree.rows.push(TreeRow { entry: more_row(dir), depth, has_children: false });
        }
        Ok(())
    }

    /// True for a listed file that hasn't been fetched recently.
    pub fn needs_fetch(&self, path: &Path) -> bool {
        let state = self.state.lock().unwrap();
        state.files.contains(path) && !state.fetched.get(path).is_some_and(|at| at.elapsed() < FILE_TTL)
    }

    /// Writes a listed file's content to its path in the cache.
    pub fn fetch(&self, path: &Path) -> Result<(), SourceError> {
        let source = self.source_for(path)?;
        let name = crate::paths::display_name(path);
        let reference = self.state.lock().unwrap().refs.get(path).cloned().ok_or_else(|| {
            SourceError::Failed(format!("{name} isn't listed any more; refresh the folder (⌘R)"))
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SourceError::Failed(format!("Can't create {}: {e}", parent.display())))?;
        }
        // A stale copy must not pass for the new one.
        let _ = std::fs::remove_file(path);
        run(&source, "read", &[reference, path.to_string_lossy().into_owned()], REQUEST_TIMEOUT)?;
        if !path.is_file() {
            return Err(SourceError::Failed(format!("{}: `read` finished without writing {name}", source.name)));
        }
        self.state.lock().unwrap().fetched.insert(path.to_path_buf(), Instant::now());
        Ok(())
    }

    /// Fetches every file in `paths` that needs it.
    pub fn fetch_all(&self, paths: &[PathBuf]) -> Result<(), SourceError> {
        let wanted: Vec<&PathBuf> = paths.iter().filter(|p| self.needs_fetch(p)).collect();
        std::thread::scope(|scope| {
            let tasks: Vec<_> = wanted.iter().map(|p| scope.spawn(move || self.fetch(p))).collect();
            tasks.into_iter().try_for_each(|t| t.join().unwrap_or_else(|_| Err(SourceError::Failed("fetching crashed".into()))))
        })
    }

    /// Runs the adapter's sign-in; drops cached listings so they're fetched again.
    pub fn login(&self, id: &str) -> Result<(), SourceError> {
        let source = self.sources().into_iter().find(|s| s.id == id).ok_or_else(|| SourceError::Failed(format!("No source \"{id}\"")))?;
        run(&source, "login", &[], LOGIN_TIMEOUT)?;
        self.invalidate(&self.mount_path(&source));
        Ok(())
    }

    /// The source whose mount folder is `path`, for breadcrumbs and titles.
    pub fn mount_named(&self, path: &Path) -> Option<(PathBuf, String)> {
        let source = self.source_at(path)?;
        Some((self.mount_path(&source), source.name))
    }
}

fn more_row(dir: &Path) -> Entry {
    Entry {
        name: "Load more…".into(),
        path: dir.join(MORE_NAME),
        is_dir: false,
        is_package: false,
        is_symlink: false,
        is_hidden: false,
        size: 0,
        modified: None,
        color: None,
        label: Some("Open to list more".into()),
    }
}

/// Runs `command… verb args…` and returns its stdout. Stopped after `timeout`.
fn run(source: &Source, verb: &str, args: &[String], timeout: Duration) -> Result<Vec<u8>, SourceError> {
    let Some((program, own_args)) = source.command.split_first() else {
        return Err(SourceError::Failed(format!("{}: no command configured", source.name)));
    };
    let mut child = Command::new(crate::paths::expand_tilde(program))
        .args(own_args)
        .arg(verb)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| SourceError::Failed(format!("{}: can't run {program}: {e}", source.name)))?;
    // Read both pipes while waiting, so a chatty adapter can't block on a full pipe.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SourceError::Failed(format!(
                    "{}: `{verb}` didn't finish within {timeout:?} and was stopped",
                    source.name
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(SourceError::Failed(format!("{}: {e}", source.name))),
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if status.success() {
        return Ok(stdout);
    }
    if status.code() == Some(NOT_SIGNED_IN) {
        return Err(SourceError::NotSignedIn { id: source.id.clone(), name: source.name.clone() });
    }
    let stderr = String::from_utf8_lossy(&stderr);
    let message = match stderr.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
        Some(line) => line.to_string(),
        None => match status.code() {
            Some(code) => format!("`{verb}` failed (exit {code}) without saying why"),
            None => format!("`{verb}` was killed"),
        },
    };
    Err(SourceError::Failed(format!("{}: {message}", source.name)))
}

fn parse_time(text: &str) -> Option<SystemTime> {
    let time = chrono::DateTime::parse_from_rfc3339(text).ok()?;
    Some(SystemTime::from(time))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listing::SortKey;
    use std::os::unix::fs::PermissionsExt;

    const ORDER: SortOrder = SortOrder { key: SortKey::Name, ascending: true, folders_first: true };

    /// The example adapter from docs/sources.md: the docs stay true.
    fn example() -> Source {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/static-source");
        Source { id: "ex".into(), name: "Example".into(), command: vec![script.to_string_lossy().into()], root: "/".into() }
    }

    /// An adapter for the failure cases.
    fn broken(dir: &Path) -> Source {
        let script = dir.join("adapter");
        std::fs::write(
            &script,
            r#"#!/bin/sh
case "$1 $2" in
  "list /") echo '{"entries":[{"name":"missing","kind":"folder","ref":"/missing"}]}' ;;
  "list /locked") exit 77 ;;
  "list /quiet") exit 3 ;;
  "list /slow") sleep 5 ;;
  "list /junk") echo 'not json' ;;
  "list /bad") echo '{"entries":[{"name":"a/b","kind":"file","ref":"x"},{"name":"t","kind":"thing","ref":"x"},
    {"name":"f","kind":"file","ref":"F","modified":"yesterday"},{"name":"f","kind":"file","ref":"G"}]}' ;;
  "read F") ;;
  *) echo "no such thing: $*" >&2; exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Source { id: "b".into(), name: "Broken".into(), command: vec![script.to_string_lossy().into()], root: "/".into() }
    }

    fn mounted(source: &Source) -> (tempfile::TempDir, Sources, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let sources = Sources::new(tmp.path().join("mounts"));
        assert!(sources.set(vec![source.clone()]).is_empty());
        let root = sources.root_path(source);
        (tmp, sources, root)
    }

    fn names(tree: &SourceTree) -> Vec<(&str, usize)> {
        tree.rows.iter().map(|r| (r.entry.name.as_str(), r.depth)).collect()
    }

    #[test]
    fn lists_expanded_folders_and_pages() {
        let (_tmp, sources, root) = mounted(&example());
        let docs = root.join("docs");
        let tree = sources.read_tree(&root, &HashSet::from([docs.clone()]), false, ORDER).unwrap();
        assert_eq!(names(&tree), vec![("big", 0), ("docs", 0), ("notes.md", 1), ("readme.txt", 1), ("hello.txt", 0)]);
        assert!(tree.warnings.is_empty(), "{:?}", tree.warnings);
        assert!(docs.is_dir(), "listed folders exist in the cache");
        let hello = &tree.rows[4].entry;
        assert_eq!(hello.kind(), "Greeting");
        assert!(hello.modified.is_some());

        // "big" comes in pages: a "Load more…" row until the last one.
        let big = root.join("big");
        let tree = sources.read_tree(&big, &HashSet::new(), false, ORDER).unwrap();
        assert_eq!(names(&tree), vec![("item-1.txt", 0), ("item-2.txt", 0), ("Load more…", 0)]);
        assert!(sources.is_more_row(&tree.rows[2].entry.path));
        sources.load_more(&big).unwrap();
        let tree = sources.read_tree(&big, &HashSet::new(), false, ORDER).unwrap();
        assert_eq!(names(&tree), vec![("item-1.txt", 0), ("item-2.txt", 0), ("item-3.txt", 0)]);
    }

    #[test]
    fn fetches_files_to_their_path() {
        let (_tmp, sources, root) = mounted(&example());
        sources.list(&root).unwrap();
        let hello = root.join("hello.txt");
        assert!(sources.needs_fetch(&hello));
        assert!(!sources.needs_fetch(&root.join("docs")), "folders aren't fetched");
        sources.fetch_all(std::slice::from_ref(&hello)).unwrap();
        assert_eq!(std::fs::read_to_string(&hello).unwrap(), "Hello from a static source.\n");
        assert!(!sources.needs_fetch(&hello));
    }

    #[test]
    fn reports_every_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let (_mount, sources, root) = mounted(&broken(tmp.path()));
        let err = |dir: &str| sources.list(&root.join(dir)).unwrap_err().to_string();

        assert_eq!(sources.list(&root.join("locked")).unwrap_err(), SourceError::NotSignedIn { id: "b".into(), name: "Broken".into() });
        assert_eq!(err("missing"), "Broken: no such thing: list /missing");
        assert_eq!(err("quiet"), "Broken: `list` failed (exit 3) without saying why");
        assert!(err("junk").starts_with("Broken: `list` printed something that isn't a listing"));

        let tree = sources.read_tree(&root.join("bad"), &HashSet::new(), false, ORDER).unwrap();
        assert_eq!(names(&tree), vec![("f", 0)]);
        assert_eq!(tree.warnings.len(), 4, "{:?}", tree.warnings);

        let f = root.join("bad/f");
        assert_eq!(sources.fetch(&f).unwrap_err().to_string(), "Broken: `read` finished without writing f");

        // An expanded subfolder that fails is reported, not just left empty.
        let tree = sources.read_tree(&root, &HashSet::from([root.join("missing")]), false, ORDER).unwrap();
        assert_eq!(names(&tree), vec![("missing", 0)]);
        assert_eq!(tree.warnings, vec!["Can't list missing: Broken: no such thing: list /missing"]);
    }

    #[test]
    fn stops_an_adapter_that_hangs() {
        let tmp = tempfile::tempdir().unwrap();
        let source = broken(tmp.path());
        let started = Instant::now();
        let err = run(&source, "list", &["/slow".into()], Duration::from_millis(200)).unwrap_err();
        assert_eq!(err.to_string(), "Broken: `list` didn't finish within 200ms and was stopped");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn checks_the_config() {
        let tmp = tempfile::tempdir().unwrap();
        let sources = Sources::new(tmp.path().join("mounts"));
        let source = |id: &str, command: &[&str]| Source {
            id: id.into(),
            name: id.into(),
            command: command.iter().map(|s| s.to_string()).collect(),
            root: "/".into(),
        };
        let problems = sources.set(vec![source("a", &["x"]), source("a", &["y"]), source("b", &[]), source("c/d", &["x"])]);
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert_eq!(sources.sources().len(), 1);

        let gone = tmp.path().join("mounts/old/x");
        assert_eq!(sources.orphan(&gone).as_deref(), Some("old"));
        assert_eq!(sources.orphan(&tmp.path().join("mounts/a/x")), None);
    }
}
