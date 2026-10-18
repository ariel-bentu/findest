//! Running a query against the index.
//!
//! Pass 1 evaluates the query on index metadata in parallel, walking entries
//! in their (name-sorted) order so results come out sorted. Terms that need
//! file contents evaluate to "maybe" here (three-valued logic), so pass 2 only
//! reads the files that could still match.

use std::fs;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rayon::prelude::*;

use super::index::Index;
use super::query::{Node, Pred, Query};

#[derive(Clone, Debug)]
pub struct SearchOptions {
    /// Only entries inside this folder (an index entry id).
    pub under: Option<u32>,
    /// Include hidden files and anything inside hidden folders.
    pub include_hidden: bool,
    /// `content:` skips files larger than this.
    pub max_content_bytes: u64,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self { under: None, include_hidden: false, max_content_bytes: 20 * 1024 * 1024 }
    }
}

#[derive(Debug, Default)]
pub struct SearchResult {
    /// Matching entry ids, sorted by name.
    pub ids: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tri {
    No,
    Maybe,
    Yes,
}

impl Tri {
    fn not(self) -> Tri {
        match self {
            Tri::No => Tri::Yes,
            Tri::Maybe => Tri::Maybe,
            Tri::Yes => Tri::No,
        }
    }
}

impl From<bool> for Tri {
    fn from(b: bool) -> Tri {
        if b { Tri::Yes } else { Tri::No }
    }
}

/// Per-thread scratch space for building paths on demand.
#[derive(Default)]
pub(crate) struct Scratch {
    chain: Vec<u32>,
    path: String,
    lower: String,
    /// `path` and `lower` hold the current subject's path.
    built: bool,
}

/// What a query is tested against: an index entry, or an item found on
/// disk by a live search.
pub(crate) trait Subject {
    fn name(&self) -> &str;
    fn lower_name(&self) -> &str;
    /// Appends the full path to `out` (`chain` is reusable scratch).
    fn write_path(&self, chain: &mut Vec<u32>, out: &mut String);
    /// A real folder (not a package or symlink).
    fn is_dir(&self) -> bool;
    fn size(&self) -> u64;
    /// Modification time in Unix seconds.
    fn mtime(&self) -> i64;
    /// A source entry's attribute; local files have none, so `key:value` never matches them.
    fn attribute(&self, _key: &str) -> Option<&str> {
        None
    }
}

/// An entry of the index.
struct Indexed<'a> {
    index: &'a Index,
    id: u32,
}

impl Subject for Indexed<'_> {
    fn name(&self) -> &str {
        self.index.name(self.id)
    }
    fn lower_name(&self) -> &str {
        self.index.lower_name(self.id)
    }
    fn write_path(&self, chain: &mut Vec<u32>, out: &mut String) {
        self.index.write_path(self.id, chain, out);
    }
    fn is_dir(&self) -> bool {
        self.index.is_dir(self.id)
    }
    fn size(&self) -> u64 {
        self.index.size(self.id)
    }
    fn mtime(&self) -> i64 {
        self.index.mtime(self.id)
    }
}

pub(crate) struct Ctx<'a, S: Subject> {
    pub(crate) subject: &'a S,
    /// File contents for pass 2; `None` in pass 1.
    pub(crate) content: Option<&'a [u8]>,
}

