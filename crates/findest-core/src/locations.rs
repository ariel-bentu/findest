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
    {
        places.extend(linux_cloud_places(home));
    }
    places
}

#[cfg(not(target_os = "macos"))]
fn linux_cloud_places(home: &Path) -> Vec<Place> {
    let mut places = Vec::new();
    let mut push_unique = |path: PathBuf, label: String| {
        if path.is_dir() && !places.iter().any(|p: &Place| p.path == path) {
            places.push(Place { path, label });
        }
    };

    for (dir, label) in [
        ("OneDrive", "OneDrive"),
        ("Dropbox", "Dropbox"),
        ("Google Drive", "Google Drive"),
        ("GoogleDrive", "Google Drive"),
        ("Nextcloud", "Nextcloud"),
        ("ownCloud", "ownCloud"),
        ("MEGA", "MEGA"),
        ("pCloudDrive", "pCloud"),
        ("Box", "Box"),
        ("Sync", "Sync"),
        ("Insync", "Insync"),
    ] {
        push_unique(home.join(dir), label.into());
    }

    // Multi-account / organization folders in $HOME (e.g. "OneDrive - Contoso", "Dropbox (Business)").
    if let Ok(read) = fs::read_dir(home) {
        let mut dynamic: Vec<Place> = read
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?;
                let label = linux_home_cloud_label(name)?;
                Some(Place { path, label })
            })
            .collect();
        dynamic.sort_by(|a, b| a.label.cmp(&b.label));
        for p in dynamic {
            push_unique(p.path, p.label);
        }
    }

    // Dropbox custom folder locations from ~/.dropbox/info.json.
    if let Ok(info) = fs::read_to_string(home.join(".dropbox/info.json"))
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&info)
    {
        for (key, suffix) in [("personal", ""), ("business", " – Business")] {
            if let Some(p) = json.get(key).and_then(|v| v.get("path")).and_then(|v| v.as_str()) {
                push_unique(PathBuf::from(p), format!("Dropbox{suffix}"));
            }
        }
    }

    // GNOME Online Accounts / GVFS mounts under ~/.gvfs or $XDG_RUNTIME_DIR/gvfs.
    for gvfs_dir in gvfs_roots(home) {
        let Ok(read) = fs::read_dir(gvfs_dir) else { continue };
        let mut gvfs_places: Vec<Place> = read
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?;
                let label = gvfs_label(name)?;
                Some(Place { path, label })
            })
            .collect();
        gvfs_places.sort_by(|a, b| a.label.cmp(&b.label));
        for p in gvfs_places {
            push_unique(p.path, p.label);
        }
    }

    places
}

#[cfg(not(target_os = "macos"))]
fn linux_home_cloud_label(name: &str) -> Option<String> {
    if name.starts_with('.') {
        return None;
    }
    for (prefix, provider) in [
        ("OneDrive - ", "OneDrive"),
        ("OneDrive-", "OneDrive"),
        ("Google Drive - ", "Google Drive"),
        ("GoogleDrive-", "Google Drive"),
        ("Nextcloud - ", "Nextcloud"),
        ("Nextcloud-", "Nextcloud"),
        ("ownCloud - ", "ownCloud"),
        ("ownCloud-", "ownCloud"),
    ] {
        if let Some(account) = name.strip_prefix(prefix) {
            let account = account.trim();
            if !account.is_empty() {
                return Some(format!("{provider} – {account}"));
            }
        }
    }
    if let Some(rest) = name.strip_prefix("Dropbox (").and_then(|s| s.strip_suffix(')')) {
        let account = rest.trim();
        if !account.is_empty() {
            return Some(format!("Dropbox – {account}"));
        }
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn gvfs_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots = vec![home.join(".gvfs")];
    if dirs::home_dir().as_deref() == Some(home) {
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            roots.push(PathBuf::from(runtime).join("gvfs"));
        }
        let uid = unsafe { libc::getuid() };
        let run_gvfs = PathBuf::from(format!("/run/user/{uid}/gvfs"));
        if !roots.contains(&run_gvfs) {
            roots.push(run_gvfs);
        }
    }
    roots
}

