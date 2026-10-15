//! File and folder operations. All functions are synchronous; callers run
//! them off the UI thread.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Returns `dir/stem ext`, or `dir/stem N ext` with the smallest N >= 2 that
/// doesn't exist yet. `ext` includes the leading dot, or is empty.
pub fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{stem}{ext}"));
    if !exists(&first) {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem} {n}{ext}")))
        .find(|p| !exists(p))
        .expect("unbounded range")
}

/// Like `Path::exists` but true for dangling symlinks too.
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn split_name(path: &Path) -> (String, String) {
    let name = crate::paths::display_name(path);
    // Treat dotfiles like ".env" as having no extension.
    match name.rfind('.') {
        Some(i) if i > 0 && !path.is_dir() => (name[..i].to_string(), name[i..].to_string()),
        _ => (name, String::new()),
    }
}

pub fn create_folder(dir: &Path) -> Result<PathBuf> {
    let path = unique_path(dir, "untitled folder", "");
    fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
    Ok(path)
}

pub fn create_file(dir: &Path) -> Result<PathBuf> {
    let path = unique_path(dir, "untitled", ".txt");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    Ok(path)
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("Name can't be empty");
    }
    if name == "." || name == ".." {
        bail!("\"{name}\" is a reserved name");
    }
    if name.contains('/') || name.contains('\0') {
        bail!("Names can't contain \"/\"");
    }
    Ok(())
}

/// Renames `path` within its folder. Allows case-only renames.
pub fn rename(path: &Path, new_name: &str) -> Result<PathBuf> {
    validate_name(new_name)?;
    let parent = path.parent().context("can't rename the root folder")?;
    let target = parent.join(new_name);
    if target == path {
        return Ok(target);
    }
    let case_only = crate::paths::display_name(path).to_lowercase() == new_name.to_lowercase();
    if !case_only && exists(&target) {
        bail!("\"{new_name}\" already exists");
    }
    fs::rename(path, &target).with_context(|| format!("renaming {}", path.display()))?;
    Ok(target)
}

pub fn move_to_trash(paths: &[PathBuf]) -> Result<()> {
    trash::delete_all(paths).context("moving to Trash")
}

pub fn delete_permanently(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        let meta = fs::symlink_metadata(path)?;
        if meta.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        }
        .with_context(|| format!("deleting {}", path.display()))?;
    }
    Ok(())
}

/// Copies `path` next to itself as "name copy.ext", "name copy 2.ext", ...
pub fn duplicate(path: &Path) -> Result<PathBuf> {
    let dir = path.parent().context("can't duplicate the root folder")?;
    let (stem, ext) = split_name(path);
    let target = unique_path(dir, &format!("{stem} copy"), &ext);
    copy_recursive(path, &target)?;
    Ok(target)
}

/// Copies each path into `dest_dir`. Name clashes get a " copy" suffix.
pub fn copy_into(paths: &[PathBuf], dest_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut created = Vec::new();
    for src in paths {
        ensure_not_into_itself(src, dest_dir)?;
        let name = crate::paths::display_name(src);
        let mut target = dest_dir.join(&name);
        if exists(&target) {
            let (stem, ext) = split_name(src);
            target = unique_path(dest_dir, &format!("{stem} copy"), &ext);
        }
        copy_recursive(src, &target)?;
        created.push(target);
    }
    Ok(created)
}

/// Moves each path into `dest_dir`. Refuses to overwrite existing items.
pub fn move_into(paths: &[PathBuf], dest_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut moved = Vec::new();
    for src in paths {
        ensure_not_into_itself(src, dest_dir)?;
        let target = dest_dir.join(crate::paths::display_name(src));
        if target == *src {
            moved.push(target);
            continue;
        }
        if exists(&target) {
            bail!("\"{}\" already exists in {}", crate::paths::display_name(src), dest_dir.display());
        }
        match fs::rename(src, &target) {
            Ok(()) => {}
            // Different volume: copy then remove.
            Err(e) if e.raw_os_error() == Some(18 /* EXDEV */) => {
                copy_recursive(src, &target)?;
                delete_permanently(std::slice::from_ref(src))?;
            }
            Err(e) => return Err(e).with_context(|| format!("moving {}", src.display())),
        }
        moved.push(target);
    }
    Ok(moved)
}

