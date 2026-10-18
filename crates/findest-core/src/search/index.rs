//! The filename index.
//!
//! Entries are stored as parallel arrays ("struct of arrays") rather than a
//! tree of objects: names live in one big string, and each entry refers to
//! its parent folder by number. That keeps a million files in tens of
//! megabytes and lets searches stream through memory at full speed.
//!
//! Entries are sorted by lowercase name when the index is built, so a
//! search that scans entries in order yields results already sorted.
//!
//! A full scan is expensive, so between scans changes reported by the file
//! watcher are applied as a small overlay (`with_changes`): entries added
//! since the scan get ids after the scanned ones, and removed entries are
//! marked rather than deleted. Ids therefore stay valid until the next scan.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::os::macos::fs::MetadataExt;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rayon::prelude::*;

pub const NO_PARENT: u32 = u32::MAX;

pub const FLAG_DIR: u8 = 1;
pub const FLAG_HIDDEN: u8 = 2;
pub const FLAG_SYMLINK: u8 = 4;
/// App bundles and similar: shown as files, not descended into.
pub const FLAG_PACKAGE: u8 = 8;
/// Hidden itself or inside a hidden folder (e.g. anything under `~/.config`).
pub const FLAG_HIDDEN_TREE: u8 = 16;

const UF_HIDDEN: u32 = 0x8000;
pub(crate) const PACKAGE_EXTENSIONS: &[&str] = &["app", "bundle", "framework", "plugin", "kext", "xcarchive", "photoslibrary"];

/// Paths and folder names left out of the index.
#[derive(Clone, Debug, Default)]
pub struct Excludes {
    paths: Vec<PathBuf>,
    names: Vec<String>,
}

impl Excludes {
    /// `"**/name"` excludes a folder name anywhere; anything else is a path
    /// (with `~` expanded).
    pub fn new(patterns: &[String]) -> Excludes {
        let mut ex = Excludes::default();
        for p in patterns {
            match p.strip_prefix("**/") {
                Some(name) => ex.names.push(name.trim_end_matches('/').to_string()),
                None => ex.paths.push(crate::paths::expand_tilde(p.trim_end_matches('/'))),
            }
        }
        ex
    }

    pub(crate) fn excludes(&self, path: &Path, name: &str) -> bool {
        self.names.iter().any(|n| n == name) || self.paths.iter().any(|p| p == path)
    }

    /// Whether a scan of `roots` would skip `path`: it is outside every root,
    /// or something between its deepest root and itself is excluded. (Roots
    /// are scanned even when they sit under an excluded path.)
    pub fn skips(&self, path: &Path, roots: &[PathBuf]) -> bool {
        let Some(root) = roots.iter().filter(|r| path.starts_with(r)).max_by_key(|r| r.components().count()) else {
            return true;
        };
        path.ancestors()
            .take_while(|p| *p != root.as_path())
            .any(|p| p.file_name().is_some_and(|n| self.excludes(p, &n.to_string_lossy())))
    }
}

/// The arrays from a full scan, sorted by name.
#[derive(Default)]
struct Packed {
    names: String,
    name_ends: Vec<u32>,
    /// Lowercased names (may differ in byte length for non-ASCII names).
    lower: String,
    lower_ends: Vec<u32>,
    parents: Vec<u32>,
    flags: Vec<u8>,
    sizes: Vec<u64>,
    mtimes: Vec<i64>,
}

/// Changes since the full scan. Added entries are unsorted and few.
#[derive(Clone, Default)]
struct Delta {
    names: Vec<String>,
    lower: Vec<String>,
    parents: Vec<u32>,
    flags: Vec<u8>,
    sizes: Vec<u64>,
    mtimes: Vec<i64>,
    /// One bit per id (scanned or added): the entry no longer exists.
    removed: Vec<u64>,
    removed_count: usize,
}

/// Cheap to clone: the scanned arrays are shared.
#[derive(Clone, Default)]
pub struct Index {
    base: Arc<Packed>,
    delta: Arc<Delta>,
    /// Unix seconds when the full scan finished.
    pub built_at: i64,
    /// Unix seconds when changes were last applied (`built_at` until then).
    pub updated_at: i64,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index").field("len", &self.len()).field("built_at", &self.built_at).finish_non_exhaustive()
    }
}

fn slice<'a>(buf: &'a str, ends: &[u32], id: u32) -> &'a str {
    let id = id as usize;
    let start = if id == 0 { 0 } else { ends[id - 1] as usize };
    &buf[start..ends[id] as usize]
}