/// Formats a GVFS mount directory name such as
/// `google-drive:host=gmail.com,user=alice` or `smb-share:server=nas,share=docs`.
#[cfg(not(target_os = "macos"))]
pub fn gvfs_label(name: &str) -> Option<String> {
    let (scheme, rest) = name.split_once(':')?;
    let mut host = None;
    let mut user = None;
    let mut share = None;
    let mut prefix = None;
    for part in rest.split(',') {
        if let Some((k, v)) = part.split_once('=') {
            match k {
                "host" | "server" => host = Some(v),
                "user" => user = Some(v),
                "share" => share = Some(v),
                "prefix" => prefix = Some(v),
                _ => {}
            }
        }
    }
    match scheme {
        "google-drive" => {
            let account = match (user, host) {
                (Some(u), Some(h)) if !u.contains('@') => format!("{u}@{h}"),
                (Some(u), _) => u.to_string(),
                (None, Some(h)) => h.to_string(),
                (None, None) => String::new(),
            };
            Some(if account.is_empty() { "Google Drive".into() } else { format!("Google Drive – {account}") })
        }
        "onedrive" => {
            let account = user.or(host).unwrap_or("");
            Some(if account.is_empty() { "OneDrive".into() } else { format!("OneDrive – {account}") })
        }
        "dav" | "davs" | "dav+sd" | "davs+sd" => {
            let is_nextcloud = prefix.is_some_and(|p| p.contains("remote.php"));
            let provider = if is_nextcloud { "Nextcloud" } else { "WebDAV" };
            let detail = match (user, host) {
                (Some(u), Some(h)) => format!("{u}@{h}"),
                (None, Some(h)) => h.to_string(),
                (Some(u), None) => u.to_string(),
                (None, None) => rest.to_string(),
            };
            Some(if detail.is_empty() { provider.into() } else { format!("{provider} – {detail}") })
        }
        "smb-share" => {
            let detail = match (share, host) {
                (Some(s), Some(h)) => format!("{s} ({h})"),
                (Some(s), None) => s.to_string(),
                (None, Some(h)) => h.to_string(),
                (None, None) => rest.to_string(),
            };
            Some(if detail.is_empty() { "SMB Share".into() } else { format!("SMB – {detail}") })
        }
        "sftp" | "ftp" | "ftps" => {
            let proto = scheme.to_uppercase();
            let detail = match (user, host) {
                (Some(u), Some(h)) => format!("{u}@{h}"),
                (None, Some(h)) => h.to_string(),
                _ => rest.to_string(),
            };
            Some(if detail.is_empty() { proto } else { format!("{proto} – {detail}") })
        }
        _ => None,
    }
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
    if let Ok(rest) = path.strip_prefix(cloud_storage(home))
        && let Some(first) = rest.components().next().and_then(|c| c.as_os_str().to_str())
    {
        return Some(Place { path: cloud_storage(home).join(first), label: cloud_label(first) });
    }
    #[cfg(not(target_os = "macos"))]
    {
        for gvfs_dir in gvfs_roots(home) {
            if let Ok(rest) = path.strip_prefix(&gvfs_dir)
                && let Some(first) = rest.components().next().and_then(|c| c.as_os_str().to_str())
                && let Some(label) = gvfs_label(first)
            {
                return Some(Place { path: gvfs_dir.join(first), label });
            }
        }
        if let Some(place) = linux_cloud_places(home)
            .into_iter()
            .filter(|p| path.starts_with(&p.path))
            .max_by_key(|p| p.path.as_os_str().len())
        {
            return Some(place);
        }
    }
    None
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
        fs::create_dir_all(home.join("Nextcloud")).unwrap();
        fs::create_dir_all(home.join("OneDrive - Contoso")).unwrap();
        fs::create_dir_all(home.join(".gvfs/google-drive:host=gmail.com,user=alice")).unwrap();

        let labels: Vec<String> = cloud_places(home).into_iter().map(|p| p.label).collect();
        assert_eq!(
            labels,
            vec![
                "OneDrive",
                "Dropbox",
                "Google Drive",
                "Nextcloud",
                "OneDrive – Contoso",
                "Google Drive – alice@gmail.com",
            ]
        );

        let gvfs_doc = home.join(".gvfs/google-drive:host=gmail.com,user=alice/Reports/Q1");
        let crumbs: Vec<String> = breadcrumbs(home, &gvfs_doc).into_iter().map(|p| p.label).collect();
        assert_eq!(crumbs, vec!["Google Drive – alice@gmail.com", "Reports", "Q1"]);

        assert_eq!(gvfs_label("smb-share:server=nas.local,share=media").as_deref(), Some("SMB – media (nas.local)"));
        assert_eq!(
            gvfs_label("davs:host=cloud.example.com,user=bob,prefix=%2Fremote.php%2Fdav").as_deref(),
            Some("Nextcloud – bob@cloud.example.com")
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_trash() {
        let home = tempfile::tempdir().unwrap();
        let t = trash(home.path());
        assert!(t.ends_with("Trash/files"));
    }
}
