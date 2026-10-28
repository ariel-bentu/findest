//! File and folder operations. All functions are synchronous; callers run
//! them off the UI thread.

use std::fs;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Live progress of a copy or move, shared with the UI; also how it's
/// paused, cancelled and resumed. Totals are filled in by a quick scan
/// before any data is copied.
#[derive(Default)]
pub struct Progress {
    pub total_files: AtomicU64,
    pub total_bytes: AtomicU64,
    pub done_files: AtomicU64,
    pub done_bytes: AtomicU64,
    /// Set once the totals are known.
    pub counted: AtomicBool,
    cancelled: AtomicBool,
    paused: AtomicBool,
    /// Items fully transferred: a resumed transfer continues after them.
    items_done: AtomicUsize,
    /// What the item in progress is being written to, so a transfer that
    /// stopped can be discarded.
    partial: Mutex<Option<PathBuf>>,
    /// Files left out because they couldn't be read, with the reason.
    pub skipped: Mutex<Vec<(PathBuf, String)>>,
}

impl Progress {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Waits while paused; fails once cancelled.
    fn checkpoint(&self) -> Result<()> {
        while self.is_paused() && !self.is_cancelled() {
            std::thread::sleep(Duration::from_millis(100));
        }
        if self.is_cancelled() {
            bail!("Cancelled");
        }
        Ok(())
    }

    /// Notes an unreadable file and carries on.
    fn skip(&self, path: &Path, error: &std::io::Error) -> Result<()> {
        self.skipped.lock().unwrap().push((path.to_path_buf(), error.to_string()));
        Ok(())
    }

    /// Removes what the stopped transfer had partly written (a stopped move
    /// still has its original).
    pub fn discard_partial(&self) {
        if let Some(path) = self.partial.lock().unwrap().take() {
            let _ = delete_permanently(&[path]);
        }
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
        self.checkpoint()
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
        remove_trashinfo(path);
    }
    Ok(())
}

/// Deleting an item from a freedesktop Trash (`Trash/files/NAME`) also removes
/// its `Trash/info/NAME.trashinfo` record, so nothing is left to "restore".
fn remove_trashinfo(path: &Path) {
    let Some(files) = path.parent().filter(|f| f.file_name().is_some_and(|n| n == "files")) else { return };
    let Some(trash) = files.parent().filter(|t| t.join("info").is_dir()) else { return };
    let Some(name) = path.file_name() else { return };
    let mut record = name.to_os_string();
    record.push(".trashinfo");
    let _ = fs::remove_file(trash.join("info").join(record));
}

fn trash_entries(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    Ok(fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| !p.ends_with(".DS_Store"))
        .collect())
}

/// The number of items in the Trash whose files are in `dir`. On Linux this
/// also counts items trashed from other drives (`$topdir/.Trash-$uid`).
pub fn trash_item_count(dir: &Path) -> std::io::Result<usize> {
    let here = trash_entries(dir)?.len();
    #[cfg(not(target_os = "macos"))]
    {
        // Items in the home Trash are in both counts, so take the larger.
        if let Ok(all) = trash::os_limited::list() {
            return Ok(all.len().max(here));
        }
    }
    Ok(here)
}

/// Permanently erases everything in the Trash folder `dir`, and on Linux the
/// `info` records beside it that no longer have a file.
pub fn empty_trash_folder(dir: &Path) -> Result<()> {
    delete_permanently(&trash_entries(dir)?)?;
    #[cfg(not(target_os = "macos"))]
    if let Some(info) = dir.parent().map(|t| t.join("info")).filter(|i| i.is_dir())
        && let Ok(read) = fs::read_dir(info)
    {
        for record in read.filter_map(|e| e.ok()) {
            let _ = fs::remove_file(record.path());
        }
    }
    Ok(())
}

