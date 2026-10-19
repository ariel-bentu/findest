//! Well-known places for the sidebar: cloud drives and mounted volumes.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub path: PathBuf,
    pub label: String,
}

pub fn icloud_drive(home: &Path) -> PathBuf {
    home.join("Library/Mobile Documents/com~apple~CloudDocs")
}

/// The user's Trash.
pub fn trash(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join(".Trash")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = home;
        dirs::data_dir().unwrap_or_else(|| home.join(".local/share")).join("Trash/files")
    }
}

fn cloud_storage(home: &Path) -> PathBuf {
    home.join("Library/CloudStorage")
}

/// iCloud Drive plus File Provider drives (OneDrive, Google Drive, Dropbox, ...)
/// on macOS, and common cloud-sync folders on Linux.
pub fn cloud_places(home: &Path) -> Vec<Place> {
    let mut places = Vec::new();
    let icloud = icloud_drive(home);
    if icloud.is_dir() {
        places.push(Place { path: icloud, label: "iCloud Drive".into() });
    }
    if let Ok(read) = fs::read_dir(cloud_storage(home)) {
        let mut providers: Vec<Place> = read
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?.to_string();
                (!name.starts_with('.')).then(|| Place { label: cloud_label(&name), path })
            })
            .collect();
        providers.sort_by(|a, b| a.label.cmp(&b.label));
        places.extend(providers);
    }
    #[cfg(not(target_os = "macos"))]
    for (dir, label) in [
        ("OneDrive", "OneDrive"),
        ("Dropbox", "Dropbox"),
        ("Google Drive", "Google Drive"),
        ("GoogleDrive", "Google Drive"),
    ] {
        let path = home.join(dir);
        if path.is_dir() && !places.iter().any(|p| p.path == path) {
            places.push(Place { path, label: label.into() });
        }
    }
    places
}

/// "OneDrive-Contoso" -> "OneDrive – Contoso", "GoogleDrive-me@x.com" -> "Google Drive – me@x.com".
fn cloud_label(folder: &str) -> String {
    let (provider, account) = folder.split_once('-').unwrap_or((folder, ""));
    let provider = match provider {
        "GoogleDrive" => "Google Drive",
        "OneDrive" => "OneDrive",
        "Dropbox" => "Dropbox",
        "Box" => "Box",
        other => other,
    };
    if account.is_empty() { provider.to_string() } else { format!("{provider} – {account}") }
}

/// Mounted volumes other than the boot disk.
pub fn volumes() -> Vec<Place> {
    #[cfg(target_os = "macos")]
    {
        let Ok(read) = fs::read_dir("/Volumes") else { return Vec::new() };
        let mut volumes: Vec<Place> = read
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            // The boot volume appears as a symlink to "/".
            .filter(|p| fs::read_link(p).is_err() && p.is_dir())
            .map(|path| Place { label: crate::paths::display_name(&path), path })
            .collect();
        volumes.sort_by(|a, b| a.label.cmp(&b.label));
        volumes
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut roots = vec![PathBuf::from("/media"), PathBuf::from("/mnt")];
        // systemd mounts removable media here.
        if let Some(user) = dirs::home_dir().and_then(|h| h.file_name().map(|n| n.to_owned())) {
            roots.push(PathBuf::from("/run/media").join(user));
        }
        let mut volumes = Vec::new();
        for root in roots {
            let Ok(read) = fs::read_dir(root) else { continue };
            for path in read.filter_map(|e| e.ok()).map(|e| e.path()) {
                if path.is_dir()
                    && !path.is_symlink()
                    && !path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'))
                {
                    volumes.push(Place { label: crate::paths::display_name(&path), path });
                }
            }
        }
        volumes.sort_by(|a, b| a.label.cmp(&b.label));
        volumes.dedup_by(|a, b| a.path == b.path);
        volumes
    }
}