impl<S: Subject> Ctx<'_, S> {
    /// Tests the query against the subject (resets `scratch` for it).
    pub(crate) fn test(&self, node: &Node, scratch: &mut Scratch) -> Tri {
        scratch.built = false;
        self.eval(node, scratch)
    }

    fn eval(&self, node: &Node, scratch: &mut Scratch) -> Tri {
        match node {
            Node::All => Tri::Yes,
            Node::Not(inner) => self.eval(inner, scratch).not(),
            Node::And(parts) => {
                let mut acc = Tri::Yes;
                for p in parts {
                    match self.eval(p, scratch) {
                        Tri::No => return Tri::No,
                        Tri::Maybe => acc = Tri::Maybe,
                        Tri::Yes => {}
                    }
                }
                acc
            }
            Node::Or(parts) => {
                let mut acc = Tri::No;
                for p in parts {
                    match self.eval(p, scratch) {
                        Tri::Yes => return Tri::Yes,
                        Tri::Maybe => acc = Tri::Maybe,
                        Tri::No => {}
                    }
                }
                acc
            }
            Node::Leaf(pred) => self.pred(pred, scratch),
        }
    }

    fn pred(&self, pred: &Pred, scratch: &mut Scratch) -> Tri {
        let subject = self.subject;
        match pred {
            Pred::Name(text) => text.matches(subject.name(), subject.lower_name()).into(),
            Pred::Path(text) => {
                if !scratch.built {
                    scratch.path.clear();
                    subject.write_path(&mut scratch.chain, &mut scratch.path);
                    scratch.lower.clear();
                    scratch.lower.push_str(&scratch.path.to_lowercase());
                    scratch.built = true;
                }
                text.matches(&scratch.path, &scratch.lower).into()
            }
            Pred::Ext(exts) => {
                let name = subject.lower_name();
                (!subject.is_dir()
                    && name.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && exts.iter().any(|e| e == ext)))
                .into()
            }
            Pred::IsDir(dir) => (subject.is_dir() == *dir).into(),
            Pred::Size(range) => (!subject.is_dir() && range.contains(subject.size() as i64)).into(),
            Pred::Modified(range) => range.contains(subject.mtime()).into(),
            Pred::Hidden => Tri::Yes,
            Pred::Attr { key, value } => subject.attribute(key).is_some_and(|v| v.to_lowercase() == *value).into(),
            Pred::Content(re) => {
                if subject.is_dir() {
                    return Tri::No;
                }
                match self.content {
                    Some(bytes) => re.is_match(bytes).into(),
                    None => Tri::Maybe,
                }
            }
        }
    }
}

/// Runs `query`. `scanned` counts files read for `content:`; returns `None`
/// if `cancel` was set.
pub fn search(
    index: &Index,
    query: &Query,
    opts: &SearchOptions,
    scanned: &AtomicUsize,
    cancel: &AtomicBool,
) -> Option<SearchResult> {
    let include_hidden = opts.include_hidden || query.wants_hidden;
    let check = |scratch: &mut Scratch, id: u32| {
        if cancel.load(Ordering::Relaxed) || index.is_removed(id) || (!include_hidden && index.in_hidden_tree(id)) {
            return None;
        }
        let ctx = Ctx { subject: &Indexed { index, id }, content: None };
        let verdict = ctx.test(&query.root, scratch);
        // Scope check last: it walks parents, the query is usually cheaper.
        (verdict != Tri::No && opts.under.is_none_or(|dir| id != dir && index.is_under(id, dir))).then_some((id, verdict))
    };
    let mut pass1: Vec<(u32, Tri)> =
        index.scanned_ids().into_par_iter().with_min_len(4096).map_init(Scratch::default, check).flatten().collect();
    // Entries added since the full scan aren't in name order: sort and merge them in.
    let mut added: Vec<(u32, Tri)> = index.added_ids().into_par_iter().map_init(Scratch::default, check).flatten().collect();
    if !added.is_empty() {
        added.sort_by(|a, b| index.cmp_ids(a.0, b.0));
        pass1 = index.merge_sorted(pass1, added, |&(id, _)| id);
    }
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    if !query.has_content {
        return Some(SearchResult { ids: pass1.into_iter().map(|(id, _)| id).collect() });
    }

    // Pass 2: read only the files whose match depends on their contents.
    let ids: Vec<u32> = pass1
        .into_par_iter()
        .map_init(Scratch::default, |scratch, (id, verdict)| {
            if verdict == Tri::Yes {
                return Some(id);
            }
            if cancel.load(Ordering::Relaxed) || index.size(id) > opts.max_content_bytes {
                return None;
            }
            let bytes = read_text_file(&index.path(id), opts.max_content_bytes)?;
            scanned.fetch_add(1, Ordering::Relaxed);
            let ctx = Ctx { subject: &Indexed { index, id }, content: Some(&bytes) };
            (ctx.test(&query.root, scratch) == Tri::Yes).then_some(id)
        })
        .flatten()
        .collect();
    (!cancel.load(Ordering::Relaxed)).then_some(SearchResult { ids })
}

