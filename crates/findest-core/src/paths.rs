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
    fn display_name_of_root() {
        assert_eq!(display_name(Path::new("/")), "/");
        assert_eq!(display_name(Path::new("/a/b.txt")), "b.txt");
    }
}
