//! Details for the info panel: timestamps, permissions, folder sizes.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq)]
pub struct FileInfo {
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    /// Unix permission bits (e.g. 0o644).
    pub mode: u32,
    /// For symlinks: where the link points.
    pub link_target: Option<PathBuf>,
    pub inode: u64,
}

pub fn file_info(path: &Path) -> io::Result<FileInfo> {
    let link = fs::symlink_metadata(path)?;
    let link_target = link.file_type().is_symlink().then(|| fs::read_link(path).ok()).flatten();
    let meta = if link_target.is_some() { fs::metadata(path).unwrap_or(link) } else { link };
    Ok(FileInfo {
        created: meta.created().ok(),
        modified: meta.modified().ok(),
        accessed: meta.accessed().ok(),
        mode: meta.permissions().mode(),
        link_target,
        inode: meta.ino(),
    })
}

/// `ls`-style permissions, e.g. "rw-r--r--".
pub fn permissions_string(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// Total bytes and number of items inside `dir`, recursively, without
/// following symlinks. Returns `None` if cancelled. Unreadable subfolders are
/// skipped. Hard-linked files are counted once.
pub fn dir_size(dir: &Path, cancel: &AtomicBool) -> Option<(u64, u64)> {
    let mut bytes = 0u64;
    let mut items = 0u64;
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else { continue };
        for item in read.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let Ok(meta) = item.metadata() else { continue };
            items += 1;
            if meta.is_dir() {
                stack.push(item.path());
            } else if meta.nlink() <= 1 || seen.insert((meta.dev(), meta.ino())) {
                bytes += meta.len();
            }
        }
    }
    Some((bytes, items))
}

/// Formats a byte count with thousands separators: 1234567 -> "1,234,567".
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions() {
        assert_eq!(permissions_string(0o644), "rw-r--r--");
        assert_eq!(permissions_string(0o755), "rwxr-xr-x");
        assert_eq!(permissions_string(0o100600), "rw-------");
    }

    #[test]
    fn digits() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(1234567), "1,234,567");
    }

    #[test]
    fn sizes_and_links() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("a"), "12345").unwrap();
        fs::write(tmp.path().join("sub/b"), "123").unwrap();
        fs::hard_link(tmp.path().join("a"), tmp.path().join("a-link")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("a"), tmp.path().join("s")).unwrap();

        let cancel = AtomicBool::new(false);
        let (bytes, items) = dir_size(tmp.path(), &cancel).unwrap();
        // a (5, hard link counted once) + sub/b (3) + symlink's own size.
        let link_len = fs::symlink_metadata(tmp.path().join("s")).unwrap().len();
        assert_eq!(bytes, 8 + link_len);
        assert_eq!(items, 5);

        cancel.store(true, Ordering::Relaxed);
        assert!(dir_size(tmp.path(), &cancel).is_none());

        let info = file_info(&tmp.path().join("s")).unwrap();
        assert_eq!(info.link_target.unwrap(), tmp.path().join("a"));
        assert!(info.created.is_some());
    }
}