/// The file's bytes, or `None` for unreadable or binary files (NUL in the first 8 KB).
pub(crate) fn read_text_file(path: &std::path::Path, max: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path).ok()?.take(max).read_to_end(&mut bytes).ok()?;
    let head = &bytes[..bytes.len().min(8192)];
    (!head.contains(&0)).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::index::tests::{sample_index, sample_tree};
    use crate::search::parse;

    fn names(index: &Index, q: &str, opts: &SearchOptions) -> Vec<String> {
        let query = parse(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        let result = search(index, &query, opts, &AtomicUsize::new(0), &AtomicBool::new(false)).unwrap();
        result.ids.iter().map(|&id| index.name(id).to_string()).collect()
    }

    #[test]
    fn query_language() {
        let tmp = sample_tree();
        let index = sample_index(tmp.path());
        let o = SearchOptions::default();
        assert_eq!(names(&index, "img", &o), ["IMG_0001.jpg", "img_0002.PNG"], "case-insensitive, sorted");
        assert_eq!(names(&index, "IMG", &o), ["IMG_0001.jpg"], "smart case");
        assert_eq!(names(&index, "*.jpg", &o), ["IMG_0001.jpg"]);
        assert_eq!(names(&index, "ext:png;jpg", &o), ["IMG_0001.jpg", "img_0002.PNG"]);
        assert_eq!(names(&index, "size:>1mb", &o), ["IMG_0001.jpg"]);
        assert_eq!(names(&index, "folder:find", &o), ["findest"]);
        assert_eq!(names(&index, "file:find", &o), Vec::<String>::new());
        assert_eq!(names(&index, "findest/src", &o), ["main.rs", "src"], "path terms");
        assert_eq!(names(&index, "regex:^img_\\d+\\.", &o), ["IMG_0001.jpg", "img_0002.PNG"]);
        assert_eq!(names(&index, "ext:jpg | ext:md", &o), ["IMG_0001.jpg", "README.md"]);
        assert_eq!(names(&index, "ext:png;jpg !IMG", &o), ["img_0002.PNG"]);
        assert_eq!(names(&index, "dm:today ext:txt", &o), ["notes.txt"]);
        assert!(names(&index, "settings", &o).is_empty(), "hidden folders are skipped");
        assert_eq!(names(&index, "hidden: settings", &o), ["settings.toml"]);
        assert_eq!(names(&index, "Tool", &o), ["Tool.app"]);
    }

    #[test]
    fn content_search() {
        let tmp = sample_tree();
        let index = sample_index(tmp.path());
        let o = SearchOptions::default();
        assert_eq!(names(&index, "content:todo", &o), ["main.rs", "notes.txt"]);
        assert_eq!(names(&index, "content:TODO ext:txt", &o), ["notes.txt"]);
        assert_eq!(names(&index, "content:todo | readme", &o), ["main.rs", "notes.txt", "README.md"]);
        assert_eq!(names(&index, "!content:todo ext:rs;txt;md", &o), ["README.md"]);
        assert!(names(&index, "content:nothing-like-this", &o).is_empty());
    }

    #[test]
    fn scoped_and_cancelled() {
        let tmp = sample_tree();
        let index = sample_index(tmp.path());
        let photos = index.find(&tmp.path().join("Photos")).unwrap();
        let o = SearchOptions { under: Some(photos), ..Default::default() };
        assert_eq!(names(&index, "", &o), ["2024", "IMG_0001.jpg", "img_0002.PNG", "notes.txt"]);
        let query = parse("img").unwrap();
        assert!(search(&index, &query, &SearchOptions::default(), &AtomicUsize::new(0), &AtomicBool::new(true)).is_none());
    }
}
