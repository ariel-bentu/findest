//! Native Finder icons via `NSWorkspace`, rendered to PNG off the UI thread
//! and cached by [`IconKey`] so a folder of 10k `.txt` files costs one lookup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use findest_core::listing::Entry;
use gpui::{Image, ImageFormat};
use objc2::{AllocAnyThread, Message};
use objc2::rc::autoreleasepool;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSFontWeightMedium, NSImage, NSImageSymbolConfiguration,
    NSWorkspace,
};
use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};
use objc2_uniform_type_identifiers::{UTType, UTTypeData, UTTypeFolder};

/// Rendered icon edge in pixels; displayed at ~18pt, so this stays sharp on Retina.
const ICON_PIXELS: f64 = 64.0;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum IconKey {
    /// Per-item icon: apps/bundles and folders with special icons.
    Path(PathBuf),
    /// Icon for a file extension (lowercased).
    Extension(String),
    Folder,
    File,
    /// An SF Symbol (as in Finder's sidebar) tinted with an sRGB `0xRRGGBB` color.
    Symbol { name: &'static str, rgb: u32 },
}

impl IconKey {
    pub fn for_entry(entry: &Entry, home: &Path) -> IconKey {
        if entry.is_package {
            return IconKey::Path(entry.path.clone());
        }
        if entry.is_dir {
            // Top-level folders like ~/Desktop or /Applications have custom icons.
            let parent = entry.path.parent();
            return if parent == Some(home) || parent == Some(Path::new("/")) {
                IconKey::Path(entry.path.clone())
            } else {
                IconKey::Folder
            };
        }
        match entry.extension() {
            Some(ext) => IconKey::Extension(ext.to_ascii_lowercase()),
            None => IconKey::File,
        }
    }

    /// Renders the icon as PNG bytes. Safe to call from a background thread.
    fn load(&self) -> Option<Vec<u8>> {
        if let IconKey::Symbol { name, rgb } = self {
            return render_symbol(name, *rgb);
        }
        autoreleasepool(|_| {
            let workspace = NSWorkspace::sharedWorkspace();
            let image = match self {
                IconKey::Path(path) => workspace.iconForFile(&NSString::from_str(path.to_str()?)),
                IconKey::Extension(ext) => {
                    let ty = UTType::typeWithFilenameExtension(&NSString::from_str(ext));
                    // SAFETY: UTTypeData is an immutable framework constant.
                    let ty = ty.unwrap_or_else(|| unsafe { UTTypeData }.retain());
                    workspace.iconForContentType(&ty)
                }
                // SAFETY: immutable framework constants.
                IconKey::Folder => workspace.iconForContentType(unsafe { UTTypeFolder }),
                IconKey::File => workspace.iconForContentType(unsafe { UTTypeData }),
                IconKey::Symbol { .. } => unreachable!("handled above"),
            };
            let size = NSSize::new(ICON_PIXELS, ICON_PIXELS);
            image.setSize(size);
            let mut rect = NSRect::new(NSPoint::new(0.0, 0.0), size);
            // SAFETY: `rect` is a valid pointer for the duration of the call.
            let cg_image = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
            let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &cg_image);
            // SAFETY: an empty properties dictionary is valid.
            let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
            Some(png.to_vec())
        })
    }
}

/// Renders an SF Symbol at Retina resolution, keeping its aspect ratio.
fn render_symbol(name: &str, rgb: u32) -> Option<Vec<u8>> {
    autoreleasepool(|_| {
        let c = |shift: u32| ((rgb >> shift) & 0xff) as f64 / 255.0;
        let color = NSColor::colorWithSRGBRed_green_blue_alpha(c(16), c(8), c(0), 1.0);
        // SAFETY: NSFontWeightMedium is an immutable framework constant.
        let weight = unsafe { NSFontWeightMedium };
        let config = NSImageSymbolConfiguration::configurationWithPointSize_weight(32.0, weight)
            .configurationByApplyingConfiguration(&NSImageSymbolConfiguration::configurationWithHierarchicalColor(&color));
        let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)?
            .imageWithSymbolConfiguration(&config)?;
        let size = image.size();
        let mut rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size.width * 2.0, size.height * 2.0));
        // SAFETY: `rect` is a valid pointer for the duration of the call.
        let cg_image = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
        let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &cg_image);
        // SAFETY: an empty properties dictionary is valid.
        let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
        Some(png.to_vec())
    })
}

/// Icon cache. `None` means "requested, not ready (or failed)".
#[derive(Default)]
pub struct IconCache {
    icons: HashMap<IconKey, Option<Arc<Image>>>,
}

impl IconCache {
    /// Returns the cached icon, or `None` plus `true` if the caller should start loading it.
    pub fn get(&mut self, key: &IconKey) -> (Option<Arc<Image>>, bool) {
        match self.icons.get(key) {
            Some(icon) => (icon.clone(), false),
            None => {
                self.icons.insert(key.clone(), None);
                (None, true)
            }
        }
    }

    /// The cached icon without requesting a load.
    pub fn peek(&self, key: &IconKey) -> Option<Arc<Image>> {
        self.icons.get(key).cloned().flatten()
    }

    pub fn insert(&mut self, key: IconKey, png: Option<Vec<u8>>) {
        let image = png.map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)));
        self.icons.insert(key, image);
    }

    /// Per-path icons can change (e.g. an app was updated); drop them on refresh.
    pub fn clear_paths(&mut self) {
        self.icons.retain(|key, _| !matches!(key, IconKey::Path(_)));
    }
}

pub fn load_icon(key: &IconKey) -> Option<Vec<u8>> {
    key.load()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_png(bytes: &[u8]) -> bool {
        bytes.starts_with(b"\x89PNG")
    }

    #[test]
    fn unknown_symbol_is_none() {
        assert!(load_icon(&IconKey::Symbol { name: "no.such.symbol.xyz", rgb: 0 }).is_none());
    }

    #[test]
    fn renders_icons_off_main_thread() {
        let keys = [
            IconKey::Folder,
            IconKey::File,
            IconKey::Extension("pdf".into()),
            IconKey::Extension("no-such-ext-xyz".into()),
            IconKey::Path("/System/Applications/Calculator.app".into()),
            IconKey::Symbol { name: "arrow.down.circle", rgb: 0x0a64ff },
        ];
        let handle = std::thread::spawn(move || keys.iter().map(|k| load_icon(k).map(|b| is_png(&b))).collect::<Vec<_>>());
        assert!(handle.join().unwrap().iter().all(|r| *r == Some(true)));
    }
}
