//! The filename index.
//!
//! Entries are stored as parallel arrays ("struct of arrays") rather than a
//! tree of objects: names live in one big string, and each entry refers to
//! its parent folder by number. That keeps a million files in tens of
//! megabytes and lets searches stream through memory at full speed.
//!
//! Entries are sorted by lowercase name when the index is built, so a
//! search that scans entries in order yields results already sorted.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::os::macos::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
const PACKAGE_EXTENSIONS: &[&str] = &["app", "bundle", "framework", "plugin", "kext", "xcarchive", "photoslibrary"];

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

    fn excludes(&self, path: &Path, name: &str) -> bool {
        self.names.iter().any(|n| n == name) || self.paths.iter().any(|p| p == path)
    }
}

#[derive(Default)]
pub struct Index {
    names: String,
    name_ends: Vec<u32>,
    /// Lowercased names (may differ in byte length for non-ASCII names).
    lower: String,
    lower_ends: Vec<u32>,
    parents: Vec<u32>,
    flags: Vec<u8>,
    sizes: Vec<u64>,
    mtimes: Vec<i64>,
    /// Unix seconds when the scan finished.
    pub built_at: i64,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index").field("len", &self.len()).field("built_at", &self.built_at).finish()
    }
}

fn slice<'a>(buf: &'a str, ends: &[u32], id: u32) -> &'a str {
    let id = id as usize;
    let start = if id == 0 { 0 } else { ends[id - 1] as usize };
    &buf[start..ends[id] as usize]
}

impl Index {
    pub fn len(&self) -> usize {
        self.parents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    /// The entry's name. Root entries are named by their full path.
    pub fn name(&self, id: u32) -> &str {
        slice(&self.names, &self.name_ends, id)
    }

    pub fn lower_name(&self, id: u32) -> &str {
        slice(&self.lower, &self.lower_ends, id)
    }

    pub fn parent(&self, id: u32) -> Option<u32> {
        Some(self.parents[id as usize]).filter(|&p| p != NO_PARENT)
    }

    pub fn flags(&self, id: u32) -> u8 {
        self.flags[id as usize]
    }

    pub fn is_dir(&self, id: u32) -> bool {
        self.flags(id) & FLAG_DIR != 0
    }

    pub fn is_hidden(&self, id: u32) -> bool {
        self.flags(id) & FLAG_HIDDEN != 0
    }

    pub fn size(&self, id: u32) -> u64 {
        self.sizes[id as usize]
    }

    /// Modification time in Unix seconds.
    pub fn mtime(&self, id: u32) -> i64 {
        self.mtimes[id as usize]
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

    /// First entry whose lowercase name is `>= key` (entries are sorted).
    fn lower_bound(&self, key: &str) -> u32 {
        let (mut lo, mut hi) = (0u32, self.len() as u32);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.lower_name(mid) < key { lo = mid + 1 } else { hi = mid }
        }
        lo
    }

    /// The entry for an indexed path, found by binary search on its name.
    pub fn find(&self, path: &Path) -> Option<u32> {
        let name = crate::paths::display_name(path).to_lowercase();
        // A root entry's name is its full path, so try that too.
        let full = path.to_string_lossy().to_lowercase();
        let mut chain = Vec::new();
        let mut buf = String::new();
        for key in [&name, &full] {
            let mut id = self.lower_bound(key);
            while (id as usize) < self.len() && self.lower_name(id) == key.as_str() {
                buf.clear();
                self.write_path(id, &mut chain, &mut buf);
                if Path::new(&buf) == path {
                    return Some(id);
                }
                id += 1;
            }
        }
        None
    }

    /// A listing entry for `id`, built from indexed data (no disk access).
    pub fn entry(&self, id: u32) -> crate::listing::Entry {
        let flags = self.flags(id);
        crate::listing::Entry {
            name: self.name(id).to_string(),
            path: self.path(id),
            is_dir: flags & FLAG_DIR != 0,
            is_package: flags & FLAG_PACKAGE != 0,
            is_symlink: flags & FLAG_SYMLINK != 0,
            is_hidden: flags & FLAG_HIDDEN != 0,
            size: self.size(id),
            modified: u64::try_from(self.mtime(id)).ok().map(|s| UNIX_EPOCH + std::time::Duration::from_secs(s)),
        }
    }

    // ---- Building -------------------------------------------------------

    /// Scans `roots` in parallel. `progress` counts entries found so far;
    /// setting `cancel` stops the scan early (returning a partial index).
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
        let mut index = flat.into_sorted_index();
        index.built_at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        index
    }

    // ---- Persistence ----------------------------------------------------

    const MAGIC: &'static [u8; 8] = b"FNDIDX01";

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let mut w = BufWriter::new(fs::File::create(&tmp)?);
        w.write_all(Self::MAGIC)?;
        w.write_all(&self.built_at.to_le_bytes())?;
        for len in [self.len(), self.names.len(), self.lower.len()] {
            w.write_all(&(len as u64).to_le_bytes())?;
        }
        w.write_all(self.names.as_bytes())?;
        w.write_all(self.lower.as_bytes())?;
        for v in [&self.name_ends, &self.lower_ends, &self.parents] {
            for x in v.iter() {
                w.write_all(&x.to_le_bytes())?;
            }
        }
        w.write_all(&self.flags)?;
        for x in &self.sizes {
            w.write_all(&x.to_le_bytes())?;
        }
        for x in &self.mtimes {
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
        Ok(Index { names, name_ends, lower, lower_ends, parents, flags, sizes, mtimes, built_at })
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

fn scan_children(dir: &Path, excludes: &Excludes, progress: &AtomicUsize, cancel: &AtomicBool) -> Vec<Node> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
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
    progress.fetch_add(nodes.len(), Ordering::Relaxed);

    // Recurse into subfolders in parallel (work-stealing across all cores).
    let children: Vec<Vec<Node>> =
        subdirs.par_iter().map(|&i| scan_children(&dir.join(&nodes[i].name), excludes, progress, cancel)).collect();
    for (i, kids) in subdirs.into_iter().zip(children) {
        nodes[i].children = kids;
    }
    nodes
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

    fn into_sorted_index(self) -> Index {
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

        let mut index = Index {
            names: String::with_capacity(self.names.iter().map(String::len).sum()),
            lower: String::with_capacity(lowered.iter().map(String::len).sum()),
            name_ends: Vec::with_capacity(n),
            lower_ends: Vec::with_capacity(n),
            parents: Vec::with_capacity(n),
            flags: Vec::with_capacity(n),
            sizes: Vec::with_capacity(n),
            mtimes: Vec::with_capacity(n),
            built_at: 0,
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