impl Index {
    /// One past the highest id, counting removed entries (ids are `0..len()`).
    pub fn len(&self) -> usize {
        self.base_len() + self.delta.parents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live_count() == 0
    }

    /// Entries that still exist.
    pub fn live_count(&self) -> usize {
        self.len() - self.delta.removed_count
    }

    /// Entries added or removed since the full scan.
    pub fn changed_count(&self) -> usize {
        self.delta.parents.len() + self.delta.removed_count
    }

    /// Whether `other` comes from the same full scan (so ids agree).
    pub fn same_scan(&self, other: &Index) -> bool {
        Arc::ptr_eq(&self.base, &other.base)
    }

    fn base_len(&self) -> usize {
        self.base.parents.len()
    }

    /// Position in the delta arrays, for an id added since the scan.
    fn added(&self, id: u32) -> Option<usize> {
        (id as usize).checked_sub(self.base_len())
    }

    /// The entry was deleted (or replaced) since the full scan.
    pub fn is_removed(&self, id: u32) -> bool {
        self.delta.is_removed(id)
    }

    /// The entry's name. Root entries are named by their full path.
    pub fn name(&self, id: u32) -> &str {
        match self.added(id) {
            None => slice(&self.base.names, &self.base.name_ends, id),
            Some(i) => &self.delta.names[i],
        }
    }

    pub fn lower_name(&self, id: u32) -> &str {
        match self.added(id) {
            None => slice(&self.base.lower, &self.base.lower_ends, id),
            Some(i) => &self.delta.lower[i],
        }
    }

    pub fn parent(&self, id: u32) -> Option<u32> {
        let p = match self.added(id) {
            None => self.base.parents[id as usize],
            Some(i) => self.delta.parents[i],
        };
        Some(p).filter(|&p| p != NO_PARENT)
    }

    pub fn flags(&self, id: u32) -> u8 {
        match self.added(id) {
            None => self.base.flags[id as usize],
            Some(i) => self.delta.flags[i],
        }
    }

    pub fn is_dir(&self, id: u32) -> bool {
        self.flags(id) & FLAG_DIR != 0
    }

    pub fn is_hidden(&self, id: u32) -> bool {
        self.flags(id) & FLAG_HIDDEN != 0
    }

    pub fn size(&self, id: u32) -> u64 {
        match self.added(id) {
            None => self.base.sizes[id as usize],
            Some(i) => self.delta.sizes[i],
        }
    }

    /// Modification time in Unix seconds.
    pub fn mtime(&self, id: u32) -> i64 {
        match self.added(id) {
            None => self.base.mtimes[id as usize],
            Some(i) => self.delta.mtimes[i],
        }
    }

    /// Search order: by lowercase name, then by name.
    pub fn cmp_ids(&self, a: u32, b: u32) -> CmpOrdering {
        self.lower_name(a).cmp(self.lower_name(b)).then_with(|| self.name(a).cmp(self.name(b)))
    }

