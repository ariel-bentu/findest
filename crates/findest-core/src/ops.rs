//! File and folder operations. All functions are synchronous; callers run
//! them off the UI thread.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

/// Live progress of a copy or move, shared with the UI; also how it's cancelled.
/// Totals are filled in by a quick scan before any data is copied.
#[derive(Default)]
pub struct Progress {
    pub total_files: AtomicU64,
    pub total_bytes: AtomicU64,
    pub done_files: AtomicU64,
    pub done_bytes: AtomicU64,
    /// Set once the totals are known.
    pub counted: AtomicBool,
    cancelled: AtomicBool,
}

impl Progress {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Adds `paths` (recursively) to the totals.
    fn count(&self, paths: &[PathBuf]) {
        fn walk(path: &Path, p: &Progress) {
            let Ok(meta) = fs::symlink_metadata(path) else { return };
            if meta.is_dir() {
                for item in fs::read_dir(path).into_iter().flatten().flatten() {
                    walk(&item.path(), p);
                }
            } else {
                p.total_files.fetch_add(1, Ordering::Relaxed);
                p.total_bytes.fetch_add(meta.len(), Ordering::Relaxed);
            }
        }
        for path in paths {
            walk(path, self);
        }
    }

    fn file_done(&self, bytes: u64) -> Result<()> {
        self.done_files.fetch_add(1, Ordering::Relaxed);
        self.done_bytes.fetch_add(bytes, Ordering::Relaxed);
        if self.is_cancelled() {
            bail!("Cancelled");
        }
        Ok(())
    }
}

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

/// Removes a just-created item whose naming was cancelled, but only while it
/// is still an empty folder or an empty file.
pub fn discard_new(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir(path)?;
    } else if meta.is_file() && meta.len() == 0 {
        fs::remove_file(path)?;
    } else {
        bail!("\"{}\" isn't empty any more", crate::paths::display_name(path));
    }
    Ok(())
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
    copy_recursive(path, &target, &Progress::default())?;
    Ok(target)
}

/// Copies each path into `dest_dir`. Name clashes get a " copy" suffix. A
/// failed or cancelled copy removes the partly copied item.
/// What to do when something with the same name is already where an item
/// is going.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnConflict {
    /// Move the existing item to the Trash, then put the new one there.
    Replace,
    /// Give the new one a free name ("report 2.pdf").
    KeepBoth,
    /// Leave that item where it is.
    Skip,
    /// Stop with an error.
    Fail,
}

/// The items in `paths` whose name is already taken in `dest_dir`. Copying
/// an item into its own folder doesn't count (the copy is named "… copy"),
/// and neither does moving it there (nothing to do).
pub fn conflicts(paths: &[PathBuf], dest_dir: &Path) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|src| src.parent() != Some(dest_dir) && exists(&dest_dir.join(crate::paths::display_name(src))))
        .cloned()
        .collect()
}

/// Where `src` goes in `dest_dir`, or `None` to skip it. Copies into an
/// item's own folder are named "… copy"; other clashes follow `on_conflict`
/// (replacing moves the existing item to the Trash).
fn place(src: &Path, dest_dir: &Path, copying: bool, on_conflict: OnConflict) -> Result<Option<PathBuf>> {
    let name = crate::paths::display_name(src);
    let target = dest_dir.join(&name);
    let (stem, ext) = split_name(src);
    if copying && target == src {
        return Ok(Some(unique_path(dest_dir, &format!("{stem} copy"), &ext)));
    }
    if !exists(&target) || target == src {
        return Ok(Some(target)); // free, or a move to where it already is
    }
    match on_conflict {
        OnConflict::Skip => Ok(None),
        OnConflict::KeepBoth => Ok(Some(unique_path(dest_dir, &stem, &ext))),
        OnConflict::Replace => {
            if src.starts_with(&target) {
                bail!("Can't replace “{name}”: it contains the item being moved");
            }
            move_to_trash(std::slice::from_ref(&target))?;
            Ok(Some(target))
        }
        OnConflict::Fail => bail!("\"{name}\" already exists in {}", dest_dir.display()),
    }
}

/// Copies `paths` into `dest_dir`; returns what was created (skipped items
/// are left out).
pub fn copy_into(paths: &[PathBuf], dest_dir: &Path, on_conflict: OnConflict, progress: &Progress) -> Result<Vec<PathBuf>> {
    progress.count(paths);
    progress.counted.store(true, Ordering::Relaxed);
    let mut created = Vec::new();
    for src in paths {
        ensure_not_into_itself(src, dest_dir)?;
        let Some(target) = place(src, dest_dir, true, on_conflict)? else { continue };
        copy_fresh(src, &target, progress)?;
        created.push(target);
    }
    Ok(created)
}

