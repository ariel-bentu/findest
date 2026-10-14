use std::cmp::Ordering;
use std::fs;
use std::io;
use std::os::macos::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// BSD `UF_HIDDEN` file flag (hidden from GUI listings, e.g. `~/Library`).
const UF_HIDDEN: u32 = 0x8000;

/// Directory extensions that macOS presents as single opaque files.
const PACKAGE_EXTENSIONS: &[&str] = &["app", "bundle", "framework", "plugin", "kext", "xcarchive"];

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    /// True for real directories that should be navigated into (not packages).
    pub is_dir: bool,
    /// True for directory bundles like `Foo.app` that open as a unit.
    pub is_package: bool,
    pub is_symlink: bool,
    pub is_hidden: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

impl Entry {
    pub fn from_path(path: &Path) -> io::Result<Entry> {
        let name = crate::paths::display_name(path);
        let link_meta = fs::symlink_metadata(path)?;
        let is_symlink = link_meta.file_type().is_symlink();
        // Follow symlinks for type and size; fall back to the link itself if dangling.
        let meta = if is_symlink { fs::metadata(path).unwrap_or(link_meta.clone()) } else { link_meta.clone() };
        let raw_dir = meta.is_dir();
        let is_package = raw_dir && is_package_name(&name);
        Ok(Entry {
            is_hidden: name.starts_with('.') || link_meta.st_flags() & UF_HIDDEN != 0,
            is_dir: raw_dir && !is_package,
            is_package,
            is_symlink,
            size: if raw_dir { 0 } else { meta.len() },
            modified: meta.modified().ok(),
            path: path.to_path_buf(),
            name,
        })
    }

    /// The name shown in the UI. Like Finder, applications hide their ".app".
    pub fn display_name(&self) -> &str {
        if self.is_package
            && let Some(stem) = self.name.strip_suffix(".app")
            && !stem.is_empty()
        {
            return stem;
        }
        &self.name
    }

    pub fn extension(&self) -> Option<&str> {
        if self.is_dir {
            return None;
        }
        Path::new(&self.name).extension().and_then(|e| e.to_str())
    }

    /// Human readable kind, e.g. "Folder", "PNG file".
    pub fn kind(&self) -> String {
        if self.is_dir {
            return "Folder".into();
        }
        match self.extension() {
            Some("app") => "Application".into(),
            Some(ext) => format!("{} file", ext.to_uppercase()),
            None => "File".into(),
        }
    }
}

fn is_package_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| PACKAGE_EXTENSIONS.iter().any(|p| p.eq_ignore_ascii_case(e)))
}

/// Lists a directory. Entries that vanish or can't be stat'ed mid-listing are skipped.
pub fn read_dir(dir: &Path, show_hidden: bool) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let Ok(entry) = Entry::from_path(&item.path()) else { continue };
        if show_hidden || !entry.is_hidden {
            entries.push(entry);
        }
    }
    Ok(entries)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
    Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortOrder {
    pub key: SortKey,
    pub ascending: bool,
    pub folders_first: bool,
}

pub fn sort_entries(entries: &mut [Entry], order: SortOrder) {
    entries.sort_by(|a, b| {
        if order.folders_first && a.is_dir != b.is_dir {
            return b.is_dir.cmp(&a.is_dir);
        }
        let primary = match order.key {
            SortKey::Name => Ordering::Equal,
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Modified => a.modified.cmp(&b.modified),
            SortKey::Kind => a.kind().cmp(&b.kind()),
        };
        let ord = primary.then_with(|| natural_cmp(&a.name, &b.name));
        if order.ascending { ord } else { ord.reverse() }
    });
}

/// Case-insensitive comparison that orders embedded numbers numerically,
/// so "file2" sorts before "file10" (like Finder).
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut a = a.chars().peekable();
    let mut b = b.chars().peekable();
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = take_number(&mut a);
                let nb = take_number(&mut b);
                // Compare by magnitude (length after stripping zeros), then lexically.
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb)).then_with(|| na.len().cmp(&nb.len()));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

fn take_number(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut s = String::new();
    while let Some(c) = chars.peek().copied().filter(char::is_ascii_digit) {
        s.push(c);
        chars.next();
    }
    s
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64;
    let mut unit = "";
    for u in UNITS {
        value /= 1000.0;
        unit = u;
        if value < 1000.0 {
            break;
        }
    }
    if value < 10.0 { format!("{value:.1} {unit}") } else { format!("{value:.0} {unit}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_ordering() {
        let mut names = vec!["file10", "File2", "file1", "a", "file02"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, vec!["a", "file1", "File2", "file02", "file10"]);
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(12), "12 bytes");
        assert_eq!(format_size(1_500), "1.5 KB");
        assert_eq!(format_size(250_000_000), "250 MB");
    }

    #[test]
    fn lists_and_sorts_with_folders_first() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("b.txt"), "hello").unwrap();
        fs::write(tmp.path().join(".hidden"), "").unwrap();
        fs::create_dir(tmp.path().join("z-dir")).unwrap();
        fs::create_dir(tmp.path().join("Tool.app")).unwrap();

        let mut entries = read_dir(tmp.path(), false).unwrap();
        sort_entries(&mut entries, SortOrder { key: SortKey::Name, ascending: true, folders_first: true });
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["z-dir", "b.txt", "Tool.app"]);
        assert!(entries[2].is_package && !entries[2].is_dir);
        assert_eq!(entries[2].display_name(), "Tool");
        assert_eq!(entries[1].display_name(), "b.txt");
        assert_eq!(entries[1].size, 5);

        assert_eq!(read_dir(tmp.path(), true).unwrap().len(), 4);
    }
}