/// True when both paths are on the same filesystem, so a move is a cheap rename.
pub fn same_volume(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::symlink_metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

fn ensure_not_into_itself(src: &Path, dest_dir: &Path) -> Result<()> {
    if dest_dir.starts_with(src) {
        bail!("Can't put \"{}\" inside itself", crate::paths::display_name(src));
    }
    Ok(())
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
    } else if ft.is_dir() {
        fs::create_dir(dst)?;
        for item in fs::read_dir(src)? {
            let item = item?;
            copy_recursive(&item.path(), &dst.join(item.file_name()))?;
        }
        fs::set_permissions(dst, meta.permissions())?;
    } else {
        // std uses clonefile/fcopyfile on macOS, so this is cheap on APFS.
        fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_unique_folders_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let a = create_folder(tmp.path()).unwrap();
        let b = create_folder(tmp.path()).unwrap();
        assert_eq!(a.file_name().unwrap(), "untitled folder");
        assert_eq!(b.file_name().unwrap(), "untitled folder 2");
        let f = create_file(tmp.path()).unwrap();
        let g = create_file(tmp.path()).unwrap();
        assert_eq!(f.file_name().unwrap(), "untitled.txt");
        assert_eq!(g.file_name().unwrap(), "untitled 2.txt");
    }

    #[test]
    fn rename_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.txt");
        fs::write(&a, "x").unwrap();
        fs::write(tmp.path().join("b.txt"), "y").unwrap();

        assert!(rename(&a, "b.txt").is_err());
        assert!(rename(&a, "x/y").is_err());
        assert!(rename(&a, "  ").is_err());
        let upper = rename(&a, "A.txt").unwrap();
        assert_eq!(upper.file_name().unwrap(), "A.txt");
        let c = rename(&upper, "c.md").unwrap();
        assert_eq!(fs::read_to_string(c).unwrap(), "x");
    }

    #[test]
    fn duplicate_keeps_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("report.pdf");
        fs::write(&a, "x").unwrap();
        assert_eq!(duplicate(&a).unwrap().file_name().unwrap(), "report copy.pdf");
        assert_eq!(duplicate(&a).unwrap().file_name().unwrap(), "report copy 2.pdf");

        let d = tmp.path().join("v1.0");
        fs::create_dir(&d).unwrap();
        fs::write(d.join("inner"), "z").unwrap();
        let dup = duplicate(&d).unwrap();
        assert_eq!(dup.file_name().unwrap(), "v1.0 copy");
        assert_eq!(fs::read_to_string(dup.join("inner")).unwrap(), "z");
    }

    #[test]
    fn copy_and_move() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let f = src.join("f.txt");
        fs::write(&f, "1").unwrap();

        copy_into(std::slice::from_ref(&f), &dst).unwrap();
        let again = copy_into(std::slice::from_ref(&f), &dst).unwrap();
        assert_eq!(again[0].file_name().unwrap(), "f copy.txt");

        assert!(move_into(std::slice::from_ref(&f), &dst).is_err(), "must not overwrite");
        assert!(copy_into(std::slice::from_ref(&src), &src).is_err(), "no copying into itself");

        let g = src.join("g.txt");
        fs::write(&g, "2").unwrap();
        let moved = move_into(std::slice::from_ref(&g), &dst).unwrap();
        assert!(!g.exists() && moved[0].exists());
    }

    #[test]
    fn same_volume_for_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f");
        fs::write(&f, "").unwrap();
        assert!(same_volume(&f, tmp.path()));
        assert!(!same_volume(&tmp.path().join("missing"), tmp.path()));
    }

    #[test]
    fn delete_permanently_removes_trees() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("d");
        fs::create_dir_all(d.join("e")).unwrap();
        fs::write(d.join("e/f"), "").unwrap();
        delete_permanently(std::slice::from_ref(&d)).unwrap();
        assert!(!d.exists());
    }
}