/// Empties the user's Trash: the folder `dir` and, on Linux, the Trash folders
/// of other drives. This erases the real system Trash: tests use
/// [`empty_trash_folder`] on a temporary folder instead.
pub fn empty_trash(dir: &Path) -> Result<()> {
    #[cfg(not(target_os = "macos"))]
    if let Ok(all) = trash::os_limited::list() {
        trash::os_limited::purge_all(all).context("emptying the Trash")?;
    }
    empty_trash_folder(dir)
}

/// Copies `path` next to itself as "name copy.ext", "name copy 2.ext", ...
pub fn duplicate(path: &Path) -> Result<PathBuf> {
    let dir = path.parent().context("can't duplicate the root folder")?;
    let (stem, ext) = split_name(path);
    let target = unique_path(dir, &format!("{stem} copy"), &ext);
    copy_recursive(path, &target, false, false, &Progress::default())?;
    Ok(target)
}

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

/// One item of a copy or move: where from, where to.
pub type Planned = (PathBuf, PathBuf);

/// Decides where each of `paths` goes in `dest_dir`, settling name clashes
/// (replacing moves the existing item to the Trash now). Skipped items are
/// left out.
pub fn plan(paths: &[PathBuf], dest_dir: &Path, copying: bool, on_conflict: OnConflict) -> Result<Vec<Planned>> {
    let mut items = Vec::new();
    for src in paths {
        ensure_not_into_itself(src, dest_dir)?;
        if let Some(target) = place(src, dest_dir, copying, on_conflict)? {
            items.push((src.clone(), target));
        }
    }
    Ok(items)
}