/// Zips `paths` next to them, like Finder's Compress: one item becomes
/// "name.zip", several become "Archive.zip" (" 2", " 3"... on clashes).
/// The items must share a folder.
pub fn compress(paths: &[PathBuf]) -> Result<PathBuf> {
    let first = paths.first().context("nothing to compress")?;
    let dir = first.parent().context("can't compress the root folder")?;
    if paths.iter().any(|p| p.parent() != Some(dir)) {
        bail!("Items to compress together must be in the same folder");
    }
    let stem = if paths.len() == 1 { crate::paths::display_name(first) } else { "Archive".to_string() };
    let target = unique_path(dir, &stem, ".zip");
    let status = if paths.len() == 1 {
        // ditto is what Finder uses: keeps permissions, symlinks and Mac metadata.
        std::process::Command::new("/usr/bin/ditto")
            .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
            .arg(first)
            .arg(&target)
            .status()
    } else {
        let names: Vec<_> = paths.iter().filter_map(|p| p.file_name()).collect();
        std::process::Command::new("/usr/bin/zip")
            .args(["-r", "-y", "-q"])
            .arg(&target)
            .args(names)
            .current_dir(dir)
            .status()
    }
    .context("running the zip tool")?;
    if !status.success() {
        let _ = fs::remove_file(&target);
        bail!("Couldn't compress ({status})");
    }
    Ok(target)
}

pub fn is_zip(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) && path.is_file()
}