    /// Merges id lists that are each in search order.
    pub fn merge_sorted<T>(&self, a: Vec<T>, b: Vec<T>, id: impl Fn(&T) -> u32) -> Vec<T> {
        let mut out = Vec::with_capacity(a.len() + b.len());
        let (mut a, mut b) = (a.into_iter().peekable(), b.into_iter().peekable());
        loop {
            let take_a = match (a.peek(), b.peek()) {
                (Some(x), Some(y)) => self.cmp_ids(id(x), id(y)) != CmpOrdering::Greater,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            out.extend(if take_a { a.next() } else { b.next() });
        }
        out
    }

    /// Ids added since the full scan (not in search order).
    pub fn added_ids(&self) -> std::ops::Range<u32> {
        self.base_len() as u32..self.len() as u32
    }

    /// Ids from the full scan, in search order.
    pub fn scanned_ids(&self) -> std::ops::Range<u32> {
        0..self.base_len() as u32
    }

    /// Appends the full path of `id` to `out` (reusable buffer, no allocation per call).
    pub fn write_path(&self, id: u32, chain: &mut Vec<u32>, out: &mut String) {
        chain.clear();
        let mut cur = Some(id);
        while let Some(c) = cur {
            chain.push(c);
            cur = self.parent(c);
        }
        for (i, &c) in chain.iter().rev().enumerate() {
            if i > 0 && !out.ends_with('/') {
                out.push('/');
            }
            out.push_str(self.name(c));
        }
    }

    pub fn path(&self, id: u32) -> PathBuf {
        let mut out = String::new();
        self.write_path(id, &mut Vec::new(), &mut out);
        PathBuf::from(out)
    }

    /// True if `ancestor` is `id` itself or one of its parent folders.
    pub fn is_under(&self, id: u32, ancestor: u32) -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            if c == ancestor {
                return true;
            }
            cur = self.parent(c);
        }
        false
    }

    /// Hidden itself or inside a hidden folder.
    pub fn in_hidden_tree(&self, id: u32) -> bool {
        self.flags(id) & FLAG_HIDDEN_TREE != 0
    }

    /// First scanned entry whose lowercase name is `>= key` (they are sorted).
    fn lower_bound(&self, key: &str) -> u32 {
        let (mut lo, mut hi) = (0u32, self.base_len() as u32);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.lower_name(mid) < key { lo = mid + 1 } else { hi = mid }
        }
        lo
    }

    /// The entry for an indexed path: binary search on its name among the
    /// scanned entries, then a look through the (few) added ones.
    pub fn find(&self, path: &Path) -> Option<u32> {
        let name = crate::paths::display_name(path).to_lowercase();
        // A root entry's name is its full path, so try that too.
        let full = path.to_string_lossy().to_lowercase();
        let mut chain = Vec::new();
        let mut buf = String::new();
        let mut is_match = |id: u32| {
            if self.is_removed(id) {
                return false;
            }
            buf.clear();
            self.write_path(id, &mut chain, &mut buf);
            Path::new(&buf) == path
        };
        for key in [&name, &full] {
            let start = self.lower_bound(key);
            let same = (start..self.base_len() as u32).take_while(|&id| self.lower_name(id) == key.as_str());
            let added = self.added_ids().filter(|&id| self.lower_name(id) == key.as_str());
            if let Some(id) = same.chain(added).find(|&id| is_match(id)) {
                return Some(id);
            }
        }
        None
    }

    /// A listing entry for `id`, built from indexed data (folders also read
    /// their color label).
    pub fn entry(&self, id: u32) -> crate::listing::Entry {
        let flags = self.flags(id);
        let path = self.path(id);
        crate::listing::Entry {
            name: self.name(id).to_string(),
            color: if flags & FLAG_DIR != 0 { crate::listing::read_color(&path) } else { None },
            path,
            is_dir: flags & FLAG_DIR != 0,
            is_package: flags & FLAG_PACKAGE != 0,
            is_symlink: flags & FLAG_SYMLINK != 0,
            is_hidden: flags & FLAG_HIDDEN != 0,
            size: self.size(id),
            modified: u64::try_from(self.mtime(id)).ok().map(|s| UNIX_EPOCH + std::time::Duration::from_secs(s)),
            label: None,
            mime_type: None,
        }
    }

    // ---- Building -------------------------------------------------------

    /// Scans `roots` in parallel (on the current rayon pool; see
    /// `in_background`). `progress` counts entries found so far; setting
    /// `cancel` stops the scan early (returning a partial index).
    pub fn build(roots: &[PathBuf], excludes: &Excludes, progress: &AtomicUsize, cancel: &AtomicBool) -> Index {
        let trees: Vec<Node> = roots
            .par_iter()
            .filter(|root| root.is_dir())
            .map(|root| Node {
                name: root.to_string_lossy().into_owned(),
                flags: FLAG_DIR,
                size: 0,
                mtime: mtime_of(&fs::metadata(root).ok()),
                children: scan_children(root, excludes, progress, cancel),
            })
            .collect();

        // Flatten depth-first into arrays, then sort by name.
        let mut flat = Flat::default();
        for tree in trees {
            flat.push(tree, NO_PARENT);
        }
        let built_at = unix_now();
        Index { base: Arc::new(flat.into_sorted()), delta: Arc::default(), built_at, updated_at: built_at }
    }

    // ---- Applying changes -----------------------------------------------

    /// The index with the folders around `paths` re-read from disk: each
    /// path's nearest indexed folder above it (and the path itself, if it is
    /// an indexed folder) is listed again and compared with the index. New
    /// folders are scanned in full and removed ones take their contents with
    /// them. Folder modification times aren't kept current (they change with
    /// every file inside); the next full scan refreshes them. `None` if
    /// nothing changed.
    pub fn with_changes(&self, paths: &[PathBuf], excludes: &Excludes) -> Option<Index> {
        let indexed_dir = |p: &Path| self.find(p).filter(|&id| self.is_dir(id));
        let mut dirs = HashSet::new();
        for path in paths {
            dirs.extend(indexed_dir(path));
            dirs.extend(path.ancestors().skip(1).find_map(indexed_dir));
        }
        if dirs.is_empty() {
            return None;
        }
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let found: Vec<(u32, u32)> = (0..self.len() as u32)
            .into_par_iter()
            .filter_map(|id| self.parent(id).filter(|p| dirs.contains(p)).map(|p| (p, id)))
            .filter(|&(_, id)| !self.is_removed(id))
            .collect();
        for (parent, id) in found {
            children.entry(parent).or_default().push(id);
        }

        let base_len = self.base_len();
        let mut delta = (*self.delta).clone();
        let mut removed_dirs = false;
        let no_progress = AtomicUsize::new(0);
        let no_cancel = AtomicBool::new(false);
        for dir in dirs {
            if delta.is_removed(dir) {
                continue; // replaced while re-reading its parent
            }
            let path = self.path(dir);
            let (mut nodes, subdirs) = read_entries(&path, excludes, &no_cancel);
            let mut indexed: HashMap<&str, u32> =
                children.get(&dir).into_iter().flatten().map(|&id| (self.name(id), id)).collect();
            let mut is_new = vec![true; nodes.len()];
            for (node, new) in nodes.iter().zip(&mut is_new) {
                if let Some(id) = indexed.remove(node.name.as_str()) {
                    if self.unchanged(id, node) {
                        *new = false;
                    } else {
                        delta.remove(id);
                        removed_dirs |= self.is_dir(id);
                    }
                }
            }
            for (_, id) in indexed {
                delta.remove(id);
                removed_dirs |= self.is_dir(id);
            }
            let new_subdirs: Vec<usize> = subdirs.into_iter().filter(|&i| is_new[i]).collect();
            fill_subdirs(&path, &mut nodes, &new_subdirs, excludes, &no_progress, &no_cancel);
            for (node, new) in nodes.into_iter().zip(is_new) {
                if new {
                    delta.add(node, dir, self.flags(dir), base_len);
                }
            }
        }
        if delta.parents.len() == self.delta.parents.len() && delta.removed_count == self.delta.removed_count {
            return None;
        }

        let mut next = Index { delta: Arc::new(delta), updated_at: unix_now(), ..self.clone() };
        if removed_dirs {
            // Everything under a removed folder goes too.
            let gone: Vec<u32> = (0..next.len() as u32)
                .into_par_iter()
                .filter(|&id| !next.is_removed(id) && next.ancestors(id).any(|a| next.is_removed(a)))
                .collect();
            let delta = Arc::get_mut(&mut next.delta).expect("not shared yet");
            for id in gone {
                delta.remove(id);
            }
        }
        Some(next)
    }

    fn ancestors(&self, id: u32) -> impl Iterator<Item = u32> + '_ {
        std::iter::successors(self.parent(id), |&p| self.parent(p))
    }

    /// The indexed entry still matches what's on disk.
    fn unchanged(&self, id: u32, node: &Node) -> bool {
        const KIND: u8 = FLAG_DIR | FLAG_PACKAGE | FLAG_SYMLINK | FLAG_HIDDEN;
        let flags = self.flags(id);
        flags & KIND == node.flags & KIND
            && (flags & FLAG_DIR != 0 || (self.size(id) == node.size && self.mtime(id) == node.mtime))
    }

    // ---- Persistence ----------------------------------------------------

    const MAGIC: &'static [u8; 8] = b"FNDIDX01";

    /// Saves the full scan. Changes applied since aren't saved: they are
    /// lost on quit anyway, so launch always rescans.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let b = &*self.base;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let mut w = BufWriter::new(fs::File::create(&tmp)?);
        w.write_all(Self::MAGIC)?;
        w.write_all(&self.built_at.to_le_bytes())?;
        for len in [self.base_len(), b.names.len(), b.lower.len()] {
            w.write_all(&(len as u64).to_le_bytes())?;
        }
        w.write_all(b.names.as_bytes())?;
        w.write_all(b.lower.as_bytes())?;
        for v in [&b.name_ends, &b.lower_ends, &b.parents] {
            for x in v.iter() {
                w.write_all(&x.to_le_bytes())?;
            }
        }
        w.write_all(&b.flags)?;
        for x in &b.sizes {
            w.write_all(&x.to_le_bytes())?;
        }
        for x in &b.mtimes {
            w.write_all(&x.to_le_bytes())?;
        }
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        fs::rename(tmp, path)
    }

    pub fn load(path: &Path) -> io::Result<Index> {
        let bytes = fs::read(path)?;
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "corrupt index");
        let mut r = Reader { bytes: &bytes, pos: 0 };
        if r.take(8).ok_or_else(bad)? != Self::MAGIC {
            return Err(bad());
        }
        let built_at = i64::from_le_bytes(r.array().ok_or_else(bad)?);
        let mut lens = [0usize; 3];
        for len in &mut lens {
            *len = u64::from_le_bytes(r.array().ok_or_else(bad)?) as usize;
        }
        let [n, names_len, lower_len] = lens;
        let names = String::from_utf8(r.take(names_len).ok_or_else(bad)?.to_vec()).map_err(|_| bad())?;
        let lower = String::from_utf8(r.take(lower_len).ok_or_else(bad)?.to_vec()).map_err(|_| bad())?;
        let u32s = |r: &mut Reader| -> Option<Vec<u32>> { (0..n).map(|_| r.array().map(u32::from_le_bytes)).collect() };
        let name_ends = u32s(&mut r).ok_or_else(bad)?;
        let lower_ends = u32s(&mut r).ok_or_else(bad)?;
        let parents = u32s(&mut r).ok_or_else(bad)?;
        let flags = r.take(n).ok_or_else(bad)?.to_vec();
        let sizes = (0..n).map(|_| r.array().map(u64::from_le_bytes)).collect::<Option<Vec<_>>>().ok_or_else(bad)?;
        let mtimes = (0..n).map(|_| r.array().map(i64::from_le_bytes)).collect::<Option<Vec<_>>>().ok_or_else(bad)?;
        // Validate offsets so later slicing can't panic.
        let valid_ends = |ends: &[u32], buf: &str| {
            let mut prev = 0u32;
            ends.iter().all(|&e| {
                let ok = e >= prev && (e as usize) <= buf.len() && buf.is_char_boundary(e as usize);
                prev = e;
                ok
            })
        };
        let valid_parents = parents.iter().all(|&p| p == NO_PARENT || (p as usize) < n);
        if !valid_ends(&name_ends, &names) || !valid_ends(&lower_ends, &lower) || !valid_parents {
            return Err(bad());
        }
        let base = Packed { names, name_ends, lower, lower_ends, parents, flags, sizes, mtimes };
        Ok(Index { base: Arc::new(base), delta: Arc::default(), built_at, updated_at: built_at })
    }

    /// `~/Library/Application Support/findest/index.bin`.
    pub fn default_path() -> PathBuf {
        crate::recents::Recents::path().with_file_name("index.bin")
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let out = self.bytes.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(out)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
}

