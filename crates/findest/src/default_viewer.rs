//! Making Findest the default file viewer instead of Finder, and back.
//!
//! This is the `NSFileViewer` global default, which "Show in Finder" /
//! "Reveal in Finder" in other apps use: macOS sends the viewer an "open
//! documents" event with the file, and Findest shows its folder with it
//! selected. (The default app for folders can't be changed: macOS refuses
//! it for `public.folder`, with either LaunchServices API.)
//!
//! If Findest is deleted without switching back, reveals fall back to Finder
//! (macOS ignores a viewer that isn't installed), but the setting lingers.

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CStr, c_char, c_void};

    use anyhow::{Result, bail};

    use crate::install::BUNDLE_ID;

const FILE_VIEWER_KEY: &str = "NSFileViewer";

/// Whether "Show in Finder" in other apps opens Findest.
pub fn is_default() -> bool {
    file_viewer().as_deref() == Some(BUNDLE_ID)
}

/// Makes "Show in Finder" in other apps open Findest.
pub fn use_findest() -> Result<()> {
    set_file_viewer(Some(BUNDLE_ID));
    if !is_default() {
        bail!("Couldn't save the setting");
    }
    Ok(())
}

/// Puts Finder back.
pub fn use_finder() -> Result<()> {
    set_file_viewer(None);
    if is_default() {
        bail!("Couldn't save the setting");
    }
    Ok(())
}

/// Findest's own "Reveal in Finder". While Findest handles reveals, the
/// usual way would come back to Findest, so ask Finder directly (macOS asks
/// once whether Findest may control Finder).
pub fn reveal_in_finder(path: &std::path::Path) -> Result<()> {
    if file_viewer().as_deref() != Some(BUNDLE_ID) {
        return findest_core::launch::reveal_in_finder(path);
    }
    std::process::Command::new("/usr/bin/osascript")
        .args(["-e", "on run argv", "-e", "tell application \"Finder\"", "-e", "reveal POSIX file (item 1 of argv)"])
        .args(["-e", "activate", "-e", "end tell", "-e", "end run"])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

fn file_viewer() -> Option<String> {
    let key = CfString::new(FILE_VIEWER_KEY);
    let value = unsafe { CFPreferencesCopyAppValue(key.0, kCFPreferencesAnyApplication) };
    if value.is_null() || unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
        if !value.is_null() {
            unsafe { CFRelease(value) };
        }
        return None;
    }
    unsafe { CfString::take(value) }
}

/// `None` removes the setting (Finder is the default).
fn set_file_viewer(bundle_id: Option<&str>) {
    let key = CfString::new(FILE_VIEWER_KEY);
    let value = bundle_id.map(CfString::new);
    unsafe {
        CFPreferencesSetAppValue(key.0, value.as_ref().map_or(std::ptr::null(), |v| v.0), kCFPreferencesAnyApplication);
        CFPreferencesAppSynchronize(kCFPreferencesAnyApplication);
    }
}

// ---- CoreFoundation / LaunchServices ------------------------------------

type CFRef = *const c_void;

/// An owned CFString.
struct CfString(CFRef);

impl CfString {
    fn new(s: &str) -> CfString {
        CfString(unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, UTF8, 0) })
    }

    /// Reads and releases a CFString we own (`None` for null).
    unsafe fn take(cf: CFRef) -> Option<String> {
        if cf.is_null() {
            return None;
        }
        let mut buf = [0 as c_char; 512];
        let ok = unsafe { CFStringGetCString(cf, buf.as_mut_ptr(), buf.len() as isize, UTF8) } != 0;
        unsafe { CFRelease(cf) };
        ok.then(|| unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
    }
}

impl Drop for CfString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

const UTF8: u32 = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFPreferencesAnyApplication: CFRef;
    fn CFStringCreateWithBytes(alloc: CFRef, bytes: *const u8, len: isize, encoding: u32, external: u8) -> CFRef;
    fn CFStringGetCString(s: CFRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFStringGetTypeID() -> usize;
    fn CFGetTypeID(cf: CFRef) -> usize;
    fn CFRelease(cf: CFRef);
    fn CFPreferencesCopyAppValue(key: CFRef, app: CFRef) -> CFRef;
    fn CFPreferencesSetAppValue(key: CFRef, value: CFRef, app: CFRef);
    fn CFPreferencesAppSynchronize(app: CFRef) -> u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "changes a system setting (puts back what was there)"]
    fn switches_to_findest_and_back_to_finder() {
        let before = file_viewer();
        use_findest().unwrap();
        assert_eq!(file_viewer().as_deref(), Some(BUNDLE_ID));
        assert!(is_default());

        use_finder().unwrap();
        assert_eq!(file_viewer(), None);
        assert!(!is_default());

        set_file_viewer(before.as_deref());
    }

    #[test]
    fn cf_strings_round_trip() {
        let s = CfString::new("dev.findest.Findest ✓");
        let copy = unsafe { CFStringCreateWithBytes(std::ptr::null(), "x".as_ptr(), 1, UTF8, 0) };
        assert_eq!(unsafe { CfString::take(copy) }.as_deref(), Some("x"));
        drop(s);
    }
}
}

#[cfg(target_os = "macos")]
pub use mac::*;

#[cfg(not(target_os = "macos"))]
mod imp {
    use anyhow::{Result, bail};
    use std::path::Path;

    pub fn is_default() -> bool {
        false
    }

    pub fn use_findest() -> Result<()> {
        bail!("Setting default file viewer is only supported on macOS");
    }

    pub fn use_finder() -> Result<()> {
        bail!("Setting default file viewer is only supported on macOS");
    }

    pub fn reveal_in_finder(path: &Path) -> Result<()> {
        findest_core::launch::reveal_in_finder(path)
    }
}

#[cfg(not(target_os = "macos"))]
pub use imp::*;

#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn default_viewer_stubs() {
        assert!(!is_default());
        assert!(use_findest().is_err());
        assert!(use_finder().is_err());
    }
}
