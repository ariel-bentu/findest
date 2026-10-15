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
enum Tri {
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
struct Scratch {
    chain: Vec<u32>,
    path: String,
    lower: String,
    built_for: Option<u32>,
}

struct Ctx<'a> {
    index: &'a Index,
    id: u32,
    /// File contents for pass 2; `None` in pass 1.
    content: Option<&'a [u8]>,
}

impl Ctx<'_> {
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
        let (index, id) = (self.index, self.id);
        match pred {
            Pred::Name(text) => text.matches(index.name(id), index.lower_name(id)).into(),
            Pred::Path(text) => {
                if scratch.built_for != Some(id) {
                    scratch.path.clear();
                    index.write_path(id, &mut scratch.chain, &mut scratch.path);
                    scratch.lower.clear();
                    scratch.lower.push_str(&scratch.path.to_lowercase());
                    scratch.built_for = Some(id);
                }
                text.matches(&scratch.path, &scratch.lower).into()
            }
            Pred::Ext(exts) => {
                let name = index.lower_name(id);
                (!index.is_dir(id)
                    && name.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && exts.iter().any(|e| e == ext)))
                .into()
            }
            Pred::IsDir(dir) => (index.is_dir(id) == *dir).into(),
            Pred::Size(range) => (!index.is_dir(id) && range.contains(index.size(id) as i64)).into(),
            Pred::Modified(range) => range.contains(index.mtime(id)).into(),
            Pred::Hidden => Tri::Yes,
            Pred::Content(re) => {
                if index.is_dir(id) {
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
    let pass1: Vec<(u32, Tri)> = (0..index.len() as u32)
        .into_par_iter()
        .with_min_len(4096)
        .map_init(Scratch::default, |scratch, id| {
            if cancel.load(Ordering::Relaxed) || (!include_hidden && index.in_hidden_tree(id)) {
                return None;
            }
            let ctx = Ctx { index, id, content: None };
            let verdict = ctx.eval(&query.root, scratch);
            // Scope check last: it walks parents, the query is usually cheaper.
            (verdict != Tri::No && opts.under.is_none_or(|dir| id != dir && index.is_under(id, dir))).then_some((id, verdict))
        })
        .flatten()
        .collect();
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
            let ctx = Ctx { index, id, content: Some(&bytes) };
            (ctx.eval(&query.root, scratch) == Tri::Yes).then_some(id)
        })
        .flatten()
        .collect();
    (!cancel.load(Ordering::Relaxed)).then_some(SearchResult { ids })
}

/// The file's bytes, or `None` for unreadable or binary files (NUL in the first 8 KB).
fn read_text_file(path: &std::path::Path, max: u64) -> Option<Vec<u8>> {
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