impl Delta {
    fn is_removed(&self, id: u32) -> bool {
        self.removed.get(id as usize / 64).is_some_and(|w| w >> (id % 64) & 1 != 0)
    }

    fn remove(&mut self, id: u32) {
        let (word, bit) = (id as usize / 64, 1u64 << (id % 64));
        if self.removed.len() <= word {
            self.removed.resize(word + 1, 0);
        }
        if self.removed[word] & bit == 0 {
            self.removed[word] |= bit;
            self.removed_count += 1;
        }
    }

    /// Adds `node` and everything under it.
    fn add(&mut self, node: Node, parent: u32, parent_flags: u8, base_len: usize) {
        let id = (base_len + self.parents.len()) as u32;
        let mut flags = node.flags;
        if parent_flags & FLAG_HIDDEN_TREE != 0 || flags & FLAG_HIDDEN != 0 {
            flags |= FLAG_HIDDEN_TREE;
        }
        self.lower.push(node.name.to_lowercase());
        self.names.push(node.name);
        self.parents.push(parent);
        self.flags.push(flags);
        self.sizes.push(node.size);
        self.mtimes.push(node.mtime);
        for child in node.children {
            self.add(child, id, flags, base_len);
        }
    }
}

struct Node {
    name: String,
    flags: u8,
    size: u64,
    mtime: i64,
    children: Vec<Node>,
}

