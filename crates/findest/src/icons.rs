//! File icons. macOS renders Finder/AppKit icons; other platforms fall back to
//! the Unicode icons in `render.rs`.
//!
//! The cache is shared by every icon source so a folder of 10k `.txt` files
//! costs one lookup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use findest_core::listing::{Entry, TagColor};
use gpui::{Image, ImageFormat};

#[cfg(target_os = "macos")]
use objc2::{AllocAnyThread, Message};
#[cfg(target_os = "macos")]
use objc2::rc::autoreleasepool;
#[cfg(target_os = "macos")]
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSFontWeightMedium, NSImage, NSImageSymbolConfiguration,
    NSWorkspace,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};
#[cfg(target_os = "macos")]
use objc2_uniform_type_identifiers::{UTType, UTTypeData, UTTypeFolder};

/// Rendered icon edge in pixels: sharp on Retina both in rows (~18pt) and
/// in the info panel (64pt).
const ICON_PIXELS: f64 = 128.0;
/// Edge for [`IconKey::Large`]: big tiles (up to 256pt).
const LARGE_ICON_PIXELS: f64 = 512.0;

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
    /// Another icon rendered at a higher resolution, for big tiles.
    Large(Box<IconKey>),
    /// Another icon recolored for a folder's color label.
    Tinted(Box<IconKey>, TagColor),
    /// An image file's own contents, longer side at most `pixels`. `modified`
    /// is part of the key so an edited image gets a fresh thumbnail.
    Thumbnail { path: PathBuf, modified: Option<SystemTime>, pixels: u32 },
}

impl IconKey {
    pub fn for_entry(entry: &Entry, home: &Path) -> IconKey {
        if entry.is_package {
            return IconKey::Path(entry.path.clone());
        }
        if entry.is_dir {
            // Top-level folders like ~/Desktop or /Applications have custom icons.
            let parent = entry.path.parent();
            let icon = if parent == Some(home) || parent == Some(Path::new("/")) {
                IconKey::Path(entry.path.clone())
            } else {
                IconKey::Folder
            };
            return match entry.color {
                Some(color) => IconKey::Tinted(Box::new(icon), color),
                None => icon,
            };
        }
        match entry.extension() {
            Some(ext) => IconKey::Extension(ext.to_ascii_lowercase()),
            None => IconKey::File,
        }
    }

    /// Renders the icon as PNG bytes. Safe to call from a background thread.
    fn load(&self) -> Option<Vec<u8>> {
        self.load_at(ICON_PIXELS)
    }

    fn load_at(&self, pixels: f64) -> Option<Vec<u8>> {
        match self {
            #[cfg(target_os = "macos")]
            IconKey::Symbol { name, rgb } => return render_symbol(name, *rgb),
            #[cfg(not(target_os = "macos"))]
            IconKey::Symbol { .. } => return None,
            IconKey::Large(inner) => return inner.load_at(LARGE_ICON_PIXELS),
            IconKey::Tinted(inner, color) => return tint_png(&inner.load_at(pixels)?, color.rgb()),
            IconKey::Thumbnail { path, pixels, .. } => return crate::system::image_preview_png(path, *pixels as f64),
            _ => {}
        }
        #[cfg(target_os = "macos")]
        {
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
                    IconKey::Symbol { .. } | IconKey::Large(_) | IconKey::Tinted(..) | IconKey::Thumbnail { .. } => {
                        unreachable!("handled above")
                    }
                };
                let size = NSSize::new(pixels, pixels);
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
        #[cfg(not(target_os = "macos"))]
        {
            let _ = pixels;
            None
        }
    }
}

/// Recolors an icon (a PNG) with `rgb`, keeping its shading.
fn tint_png(png: &[u8], rgb: [u8; 3]) -> Option<Vec<u8>> {
    let mut image = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?.to_rgba8();
    for pixel in image.pixels_mut() {
        pixel.0 = tint_pixel(pixel.0, rgb);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// Brightness of the standard folder icon's body: pixels this bright get
/// exactly `rgb`, brighter ones (highlights) are lighter, darker ones darker.
const FOLDER_LUMA: f32 = 0.64;

fn tint_pixel([r, g, b, a]: [u8; 4], rgb: [u8; 3]) -> [u8; 4] {
    if a == 0 {
        return [r, g, b, a];
    }
    let luma = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0;
    let k = luma / FOLDER_LUMA;
    let shade = |c: u8| {
        let c = c as f32 / 255.0;
        let v = if k <= 1.0 { c * k } else { c + (1.0 - c) * (k - 1.0).min(1.0) };
        (v.clamp(0.0, 1.0) * 255.0).round() as u8
    };
    [shade(rgb[0]), shade(rgb[1]), shade(rgb[2]), a]
}

/// Renders an SF Symbol at Retina resolution, keeping its aspect ratio.
#[cfg(target_os = "macos")]
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
        self.icons.retain(|key, _| {
            let key = if let IconKey::Large(inner) = key { inner } else { key };
            !matches!(key, IconKey::Path(_) | IconKey::Thumbnail { .. })
        });
    }

    /// Thumbnails are per folder; drop them when leaving it to bound memory.
    pub fn clear_thumbnails(&mut self) {
        self.icons.retain(|key, _| !matches!(key, IconKey::Thumbnail { .. }));
    }
}

