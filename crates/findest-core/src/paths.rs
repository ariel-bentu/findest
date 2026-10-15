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

#[cfg(test)]
mod tests {
    use super::*;

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
