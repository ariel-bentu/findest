use std::path::{Path, PathBuf};

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// Expands a leading `~` to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        home_dir()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// Replaces the home directory prefix with `~` for display.
pub fn contract_tilde(path: &Path) -> String {
    let home = home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// The name shown for a path: its last component, or the path itself for `/`.
pub fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Interprets text the user typed or pasted as a path: trims whitespace and
/// matching quotes, accepts `file://` URLs, unescapes shell-style `\ `, and
/// expands `~`. Returns `None` for empty input.
pub fn parse_user_path(text: &str) -> Option<PathBuf> {
    let mut text = text.trim();
    for quote in ['\'', '"'] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            text = text[1..text.len() - 1].trim();
        }
    }
    if text.is_empty() {
        return None;
    }
    if text.starts_with("file://") {
        return path_from_file_url(text);
    }
    // Terminal-style escapes, e.g. "My\ Folder" or "a\(1\)".
    let mut unescaped = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => unescaped.extend(chars.next()),
            c => unescaped.push(c),
        }
    }
    Some(expand_tilde(&unescaped))
}

/// Converts a `file://` URL (as macOS sends when opening documents) to a path.
pub fn path_from_file_url(url: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let rest = url.strip_prefix("file://")?;
    // Skip an optional host ("file://localhost/...").
    let path = &rest[rest.find('/')?..];
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = path.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    let mut path = PathBuf::from(std::ffi::OsString::from_vec(out));
    if path.as_os_str().len() > 1 && path.to_string_lossy().ends_with('/') {
        path = path.components().collect();
    }
    Some(path)
}

/// Terminal-style Tab completion of a typed path: the new text, and the
/// matching names when there's nothing more to complete (`Some(empty)`: no match).
#[derive(Debug, PartialEq)]
pub struct Completion {
    pub text: String,
    pub candidates: Option<Vec<String>>,
}

/// Completes the last component of `text` (case-insensitively, as macOS
/// file names are): one match is completed fully, with "/" for folders;
/// several are completed as far as they agree, then listed. Hidden items
/// match only once a "." is typed.
pub fn complete_path(text: &str) -> Completion {
    let unchanged = |candidates| Completion { text: text.to_string(), candidates };
    if text == "~" {
        return Completion { text: "~/".into(), candidates: None };
    }
    let Some(slash) = text.rfind('/') else { return unchanged(Some(Vec::new())) };
    let (head, prefix) = text.split_at(slash + 1);
    let Ok(read) = std::fs::read_dir(expand_tilde(head)) else { return unchanged(Some(Vec::new())) };
    let lower = prefix.to_lowercase();
    let mut matches: Vec<(String, bool)> = read
        .filter_map(|item| item.ok())
        .filter_map(|item| {
            let name = item.file_name().to_str()?.to_string();
            let visible = !name.starts_with('.') || prefix.starts_with('.');
            // Follows symlinks, so a link to a folder completes like a folder.
            (visible && name.to_lowercase().starts_with(&lower)).then(|| (name, item.path().is_dir()))
        })
        .collect();
    matches.sort_by_key(|(name, _)| name.to_lowercase());
    match matches.as_slice() {
        [] => unchanged(Some(Vec::new())),
        [(name, is_dir)] => Completion { text: format!("{head}{name}{}", if *is_dir { "/" } else { "" }), candidates: None },
        _ => {
            // At least as long as what was typed (every name starts with it),
            // and spelled as on disk.
            let common = common_prefix(matches.iter().map(|(name, _)| name.as_str()));
            let grew = common.chars().count() > prefix.chars().count();
            let names = matches.into_iter().map(|(name, dir)| if dir { format!("{name}/") } else { name }).collect();
            Completion { text: format!("{head}{common}"), candidates: (!grew).then_some(names) }
        }
    }
}

/// The longest start all `names` share, ignoring case (spelled as in the first).
fn common_prefix<'a>(mut names: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = names.next() else { return String::new() };
    let mut len = first.chars().count();
    for name in names {
        len = first
            .chars()
            .zip(name.chars())
            .take(len)
            .take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
            .count();
    }
    first.chars().take(len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_paths_like_a_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_str().unwrap().to_string();
        for dir in ["Documents", "Downloads", "dev", ".hidden"] {
            std::fs::create_dir(tmp.path().join(dir)).unwrap();
        }
        std::fs::write(tmp.path().join("notes.txt"), "").unwrap();
        let complete = |typed: &str| complete_path(&format!("{root}/{typed}"));
        let text = |rest: &str| format!("{root}/{rest}");

        assert_eq!(complete("dev"), Completion { text: text("dev/"), candidates: None }, "a folder gets its /");
        assert_eq!(complete("no"), Completion { text: text("notes.txt"), candidates: None }, "a file doesn't");
        let documents_downloads = Some(vec!["Documents/".to_string(), "Downloads/".to_string()]);
        assert_eq!(complete("do"), Completion { text: text("Do"), candidates: documents_downloads.clone() }, "spelled as on disk, then listed");
        assert_eq!(complete("Doc"), Completion { text: text("Documents/"), candidates: None });
        assert_eq!(complete("Do").candidates, documents_downloads);
        assert_eq!(complete("d").candidates.unwrap().len(), 3);
        assert_eq!(complete("zzz"), Completion { text: text("zzz"), candidates: Some(Vec::new()) });
        assert_eq!(complete("").candidates.unwrap().len(), 4, "hidden items are left out");
        assert_eq!(complete(".h").text, text(".hidden/"));
        assert_eq!(complete_path("~").text, "~/");
    }

    #[test]
    fn tilde_round_trip() {
        let p = expand_tilde("~/Documents");
        assert_eq!(p, home_dir().join("Documents"));
        assert_eq!(contract_tilde(&p), "~/Documents");
        assert_eq!(contract_tilde(&home_dir()), "~");
        assert_eq!(expand_tilde("/tmp"), PathBuf::from("/tmp"));
    }

    #[test]
    fn user_paths() {
        assert_eq!(parse_user_path("  /tmp/a \n"), Some(PathBuf::from("/tmp/a")));
        assert_eq!(parse_user_path("'/tmp/my dir'"), Some(PathBuf::from("/tmp/my dir")));
        assert_eq!(parse_user_path("\"~/x\""), Some(home_dir().join("x")));
        assert_eq!(parse_user_path("/tmp/My\\ Folder\\(1\\)"), Some(PathBuf::from("/tmp/My Folder(1)")));
        assert_eq!(parse_user_path("file:///tmp/a%20b"), Some(PathBuf::from("/tmp/a b")));
        assert_eq!(parse_user_path("   "), None);
        assert_eq!(parse_user_path("''"), None);
    }

    #[test]
    fn file_urls() {
        assert_eq!(path_from_file_url("file:///Users/me/My%20Stuff/").unwrap(), PathBuf::from("/Users/me/My Stuff"));
        assert_eq!(path_from_file_url("file://localhost/tmp/a%C3%A9").unwrap(), PathBuf::from("/tmp/aé"));
        assert_eq!(path_from_file_url("file:///").unwrap(), PathBuf::from("/"));
        assert!(path_from_file_url("https://example.com").is_none());
    }

    #[test]
    fn display_name_of_root() {
        assert_eq!(display_name(Path::new("/")), "/");
        assert_eq!(display_name(Path::new("/a/b.txt")), "b.txt");
    }
}
