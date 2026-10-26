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

/// Extensions previewed as text without sniffing their contents.
pub const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "rst", "log", "csv", "tsv", "json", "jsonc", "toml", "yaml", "yml", "xml", "ini",
    "cfg", "conf", "env", "properties", "rs", "py", "js", "mjs", "cjs", "ts", "tsx", "jsx", "go", "java", "kt",
    "swift", "c", "h", "cc", "cpp", "hpp", "m", "mm", "rb", "php", "sh", "bash", "zsh", "fish", "ps1", "sql",
    "html", "htm", "css", "scss", "less", "vue", "svelte", "lua", "pl", "r", "scala", "dart", "gradle", "tf",
    "proto", "graphql", "lock", "gitignore", "dockerignore", "editorconfig", "plist", "srt", "tex", "svg",
];

/// Whether `path` is a readable text file (by extension or by sniffing the first 8 KB).
pub fn is_text_file(path: &Path) -> bool {
    use std::io::Read;
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    if ext.as_deref() == Some("pdf") {
        return false;
    }
    let known = ext.as_deref().is_some_and(|e| TEXT_EXTENSIONS.contains(&e))
        || path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
            (n.starts_with('.') && !n[1..].contains('.'))
                || matches!(
                    n.to_ascii_lowercase().as_str(),
                    "makefile"
                        | "dockerfile"
                        | "containerfile"
                        | "justfile"
                        | "vagrantfile"
                        | "gemfile"
                        | "rakefile"
                        | "procfile"
                        | "license"
                        | "copying"
                        | "readme"
                        | "changelog"
                        | "authors"
                        | "codeowners"
                )
        });
    let Ok(meta) = fs::metadata(path) else {
        return known;
    };
    if !meta.is_file() {
        return false;
    }
    let mut buf = Vec::with_capacity(8192);
    let Ok(mut file) = fs::File::open(path) else { return false };
    if (&mut file).take(8192).read_to_end(&mut buf).is_err() {
        return false;
    }
    if buf.contains(&0) {
        return false;
    }
    if known {
        return true;
    }
    let text = String::from_utf8_lossy(&buf);
    let replaced = text.chars().filter(|&c| c == char::REPLACEMENT_CHARACTER).count();
    replaced * 100 <= text.chars().count().max(1)
}

/// Up to `max_lines` lines of a text file, or `None` for binary files.
/// Reads at most 64 KB. Unknown extensions are sniffed: no NUL bytes and
/// mostly valid UTF-8 counts as text.
pub fn text_preview(path: &Path, max_lines: usize) -> Option<String> {
    use std::io::Read;
    const MAX_BYTES: usize = 64 * 1024;
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let known = ext.as_deref().is_some_and(|e| TEXT_EXTENSIONS.contains(&e))
        || path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.') && !n[1..].contains('.'));
    let mut buf = Vec::with_capacity(8192);
    fs::File::open(path).ok()?.take(MAX_BYTES as u64).read_to_end(&mut buf).ok()?;
    if buf.contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(&buf);
    if !known {
        let replaced = text.chars().filter(|&c| c == char::REPLACEMENT_CHARACTER).count();
        if replaced * 100 > text.chars().count().max(1) {
            return None;
        }
    }
    let mut out = String::new();
    for line in text.lines().take(max_lines) {
        out.push_str(&line.replace('\t', "    "));
        out.push('\n');
    }
    Some(out.trim_end_matches('\n').to_string())
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
    fn text_previews() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("main.rs");
        fs::write(&code, "fn main() {\n\tprintln!();\n}\nextra\n").unwrap();
        assert_eq!(text_preview(&code, 3).unwrap(), "fn main() {\n    println!();\n}");

        let makefile = tmp.path().join("Makefile");
        fs::write(&makefile, "all:\n\tcargo build\n").unwrap();
        assert!(text_preview(&makefile, 10).is_some(), "sniffed as text");

        let binary = tmp.path().join("blob.bin");
        fs::write(&binary, [0x89u8, b'P', b'N', b'G', 0, 1, 2]).unwrap();
        assert!(text_preview(&binary, 10).is_none());

        let latin1 = tmp.path().join("data");
        fs::write(&latin1, [0xff_u8, 0xfe, 0xfd, 0xfc, 0xfb].repeat(20)).unwrap();
        assert!(text_preview(&latin1, 10).is_none(), "mostly invalid UTF-8");
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