/// The root of the cloud drive containing `path`, with its label.
pub fn cloud_root(home: &Path, path: &Path) -> Option<Place> {
    let icloud = icloud_drive(home);
    if path.starts_with(&icloud) {
        return Some(Place { path: icloud, label: "iCloud Drive".into() });
    }
    let rest = path.strip_prefix(cloud_storage(home)).ok()?;
    let first = rest.components().next()?.as_os_str().to_str()?;
    Some(Place { path: cloud_storage(home).join(first), label: cloud_label(first) })
}

/// A named place that `path` lives in (Trash or a cloud drive): breadcrumbs
/// start there instead of walking through hidden folders.
fn special_root(home: &Path, path: &Path) -> Option<Place> {
    if let Some((mount, label)) = crate::sources::global().mount_named(path) {
        return Some(Place { path: mount, label });
    }
    let trash = trash(home);
    if path.starts_with(&trash) {
        return Some(Place { path: trash, label: "Trash".into() });
    }
    cloud_root(home, path)
}

/// Name to show for a folder: Trash and cloud roots get their friendly name.
pub fn friendly_name(home: &Path, path: &Path) -> String {
    match special_root(home, path) {
        Some(root) if root.path == path => root.label,
        _ if path == Path::new("/") => root_label().into(),
        _ => crate::paths::display_name(path),
    }
}

fn root_label() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Macintosh HD"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "Filesystem"
    }
}

/// Breadcrumb trail for `path`, root first. Inside a cloud drive the trail
/// starts at the drive instead of walking through `~/Library/...`.
pub fn breadcrumbs(home: &Path, path: &Path) -> Vec<Place> {
    let stop = special_root(home, path).map(|r| r.path);
    let mut crumbs = Vec::new();
    for ancestor in path.ancestors() {
        crumbs.push(Place { path: ancestor.to_path_buf(), label: friendly_name(home, ancestor) });
        if stop.as_deref() == Some(ancestor) {
            break;
        }
    }
    crumbs.reverse();
    crumbs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_cloud_places() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        fs::create_dir_all(icloud_drive(home)).unwrap();
        fs::create_dir_all(home.join("Library/CloudStorage/OneDrive-Contoso")).unwrap();
        fs::create_dir_all(home.join("Library/CloudStorage/GoogleDrive-me@example.com")).unwrap();
        fs::write(home.join("Library/CloudStorage/.DS_Store"), "").unwrap();

        let labels: Vec<String> = cloud_places(home).into_iter().map(|p| p.label).collect();
        assert_eq!(labels, vec!["iCloud Drive", "Google Drive – me@example.com", "OneDrive – Contoso"]);
    }

    #[test]
    fn breadcrumbs_start_at_cloud_root() {
        let home = Path::new("/Users/me");
        let docs = icloud_drive(home).join("Docs/Taxes");
        let labels: Vec<String> = breadcrumbs(home, &docs).into_iter().map(|p| p.label).collect();
        assert_eq!(labels, vec!["iCloud Drive", "Docs", "Taxes"]);

        let od = home.join("Library/CloudStorage/OneDrive-Contoso/Shared");
        let labels: Vec<String> = breadcrumbs(home, &od).into_iter().map(|p| p.label).collect();
        assert_eq!(labels, vec!["OneDrive – Contoso", "Shared"]);

        let trashed: Vec<String> = breadcrumbs(home, &trash(home).join("old")).into_iter().map(|p| p.label).collect();
        assert_eq!(trashed, vec!["Trash", "old"]);

        let plain: Vec<String> = breadcrumbs(home, Path::new("/Users/me/code")).into_iter().map(|p| p.label).collect();
        assert_eq!(plain, vec![root_label(), "Users", "me", "code"]);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn finds_linux_cloud_places() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        fs::create_dir_all(home.join("Dropbox")).unwrap();
        fs::create_dir_all(home.join("OneDrive")).unwrap();
        fs::create_dir_all(home.join("Google Drive")).unwrap();

        let labels: Vec<String> = cloud_places(home).into_iter().map(|p| p.label).collect();
        assert_eq!(labels, vec!["OneDrive", "Dropbox", "Google Drive"]);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_trash() {
        let home = tempfile::tempdir().unwrap();
        let t = trash(home.path());
        assert!(t.ends_with("Trash/files"));
    }
}