/// Unzips `zip` next to it, like Archive Utility: a single top-level item is
/// placed beside the zip, several go into a folder named after it. Names
/// that clash get " 2", " 3"... Returns what was created.
pub fn extract(zip: &Path) -> Result<PathBuf> {
    let dir = zip.parent().context("no parent folder")?;
    // Unpack into a hidden staging folder first so a failure leaves nothing behind.
    let staging = unique_path(dir, ".findest-extract", "");
    fs::create_dir(&staging).with_context(|| format!("creating {}", staging.display()))?;
    let result = (|| {
        let status = std::process::Command::new("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(zip)
            .arg(&staging)
            .status()
            .context("running ditto")?;
        if !status.success() {
            bail!("Couldn't extract \"{}\" ({status})", crate::paths::display_name(zip));
        }
        let items: Vec<PathBuf> = fs::read_dir(&staging)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().is_some_and(|n| n != "__MACOSX" && n != ".DS_Store"))
            .collect();
        if let [single] = items.as_slice() {
            let (stem, ext) = split_name(single);
            let target = unique_path(dir, &stem, &ext);
            fs::rename(single, &target)?;
            fs::remove_dir_all(&staging)?;
            Ok(target)
        } else {
            let (stem, _) = split_name(zip);
            let target = unique_path(dir, &stem, "");
            fs::rename(&staging, &target)?;
            let _ = fs::remove_dir_all(target.join("__MACOSX"));
            Ok(target)
        }
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

/// Moves each path into `dest_dir`. Refuses to overwrite existing items.
/// Only moves to another volume copy data, so only those count as progress.
/// Moves `paths` into `dest_dir`; returns `(from, to)` for each item moved
/// (skipped items are left out).
pub fn move_into(paths: &[PathBuf], dest_dir: &Path, on_conflict: OnConflict, progress: &Progress) -> Result<Vec<(PathBuf, PathBuf)>> {
    let across: Vec<PathBuf> = paths.iter().filter(|p| !same_volume(p, dest_dir)).cloned().collect();
    progress.count(&across);
    progress.counted.store(true, Ordering::Relaxed);
    let mut moved = Vec::new();
    for src in paths {
        ensure_not_into_itself(src, dest_dir)?;
        let Some(target) = place(src, dest_dir, false, on_conflict)? else { continue };
        if target == *src {
            moved.push((src.clone(), target));
            continue;
        }
        match fs::rename(src, &target) {
            Ok(()) => {}
            // Different volume: copy then remove.
            Err(e) if e.raw_os_error() == Some(18 /* EXDEV */) => {
                copy_fresh(src, &target, progress)?;
                delete_permanently(std::slice::from_ref(src))?;
            }
            Err(e) => return Err(e).with_context(|| format!("moving {}", src.display())),
        }
        moved.push((src.clone(), target));
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

/// Copies to a `dst` that doesn't exist yet, removing whatever was copied if it fails.
fn copy_fresh(src: &Path, dst: &Path, progress: &Progress) -> Result<()> {
    let result = copy_recursive(src, dst, progress);
    if result.is_err() && exists(dst) {
        let _ = delete_permanently(&[dst.to_path_buf()]);
    }
    result
}

fn copy_recursive(src: &Path, dst: &Path, progress: &Progress) -> Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
        progress.file_done(0)?;
    } else if ft.is_dir() {
        fs::create_dir(dst)?;
        for item in fs::read_dir(src)? {
            let item = item?;
            copy_recursive(&item.path(), &dst.join(item.file_name()), progress)?;
        }
        fs::set_permissions(dst, meta.permissions())?;
    } else {
        // std uses clonefile/fcopyfile on macOS, so this is cheap on APFS.
        fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
        progress.file_done(meta.len())?;
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

        discard_new(&a).unwrap();
        discard_new(&f).unwrap();
        assert!(!a.exists() && !f.exists());
        fs::write(&g, "keep").unwrap();
        fs::write(b.join("x"), "").unwrap();
        assert!(discard_new(&g).is_err() && discard_new(&b).is_err());
        assert!(g.exists() && b.exists());
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

        let p = Progress::default();
        let one = |path: &PathBuf| std::slice::from_ref(path).to_vec();
        copy_into(&one(&f), &dst, OnConflict::Fail, &p).unwrap();
        let dup = copy_into(&one(&f), &src, OnConflict::Fail, &p).unwrap();
        assert_eq!(dup[0].file_name().unwrap(), "f copy.txt", "a copy into its own folder never asks");

        assert_eq!(conflicts(&one(&f), &dst), one(&f));
        assert!(conflicts(&one(&f), &src).is_empty(), "its own folder isn't a conflict");
        assert!(copy_into(&one(&f), &dst, OnConflict::Fail, &p).is_err(), "must not overwrite");
        assert!(move_into(&one(&f), &dst, OnConflict::Fail, &p).is_err(), "must not overwrite");
        assert!(copy_into(&one(&src), &src, OnConflict::Fail, &p).is_err(), "no copying into itself");

        let kept = copy_into(&one(&f), &dst, OnConflict::KeepBoth, &p).unwrap();
        assert_eq!(kept[0].file_name().unwrap(), "f 2.txt");
        assert!(copy_into(&one(&f), &dst, OnConflict::Skip, &p).unwrap().is_empty());
        assert!(move_into(&one(&f), &dst, OnConflict::Skip, &p).unwrap().is_empty() && f.exists());

        let g = src.join("g.txt");
        fs::write(&g, "2").unwrap();
        let moved = move_into(&one(&g), &dst, OnConflict::Fail, &p).unwrap();
        assert!(!g.exists() && moved[0].1.exists() && moved[0].0 == g);
    }

    #[test]
    #[ignore = "moves a file to the real Trash"]
    fn replace_trashes_the_existing_item() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("src"), tmp.path().join("dst"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("f.txt"), "new").unwrap();
        fs::write(dst.join("f.txt"), "old").unwrap();
        let moved = move_into(&[src.join("f.txt")], &dst, OnConflict::Replace, &Progress::default()).unwrap();
        assert_eq!(fs::read_to_string(&moved[0].1).unwrap(), "new");
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

    #[test]
    fn copy_reports_progress_and_cancels_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a"), "12345").unwrap();
        fs::write(src.join("sub/b"), "123").unwrap();
        let dst = tmp.path().join("dst");
        fs::create_dir(&dst).unwrap();

        let progress = Progress::default();
        copy_into(std::slice::from_ref(&src), &dst, OnConflict::Fail, &progress).unwrap();
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        assert_eq!((get(&progress.total_files), get(&progress.total_bytes)), (2, 8));
        assert_eq!((get(&progress.done_files), get(&progress.done_bytes)), (2, 8));

        let cancelled = Progress::default();
        cancelled.cancel();
        assert!(copy_into(std::slice::from_ref(&src), &dst, OnConflict::Fail, &cancelled).is_err());
        assert!(!dst.join("src copy").exists(), "partial copy is removed");
    }

    #[test]
    fn compresses_like_finder() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("x.txt");
        let folder = tmp.path().join("docs");
        fs::write(&file, "hello").unwrap();
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("a.md"), "a").unwrap();

        assert_eq!(compress(std::slice::from_ref(&file)).unwrap(), tmp.path().join("x.txt.zip"));
        assert_eq!(compress(std::slice::from_ref(&folder)).unwrap(), tmp.path().join("docs.zip"));
        let both = [file.clone(), folder.clone()];
        assert_eq!(compress(&both).unwrap(), tmp.path().join("Archive.zip"));
        assert_eq!(compress(&both).unwrap(), tmp.path().join("Archive 2.zip"));
        let listing = std::process::Command::new("unzip").arg("-l").arg(tmp.path().join("Archive.zip")).output().unwrap();
        let listing = String::from_utf8_lossy(&listing.stdout);
        assert!(listing.contains("x.txt") && listing.contains("docs/a.md"), "{listing}");
        assert!(compress(&[file, tmp.path().join("docs/a.md")]).is_err(), "different folders");

        // A single item comes out beside the zip (renamed on a clash); several get a folder.
        assert_eq!(extract(&tmp.path().join("docs.zip")).unwrap(), tmp.path().join("docs 2"));
        assert!(tmp.path().join("docs 2/a.md").exists());
        let unpacked = extract(&tmp.path().join("Archive.zip")).unwrap();
        assert_eq!(unpacked, tmp.path().join("Archive"));
        assert!(unpacked.join("x.txt").exists() && unpacked.join("docs/a.md").exists());
        assert!(!fs::read_dir(tmp.path()).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".findest")));
    }
}