fn mtime_of(meta: &Option<fs::Metadata>) -> i64 {
    meta.as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The folder's entries (not descended into), and which of them are
/// folders to descend into.
fn read_entries(dir: &Path, excludes: &Excludes, cancel: &AtomicBool) -> (Vec<Node>, Vec<usize>) {
    let Ok(read) = fs::read_dir(dir) else { return Default::default() };
    let mut nodes = Vec::new();
    let mut subdirs = Vec::new();
    for item in read.flatten() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let name = item.file_name().to_string_lossy().into_owned();
        let path = item.path();
        if excludes.excludes(&path, &name) {
            continue;
        }
        // DirEntry::metadata doesn't follow symlinks.
        let meta = item.metadata().ok();
        let (mut flags, size) = match &meta {
            Some(m) if m.file_type().is_symlink() => (FLAG_SYMLINK, 0),
            Some(m) if m.is_dir() => (FLAG_DIR, 0),
            Some(m) => (0, m.len()),
            None => (0, 0),
        };
        if name.starts_with('.') || meta.as_ref().is_some_and(|m| m.st_flags() & UF_HIDDEN != 0) {
            flags |= FLAG_HIDDEN;
        }
        if flags & FLAG_DIR != 0 {
            let ext = Path::new(&name).extension().and_then(|e| e.to_str()).unwrap_or("");
            if PACKAGE_EXTENSIONS.iter().any(|p| p.eq_ignore_ascii_case(ext)) {
                flags = (flags & !FLAG_DIR) | FLAG_PACKAGE;
            } else {
                subdirs.push(nodes.len());
            }
        }
        nodes.push(Node { name, flags, size, mtime: mtime_of(&meta), children: Vec::new() });
    }
    (nodes, subdirs)
}