pub fn load_icon(key: &IconKey) -> Option<Vec<u8>> {
    key.load()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    fn is_png(bytes: &[u8]) -> bool {
        bytes.starts_with(b"\x89PNG")
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tinting_keeps_shading_and_transparency() {
        let red = TagColor::Red.rgb();
        assert_eq!(tint_pixel([1, 2, 3, 0], red), [1, 2, 3, 0], "transparent pixels untouched");
        // The folder body's own brightness maps to exactly the tag color.
        let body = [89, 189, 235, 255];
        let tinted = tint_pixel(body, red);
        assert!(tinted[..3].iter().zip(red).all(|(a, b)| a.abs_diff(b) <= 3), "{tinted:?}");
        let dark = tint_pixel([20, 40, 60, 200], red);
        let light = tint_pixel([230, 240, 250, 255], red);
        assert!(dark[0] < tinted[0] && dark[3] == 200, "darker stays darker, alpha kept");
        assert!(light[1] > tinted[1] && light[2] > tinted[2], "highlights get lighter");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tinted_folder_icons_render() {
        let folder = IconKey::Folder.load().unwrap();
        let tinted = IconKey::Tinted(Box::new(IconKey::Folder), TagColor::Green).load().unwrap();
        assert!(is_png(&tinted));
        let decode = |png: &[u8]| image::load_from_memory(png).unwrap().to_rgba8();
        let (plain, green) = (decode(&folder), decode(&tinted));
        assert_eq!(plain.dimensions(), green.dimensions());
        // Sum colors over opaque pixels: green dominates after tinting.
        let sums = |img: &image::RgbaImage| {
            img.pixels().filter(|p| p.0[3] > 128).fold([0u64; 3], |s, p| [s[0] + p.0[0] as u64, s[1] + p.0[1] as u64, s[2] + p.0[2] as u64])
        };
        let (p, g) = (sums(&plain), sums(&green));
        assert!(p[2] > p[1], "the stock folder is blue");
        assert!(g[1] > g[0] && g[1] > g[2], "the tinted one is green: {g:?}");
    }

    #[test]
    fn colored_folders_get_tinted_icons() {
        let home = Path::new("/Users/someone");
        let mut entry = Entry {
            name: "Projects".into(),
            path: home.join("code/Projects"),
            is_dir: true,
            is_package: false,
            is_symlink: false,
            is_hidden: false,
            size: Some(0),
            modified: None,
            color: None,
            label: None,
            mime_type: None,
            attributes: Vec::new(),
        };
        assert_eq!(IconKey::for_entry(&entry, home), IconKey::Folder);
        entry.color = Some(TagColor::Purple);
        assert_eq!(IconKey::for_entry(&entry, home), IconKey::Tinted(Box::new(IconKey::Folder), TagColor::Purple));
        entry.path = home.join("Documents"); // special icon, tinted too
        assert_eq!(IconKey::for_entry(&entry, home), IconKey::Tinted(Box::new(IconKey::Path(entry.path.clone())), TagColor::Purple));
    }

    #[test]
    fn unknown_symbol_is_none() {
        assert!(load_icon(&IconKey::Symbol { name: "no.such.symbol.xyz", rgb: 0 }).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn renders_icons_off_main_thread() {
        let keys = [
            IconKey::Folder,
            IconKey::File,
            IconKey::Extension("pdf".into()),
            IconKey::Extension("no-such-ext-xyz".into()),
            IconKey::Path("/System/Applications/Calculator.app".into()),
            IconKey::Symbol { name: "arrow.down.circle", rgb: 0x0a64ff },
            IconKey::Large(Box::new(IconKey::Folder)),
            IconKey::Thumbnail {
                path: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/AppIcon.png"),
                modified: None,
                pixels: 256,
            },
        ];
        let handle = std::thread::spawn(move || keys.iter().map(|k| load_icon(k).map(|b| is_png(&b))).collect::<Vec<_>>());
        assert!(handle.join().unwrap().iter().all(|r| *r == Some(true)));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn uses_text_fallbacks_without_native_icons() {
        assert!(load_icon(&IconKey::Folder).is_none());
        assert!(load_icon(&IconKey::Symbol { name: "folder", rgb: 0 }).is_none());
    }
}