/// Copies or moves the planned items. Copies within a volume are clones;
/// to another volume data goes in chunks, so progress moves within big
/// files and pausing or cancelling takes effect quickly.
///
/// Unreadable files are skipped and listed in `progress.skipped` (a move
/// then keeps that item's original). A cancelled transfer removes the item
/// it was on. Any other failure (a network volume going away, a full disk)
/// keeps what was written: run it again with `resume` to continue, from the
/// item it stopped on, keeping files that are complete and finishing a
/// partly copied one; or call `progress.discard_partial()`.
pub fn transfer(items: &[Planned], moving: bool, resume: bool, progress: &Progress) -> Result<()> {
    if !resume {
        progress.items_done.store(0, Ordering::Relaxed);
        let to_copy: Vec<PathBuf> = items
            .iter()
            .filter(|(src, dst)| !moving || dst.parent().is_some_and(|d| !same_volume(src, d)))
            .map(|(src, _)| src.clone())
            .collect();
        progress.count(&to_copy);
        progress.counted.store(true, Ordering::Relaxed);
    }
    let start = progress.items_done.load(Ordering::Relaxed);
    for (src, dst) in items.iter().skip(start) {
        progress.checkpoint()?;
        transfer_item(src, dst, moving, resume, progress)?;
        progress.items_done.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

fn transfer_item(src: &Path, dst: &Path, moving: bool, resume: bool, progress: &Progress) -> Result<()> {
    if moving && src == dst {
        return Ok(());
    }
    if moving && !resume {
        match fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(18 /* EXDEV */) => {} // other volume: copy, then remove
            Err(e) => return Err(e).with_context(|| format!("moving {}", src.display())),
        }
    }
    let chunked = dst.parent().is_some_and(|d| !same_volume(src, d));
    *progress.partial.lock().unwrap() = Some(dst.to_path_buf());
    let skipped_before = progress.skipped.lock().unwrap().len();
    if let Err(e) = copy_recursive(src, dst, chunked, resume, progress) {
        if progress.is_cancelled() {
            progress.discard_partial();
        }
        return Err(e);
    }
    *progress.partial.lock().unwrap() = None;
    // Don't remove originals of files that couldn't be copied.
    if moving && progress.skipped.lock().unwrap().len() == skipped_before {
        delete_permanently(&[src.to_path_buf()])?;
    }
    Ok(())
}

/// Copies `paths` into `dest_dir`; returns what was created (skipped items
/// are left out).
pub fn copy_into(paths: &[PathBuf], dest_dir: &Path, on_conflict: OnConflict, progress: &Progress) -> Result<Vec<PathBuf>> {
    let items = plan(paths, dest_dir, true, on_conflict)?;
    transfer(&items, false, false, progress)?;
    Ok(items.into_iter().map(|(_, dst)| dst).collect())
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
    let status = if cfg!(target_os = "macos") && paths.len() == 1 {
        // ditto is what Finder uses: keeps permissions, symlinks and Mac metadata.
        std::process::Command::new("/usr/bin/ditto")
            .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
            .arg(first)
            .arg(&target)
            .status()
    } else {
        let names: Vec<_> = paths.iter().filter_map(|p| p.file_name()).collect();
        std::process::Command::new("zip")
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
        let status = if cfg!(target_os = "macos") {
            std::process::Command::new("/usr/bin/ditto")
                .args(["-x", "-k"])
                .arg(zip)
                .arg(&staging)
                .status()
                .context("running ditto")?
        } else {
            std::process::Command::new("unzip")
                .args(["-q"])
                .arg(zip)
                .arg("-d")
                .arg(&staging)
                .status()
                .context("running unzip")?
        };
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

/// Moves `paths` into `dest_dir` (only moves to another volume copy data,
/// so only those count as progress); returns `(from, to)` for each item
/// (skipped items are left out).
pub fn move_into(paths: &[PathBuf], dest_dir: &Path, on_conflict: OnConflict, progress: &Progress) -> Result<Vec<Planned>> {
    let items = plan(paths, dest_dir, false, on_conflict)?;
    transfer(&items, true, false, progress)?;
    Ok(items)
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

/// An error reading the source that means "skip this file" rather than
/// "stop": it's unreadable or vanished.
fn skippable(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound)
}

fn copy_recursive(src: &Path, dst: &Path, chunked: bool, resume: bool, progress: &Progress) -> Result<()> {
    let meta = match fs::symlink_metadata(src) {
        Ok(meta) => meta,
        Err(e) if skippable(&e) => return progress.skip(src, &e),
        Err(e) => return Err(e).with_context(|| format!("reading {}", src.display())),
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        if !(resume && exists(dst)) {
            std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
        }
        progress.file_done(0)?;
    } else if ft.is_dir() {
        let entries = match fs::read_dir(src) {
            Ok(entries) => entries,
            Err(e) if skippable(&e) => return progress.skip(src, &e),
            Err(e) => return Err(e).with_context(|| format!("reading {}", src.display())),
        };
        if !(resume && dst.is_dir()) {
            fs::create_dir(dst).with_context(|| format!("creating {}", dst.display()))?;
        }
        for item in entries {
            let item = item?;
            copy_recursive(&item.path(), &dst.join(item.file_name()), chunked, resume, progress)?;
        }
        fs::set_permissions(dst, meta.permissions())?;
        copy_metadata(src, dst, &meta);
    } else {
        copy_file(src, dst, &meta, chunked, resume, progress)?;
    }
    Ok(())
}

/// Bytes per chunk when copying to another volume.
const CHUNK: usize = 4 << 20;

fn copy_file(src: &Path, dst: &Path, meta: &fs::Metadata, chunked: bool, resume: bool, progress: &Progress) -> Result<()> {
    let len = meta.len();
    // Resuming: a file finished before the interruption has the source's
    // size and date (the date is set last); a shorter one is continued.
    let existing = if resume { fs::symlink_metadata(dst).ok().filter(|d| d.is_file()) } else { None };
    if existing.as_ref().is_some_and(|d| d.len() == len && d.modified().ok() == meta.modified().ok()) {
        return progress.checkpoint(); // counted before the interruption
    }
    let mut input = match fs::File::open(src) {
        Ok(file) => file,
        Err(e) if skippable(&e) => return progress.skip(src, &e),
        Err(e) => return Err(e).with_context(|| format!("reading {}", src.display())),
    };
    if !chunked {
        #[cfg(target_os = "linux")]
        if try_ficlone(&input, dst, meta) {
            copy_metadata(src, dst, meta);
            return progress.file_done(len);
        }
        // std uses clonefile/fcopyfile on macOS, so this is cheap on APFS.
        drop(input);
        fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
        copy_metadata(src, dst, meta);
        return progress.file_done(len);
    }

    let offset = existing.map(|d| d.len()).filter(|&n| n < len).unwrap_or(0);
    let mut output = if offset > 0 {
        input.seek(std::io::SeekFrom::Start(offset))?;
        fs::OpenOptions::new().append(true).open(dst)
    } else {
        fs::File::create(dst)
    }
    .with_context(|| format!("writing {}", dst.display()))?;
    let mut buf = vec![0u8; CHUNK.min(len as usize).max(1)];
    loop {
        progress.checkpoint()?;
        let n = input.read(&mut buf).with_context(|| format!("reading {}", src.display()))?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n]).with_context(|| format!("writing {}", dst.display()))?;
        progress.done_bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
    output.sync_all().with_context(|| format!("writing {}", dst.display()))?;
    drop(output);
    copy_metadata(src, dst, meta);
    progress.file_done(0)
}

#[cfg(target_os = "linux")]
fn try_ficlone(src_file: &fs::File, dst: &Path, meta: &fs::Metadata) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(dst_file) = fs::File::create(dst) else {
        return false;
    };
    let rc = unsafe { libc::ioctl(dst_file.as_raw_fd(), libc::FICLONE as _, src_file.as_raw_fd()) };
    if rc == 0 {
        let _ = dst_file.set_permissions(meta.permissions());
        true
    } else {
        drop(dst_file);
        let _ = fs::remove_file(dst);
        false
    }
}

/// Permissions, extended attributes (tags, Finder info) and dates, like
/// Finder keeps them. The date goes last: it marks the file complete.
fn copy_metadata(src: &Path, dst: &Path, meta: &fs::Metadata) {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        if let (Ok(from), Ok(to)) = (std::ffi::CString::new(src.as_os_str().as_bytes()), std::ffi::CString::new(dst.as_os_str().as_bytes())) {
            unsafe { libc::copyfile(from.as_ptr(), to.as_ptr(), std::ptr::null_mut(), libc::COPYFILE_METADATA) };
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::os::unix::ffi::OsStrExt;
        const COLOR_XATTR: &[u8] = b"user.findest.color\0";
        if let (Ok(from), Ok(to)) = (
            std::ffi::CString::new(src.as_os_str().as_bytes()),
            std::ffi::CString::new(dst.as_os_str().as_bytes()),
        ) {
            let mut buf = [0u8; 32];
            let n = unsafe {
                libc::getxattr(
                    from.as_ptr(),
                    COLOR_XATTR.as_ptr().cast(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                )
            };
            if n > 0 && (n as usize) <= buf.len() {
                unsafe {
                    libc::setxattr(
                        to.as_ptr(),
                        COLOR_XATTR.as_ptr().cast(),
                        buf.as_ptr().cast(),
                        n as usize,
                        0,
                    );
                }
            }
        }
        let _ = fs::set_permissions(dst, meta.permissions());
    }
    if let (Ok(file), Ok(modified)) = (fs::OpenOptions::new().write(true).open(dst), meta.modified()) {
        let _ = file.set_modified(modified);
    }
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

    /// 9 MB of varied bytes: more than two chunks.
    fn big_file(path: &Path) -> Vec<u8> {
        let data: Vec<u8> = (0..9_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        fs::write(path, &data).unwrap();
        data
    }

    #[test]
    fn chunked_copy_keeps_contents_and_date() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("big.bin"), tmp.path().join("copy.bin"));
        let data = big_file(&src);
        let p = Progress::default();
        copy_recursive(&src, &dst, true, false, &p).unwrap();
        assert!(fs::read(&dst).unwrap() == data);
        assert_eq!(fs::metadata(&dst).unwrap().modified().unwrap(), fs::metadata(&src).unwrap().modified().unwrap());
        assert_eq!(p.done_bytes.load(Ordering::Relaxed), data.len() as u64);
        assert_eq!(p.done_files.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn resume_finishes_a_partial_file_and_keeps_complete_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("big.bin"), tmp.path().join("copy.bin"));
        let data = big_file(&src);
        fs::write(&dst, &data[..3_000_000]).unwrap(); // interrupted after 3 MB
        let p = Progress::default();
        copy_recursive(&src, &dst, true, true, &p).unwrap();
        assert!(fs::read(&dst).unwrap() == data, "continued, not restarted or corrupted");
        assert_eq!(p.done_bytes.load(Ordering::Relaxed), 6_000_000, "only the rest is copied");

        let again = Progress::default();
        copy_recursive(&src, &dst, true, true, &again).unwrap();
        assert_eq!(again.done_bytes.load(Ordering::Relaxed), 0, "a complete file is kept as is");
    }

    #[test]
    fn unreadable_files_are_skipped_and_listed() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("ok.txt"), "fine").unwrap();
        fs::write(src.join("secret.txt"), "no").unwrap();
        fs::set_permissions(src.join("secret.txt"), fs::Permissions::from_mode(0o000)).unwrap();
        let dst = tmp.path().join("dst");
        fs::create_dir(&dst).unwrap();

        let p = Progress::default();
        let items = vec![(src.clone(), dst.join("src"))];
        transfer(&items, false, false, &p).unwrap();
        assert!(dst.join("src/ok.txt").exists() && !dst.join("src/secret.txt").exists());
        let skipped = p.skipped.lock().unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].0, src.join("secret.txt"));
        fs::set_permissions(src.join("secret.txt"), fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn pause_holds_and_resume_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("big.bin"), tmp.path().join("copy.bin"));
        let data = big_file(&src);
        let p = std::sync::Arc::new(Progress::default());
        p.set_paused(true);
        let worker = {
            let (p, src, dst) = (p.clone(), src.clone(), dst.clone());
            std::thread::spawn(move || copy_recursive(&src, &dst, true, false, &p))
        };
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(p.done_bytes.load(Ordering::Relaxed), 0, "nothing copied while paused");
        p.set_paused(false);
        worker.join().unwrap().unwrap();
        assert!(fs::read(&dst).unwrap() == data);
    }

    #[test]
    fn a_failed_transfer_resumes_from_the_item_it_stopped_on() {
        let tmp = tempfile::tempdir().unwrap();
        let dst = tmp.path().join("dst");
        fs::create_dir(&dst).unwrap();
        let (a, b) = (tmp.path().join("a.txt"), tmp.path().join("b.txt"));
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        // b's destination folder is missing, like a volume that went away.
        let items = vec![(a.clone(), dst.join("a.txt")), (b.clone(), dst.join("gone/b.txt"))];
        let p = Progress::default();
        assert!(transfer(&items, false, false, &p).is_err());
        assert!(dst.join("a.txt").exists(), "work done before the failure stays");
        assert_eq!(*p.partial.lock().unwrap(), Some(dst.join("gone/b.txt")), "remembers what to discard");

        fs::write(dst.join("a.txt"), "edited since").unwrap();
        fs::create_dir(dst.join("gone")).unwrap(); // it came back
        transfer(&items, false, true, &p).unwrap();
        assert_eq!(fs::read_to_string(dst.join("gone/b.txt")).unwrap(), "b");
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "edited since", "finished items aren't redone");
    }

    #[test]
    fn discarding_removes_the_partly_copied_item() {
        let tmp = tempfile::tempdir().unwrap();
        let partial = tmp.path().join("half");
        fs::create_dir_all(partial.join("sub")).unwrap();
        fs::write(partial.join("sub/x"), "x").unwrap();
        let p = Progress::default();
        *p.partial.lock().unwrap() = Some(partial.clone());
        p.discard_partial();
        assert!(!partial.exists());
    }

    #[test]
    fn a_move_keeps_originals_it_could_not_copy() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("ok.txt"), "fine").unwrap();
        fs::write(src.join("secret.txt"), "no").unwrap();
        fs::set_permissions(src.join("secret.txt"), fs::Permissions::from_mode(0o000)).unwrap();
        let dst = tmp.path().join("dst");
        fs::create_dir(&dst).unwrap();
        // `resume` takes the copy-then-remove path a move to another volume uses.
        let p = Progress::default();
        transfer(&[(src.clone(), dst.join("src"))], true, true, &p).unwrap();
        assert!(dst.join("src/ok.txt").exists());
        assert!(src.join("secret.txt").exists(), "the unreadable original isn't deleted");
        fs::set_permissions(src.join("secret.txt"), fs::Permissions::from_mode(0o644)).unwrap();

        // Without skips, the original goes once copied.
        let other = tmp.path().join("other");
        fs::create_dir(&other).unwrap();
        fs::write(other.join("a"), "a").unwrap();
        transfer(&[(other.clone(), dst.join("other"))], true, true, &Progress::default()).unwrap();
        assert!(!other.exists() && dst.join("other/a").exists());
    }

    #[test]
    fn cancelling_a_chunked_copy_removes_the_partial_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("big.bin"), tmp.path().join("dst"));
        big_file(&src);
        fs::create_dir(&dst).unwrap();
        let p = std::sync::Arc::new(Progress::default());
        p.set_paused(true); // hold it mid-way, then cancel
        let worker = {
            let (p, src, dst) = (p.clone(), src.clone(), dst.clone());
            std::thread::spawn(move || transfer(&[(src, dst.join("big.bin"))], false, false, &p))
        };
        std::thread::sleep(Duration::from_millis(200));
        p.cancel();
        assert!(worker.join().unwrap().is_err());
        assert!(!dst.join("big.bin").exists());
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
    fn deleting_from_the_trash_removes_its_record() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = tmp.path().join("Trash");
        fs::create_dir_all(trash.join("files/old")).unwrap();
        fs::create_dir_all(trash.join("info")).unwrap();
        fs::write(trash.join("files/old/a.txt"), "a").unwrap();
        fs::write(trash.join("files/b.txt"), "b").unwrap();
        fs::write(trash.join("info/old.trashinfo"), "[Trash Info]").unwrap();
        fs::write(trash.join("info/b.txt.trashinfo"), "[Trash Info]").unwrap();
        fs::write(trash.join("info/c.txt.trashinfo"), "[Trash Info]").unwrap();

        delete_permanently(&[trash.join("files/b.txt")]).unwrap();
        assert!(!trash.join("info/b.txt.trashinfo").exists());
        assert!(trash.join("info/old.trashinfo").exists(), "other records stay");

        delete_permanently(&[trash.join("files/old")]).unwrap();
        assert!(!trash.join("info/old.trashinfo").exists());
        assert!(trash.join("info/c.txt.trashinfo").exists());
    }

    #[test]
    fn empties_a_trash_folder_with_its_records() {
        let tmp = tempfile::tempdir().unwrap();
        let files = tmp.path().join("Trash/files");
        fs::create_dir_all(&files).unwrap();
        fs::create_dir_all(tmp.path().join("Trash/info")).unwrap();
        fs::write(files.join("a.txt"), "a").unwrap();
        fs::create_dir(files.join("dir")).unwrap();
        fs::write(tmp.path().join("Trash/info/a.txt.trashinfo"), "").unwrap();
        fs::write(tmp.path().join("Trash/info/orphan.trashinfo"), "").unwrap();
        assert!(trash_item_count(&files).unwrap() >= 2);
        empty_trash_folder(&files).unwrap();
        assert_eq!(fs::read_dir(&files).unwrap().count(), 0);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(fs::read_dir(tmp.path().join("Trash/info")).unwrap().count(), 0);
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
        assert!(copy_into(std::slice::from_ref(&src), &dst, OnConflict::KeepBoth, &cancelled).is_err());
        assert!(!dst.join("src 2").exists(), "partial copy is removed");
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