fn scan_children(dir: &Path, excludes: &Excludes, progress: &AtomicUsize, cancel: &AtomicBool) -> Vec<Node> {
    let (mut nodes, subdirs) = read_entries(dir, excludes, cancel);
    progress.fetch_add(nodes.len(), Ordering::Relaxed);
    fill_subdirs(dir, &mut nodes, &subdirs, excludes, progress, cancel);
    nodes
}

/// Recurses into subfolders in parallel (work-stealing across the pool).
fn fill_subdirs(dir: &Path, nodes: &mut [Node], subdirs: &[usize], excludes: &Excludes, progress: &AtomicUsize, cancel: &AtomicBool) {
    let children: Vec<Vec<Node>> =
        subdirs.par_iter().map(|&i| scan_children(&dir.join(&nodes[i].name), excludes, progress, cancel)).collect();
    for (&i, kids) in subdirs.iter().zip(children) {
        nodes[i].children = kids;
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Runs `f` on the indexing threads: few of them, at background priority,
/// so macOS keeps them on efficiency cores and throttles their disk access
/// whenever anything else needs it. Rayon calls inside `f` use this pool.
pub fn in_background<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    let pool = POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .thread_name(|i| format!("findest-index-{i}"))
            .start_handler(|_| unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0);
            })
            .build()
            .ok()
    });
    match pool {
        Some(pool) => pool.install(f),
        None => f(),
    }
}

#[derive(Default)]
struct Flat {
    names: Vec<String>,
    parents: Vec<u32>,
    flags: Vec<u8>,
    sizes: Vec<u64>,
    mtimes: Vec<i64>,
}

impl Flat {
    fn push(&mut self, node: Node, parent: u32) {
        let id = self.names.len() as u32;
        let parent_hidden = parent != NO_PARENT && self.flags[parent as usize] & FLAG_HIDDEN_TREE != 0;
        let mut flags = node.flags;
        if parent_hidden || flags & FLAG_HIDDEN != 0 {
            flags |= FLAG_HIDDEN_TREE;
        }
        self.names.push(node.name);
        self.parents.push(parent);
        self.flags.push(flags);
        self.sizes.push(node.size);
        self.mtimes.push(node.mtime);
        for child in node.children {
            self.push(child, id);
        }
    }

    fn into_sorted(self) -> Packed {
        let n = self.names.len();
        let lowered: Vec<String> = self.names.par_iter().map(|s| s.to_lowercase()).collect();
        let mut order: Vec<u32> = (0..n as u32).collect();
        order.par_sort_unstable_by(|&a, &b| {
            lowered[a as usize].cmp(&lowered[b as usize]).then_with(|| self.names[a as usize].cmp(&self.names[b as usize]))
        });
        // new_id[old] = position in sorted order
        let mut new_id = vec![0u32; n];
        for (new, &old) in order.iter().enumerate() {
            new_id[old as usize] = new as u32;
        }

        let mut index = Packed {
            names: String::with_capacity(self.names.iter().map(String::len).sum()),
            lower: String::with_capacity(lowered.iter().map(String::len).sum()),
            name_ends: Vec::with_capacity(n),
            lower_ends: Vec::with_capacity(n),
            parents: Vec::with_capacity(n),
            flags: Vec::with_capacity(n),
            sizes: Vec::with_capacity(n),
            mtimes: Vec::with_capacity(n),
        };
        for &old in &order {
            let old = old as usize;
            index.names.push_str(&self.names[old]);
            index.name_ends.push(index.names.len() as u32);
            index.lower.push_str(&lowered[old]);
            index.lower_ends.push(index.lower.len() as u32);
            let parent = self.parents[old];
            index.parents.push(if parent == NO_PARENT { NO_PARENT } else { new_id[parent as usize] });
            index.flags.push(self.flags[old]);
            index.sizes.push(self.sizes[old]);
            index.mtimes.push(self.mtimes[old]);
        }
        index
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small tree used by the search tests too.
    pub(crate) fn sample_tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("Projects/findest/src")).unwrap();
        fs::create_dir_all(r.join("Projects/findest/node_modules/pkg")).unwrap();
        fs::create_dir_all(r.join("Photos/2024")).unwrap();
        fs::create_dir_all(r.join(".config")).unwrap();
        fs::create_dir_all(r.join("Tool.app/Contents")).unwrap();
        fs::write(r.join("Projects/findest/src/main.rs"), "fn main() { /* TODO: search */ }").unwrap();
        fs::write(r.join("Projects/findest/README.md"), "# Findest\nfast file browser").unwrap();
        fs::write(r.join("Projects/findest/node_modules/pkg/index.js"), "x").unwrap();
        fs::write(r.join("Photos/2024/IMG_0001.jpg"), vec![0u8; 2_000_000]).unwrap();
        fs::write(r.join("Photos/2024/img_0002.PNG"), vec![0u8; 10]).unwrap();
        fs::write(r.join("Photos/notes.txt"), "remember the TODO list").unwrap();
        fs::write(r.join(".config/settings.toml"), "a = 1").unwrap();
        fs::write(r.join("Tool.app/Contents/Info.plist"), "").unwrap();
        tmp
    }

    pub(crate) fn sample_index(root: &Path) -> Index {
        let ex = Excludes::new(&["**/node_modules".to_string()]);
        Index::build(&[root.to_path_buf()], &ex, &AtomicUsize::new(0), &AtomicBool::new(false))
    }

    /// Live entries as (path, flags, size, mtime), by path.
    fn snapshot(index: &Index) -> Vec<(PathBuf, u8, u64, i64)> {
        let mut out: Vec<_> = (0..index.len() as u32)
            .filter(|&id| !index.is_removed(id))
            .map(|id| (index.path(id), index.flags(id), index.size(id), index.mtime(id)))
            .collect();
        out.sort();
        out
    }

    /// Like `snapshot`, minus folder mtimes (which `with_changes` doesn't keep current).
    fn comparable(index: &Index) -> Vec<(PathBuf, u8, u64, i64)> {
        snapshot(index).into_iter().map(|(p, f, s, m)| (p, f, s, if f & FLAG_DIR != 0 { 0 } else { m })).collect()
    }

    /// All names in search order, as a search would return them.
    fn search_names(index: &Index) -> Vec<String> {
        let query = crate::search::parse("hidden:").unwrap();
        let opts = crate::search::SearchOptions::default();
        let result = crate::search::search(index, &query, &opts, &AtomicUsize::new(0), &AtomicBool::new(false)).unwrap();
        result.ids.iter().map(|&id| index.name(id).to_string()).collect()
    }

    #[test]
    fn applies_changes_like_a_fresh_scan() {
        let tmp = sample_tree();
        let r = tmp.path();
        let ex = Excludes::new(&["**/node_modules".to_string()]);
        let index = sample_index(r);
        assert!(index.with_changes(&[r.join("Photos/notes.txt")], &ex).is_none(), "nothing changed");

        fs::write(r.join("Photos/new.txt"), "hi").unwrap();
        fs::remove_file(r.join("Photos/notes.txt")).unwrap();
        fs::write(r.join("Photos/2024/img_0002.PNG"), vec![0u8; 99]).unwrap();
        fs::create_dir_all(r.join("Docs/a/b")).unwrap();
        fs::write(r.join("Docs/a/b/c.txt"), "").unwrap();
        fs::remove_dir_all(r.join("Projects/findest/src")).unwrap();
        fs::rename(r.join("Photos/2024"), r.join("Photos/2025")).unwrap();
        fs::write(r.join(".config/new.toml"), "").unwrap();
        fs::write(r.join("Projects/findest/node_modules/pkg/new.js"), "").unwrap();
        let changed = [
            "Photos/new.txt",
            "Photos/notes.txt",
            "Photos/2024/img_0002.PNG",
            "Docs/a/b/c.txt", // only the deepest path: its new folders are found from the nearest indexed one
            "Projects/findest/src",
            "Photos/2024",
            "Photos/2025",
            ".config/new.toml",
            "Projects/findest/node_modules/pkg/new.js",
        ]
        .map(|p| r.join(p));
        let updated = index.with_changes(&changed, &ex).expect("changes found");
        let fresh = sample_index(r);
        assert_eq!(comparable(&updated), comparable(&fresh));
        assert_eq!(search_names(&updated), search_names(&fresh), "added entries are merged into name order");
        assert!(updated.find(&r.join("Docs/a/b/c.txt")).is_some());
        assert!(updated.find(&r.join("Photos/2024/IMG_0001.jpg")).is_none(), "moved away");
        assert!(updated.same_scan(&index) && updated.changed_count() > 0);

        // Changes on top of changes: remove a folder that was itself added.
        fs::remove_dir_all(r.join("Docs")).unwrap();
        let again = updated.with_changes(&[r.join("Docs")], &ex).expect("changes found");
        assert_eq!(comparable(&again), comparable(&sample_index(r)));
        assert_eq!(again.live_count(), sample_index(r).live_count());
    }

    #[test]
    fn skips_matches_what_a_scan_excludes() {
        let ex = Excludes::new(&["/h/Library".to_string(), "**/node_modules".to_string()]);
        let roots = [PathBuf::from("/h"), PathBuf::from("/h/Library/CloudStorage")];
        let skips = |p: &str| ex.skips(Path::new(p), &roots);
        assert!(!skips("/h/Documents/a.json"));
        assert!(skips("/h/Library/Caches/x"), "under an excluded path");
        assert!(skips("/h/Library"), "the excluded path itself");
        assert!(skips("/h/src/node_modules/pkg/index.js"), "excluded by name");
        assert!(!skips("/h/Library/CloudStorage/Drive/doc.txt"), "a root under an excluded path is scanned");
        assert!(skips("/elsewhere/file"), "outside every root");
    }

    #[test]
    fn builds_sorted_index_with_paths() {
        let tmp = sample_tree();
        let index = sample_index(tmp.path());
        let names: Vec<&str> = (0..index.len() as u32).map(|i| index.name(i)).collect();
        let mut sorted = names.clone();
        sorted.sort_by_key(|n| n.to_lowercase());
        assert_eq!(names, sorted, "entries are sorted by lowercase name");

        assert!(!names.contains(&"node_modules"), "excluded by name");
        assert!(!names.contains(&"Info.plist"), "packages aren't descended into");
        let app = names.iter().position(|n| *n == "Tool.app").unwrap() as u32;
        assert!(index.flags(app) & FLAG_PACKAGE != 0 && !index.is_dir(app));

        let main = names.iter().position(|n| *n == "main.rs").unwrap() as u32;
        assert_eq!(index.path(main), tmp.path().join("Projects/findest/src/main.rs"));
        let projects = index.find(&tmp.path().join("Projects")).unwrap();
        assert!(index.is_under(main, projects));
        assert_eq!(index.find(tmp.path()), Some(index.find(tmp.path()).unwrap()));

        let entry = index.entry(main);
        assert_eq!(entry.path, tmp.path().join("Projects/findest/src/main.rs"));
        assert!(!entry.is_dir && entry.modified.is_some());

        let settings = names.iter().position(|n| *n == "settings.toml").unwrap() as u32;
        assert!(index.in_hidden_tree(settings) && !index.is_hidden(settings));
        assert_eq!(index.size(names.iter().position(|n| *n == "IMG_0001.jpg").unwrap() as u32), 2_000_000);
    }

    #[test]
    fn save_and_load_round_trip() {
        let tmp = sample_tree();
        let index = sample_index(tmp.path());
        let file = tmp.path().join("index.bin");
        index.save(&file).unwrap();
        let loaded = Index::load(&file).unwrap();
        assert_eq!(loaded.len(), index.len());
        for id in 0..index.len() as u32 {
            assert_eq!(loaded.name(id), index.name(id));
            assert_eq!(loaded.path(id), index.path(id));
            assert_eq!(loaded.size(id), index.size(id));
        }
        fs::write(&file, b"FNDIDX01garbage").unwrap();
        assert!(Index::load(&file).is_err());
    }
}
