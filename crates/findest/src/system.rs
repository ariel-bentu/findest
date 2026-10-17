//! Volume information and color labels via Foundation.

use std::path::Path;

use objc2::rc::autoreleasepool;
use objc2_foundation::{
    NSArray, NSNumber, NSString, NSURL, NSURLResourceKey, NSURLVolumeAvailableCapacityForImportantUsageKey,
    NSURLVolumeAvailableCapacityKey, NSURLVolumeTotalCapacityKey,
};

/// Sets (or with `None`, clears) the item's color label. Goes through
/// Foundation so Finder's color tags stay in step.
pub fn set_color(path: &Path, color: Option<findest_core::listing::TagColor>) -> anyhow::Result<()> {
    autoreleasepool(|_| {
        let path_str = path.to_str().ok_or_else(|| anyhow::anyhow!("unusual path"))?;
        let url = NSURL::fileURLWithPath(&NSString::from_str(path_str));
        let label = NSNumber::new_u8(color.map_or(0, |c| c.label()));
        // SAFETY: an NSNumber for the label-number key, as documented.
        unsafe { url.setResourceValue_forKey_error(Some(&label), objc2_foundation::NSURLLabelNumberKey) }
            .map_err(|e| anyhow::anyhow!("{}", e.localizedDescription()))
    })
}

/// `(available, total)` bytes for the volume containing `path`. "Available"
/// matches Finder: it counts purgeable space macOS can reclaim on demand.
pub fn volume_capacity(path: &Path) -> Option<(u64, u64)> {
    autoreleasepool(|_| {
        let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str()?));
        // SAFETY: immutable framework constants.
        let (important, plain, total) = unsafe {
            (NSURLVolumeAvailableCapacityForImportantUsageKey, NSURLVolumeAvailableCapacityKey, NSURLVolumeTotalCapacityKey)
        };
        let keys = NSArray::from_slice(&[important, plain, total]);
        let values = url.resourceValuesForKeys_error(&keys).ok()?;
        let number = |key: &NSURLResourceKey| {
            values.objectForKey(key).and_then(|v| v.downcast::<NSNumber>().ok()).map(|n| n.unsignedLongLongValue())
        };
        let total = number(total)?;
        // Important-usage capacity is unavailable on some volumes (e.g. network shares).
        let available = number(important).filter(|&v| v > 0).or_else(|| number(plain))?;
        Some((available, total))
    })
}

/// Pixel dimensions of an image file, read via AppKit (supports HEIC, RAW, PSD...).
pub fn image_dimensions(path: &Path) -> Option<(u32, u32)> {
    autoreleasepool(|_| {
        let rep = objc2_app_kit::NSImageRep::imageRepWithContentsOfFile(&NSString::from_str(path.to_str()?))?;
        let (w, h) = (rep.pixelsWide(), rep.pixelsHigh());
        (w > 0 && h > 0).then_some((w as u32, h as u32))
    })
}

/// Renders any image AppKit understands (HEIC, TIFF, PSD, RAW, PDF's first
/// page, ...) to PNG, scaled so its longer side is at most `max_pixels`.
pub fn image_preview_png(path: &Path, max_pixels: f64) -> Option<Vec<u8>> {
    use objc2::AllocAnyThread;
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage};
    use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize};
    autoreleasepool(|_| {
        let image = NSImage::initWithContentsOfFile(NSImage::alloc(), &NSString::from_str(path.to_str()?))?;
        let size = image.size();
        if size.width <= 0.0 || size.height <= 0.0 {
            return None;
        }
        let scale = (max_pixels / size.width.max(size.height)).min(2.0);
        let mut rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size.width * scale, size.height * scale));
        // SAFETY: `rect` is a valid pointer for the duration of the call.
        let cg_image = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
        let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &cg_image);
        // SAFETY: an empty properties dictionary is valid.
        let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
        Some(png.to_vec())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use findest_core::listing::{Entry, TagColor, read_color};

    #[test]
    fn sets_reads_and_clears_folder_colors() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Projects");
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(read_color(&dir), None);
        assert_eq!(Entry::from_path(&dir).unwrap().color, None);

        for color in TagColor::ALL {
            set_color(&dir, Some(color)).unwrap();
            assert_eq!(read_color(&dir), Some(color), "{}", color.name());
        }
        set_color(&dir, Some(TagColor::Blue)).unwrap();
        assert_eq!(Entry::from_path(&dir).unwrap().color, Some(TagColor::Blue), "listing picks it up");

        set_color(&dir, None).unwrap();
        assert_eq!(read_color(&dir), None);
    }

    #[test]
    fn files_have_no_color_in_listings() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("notes.txt");
        std::fs::write(&file, "x").unwrap();
        set_color(&file, Some(TagColor::Red)).unwrap();
        assert_eq!(read_color(&file), Some(TagColor::Red), "the tag is set");
        assert_eq!(Entry::from_path(&file).unwrap().color, None, "but only folders show it");
    }

    #[test]
    fn reads_image_dimensions() {
        let icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/AppIcon.png");
        assert_eq!(image_dimensions(&icon), Some((1024, 1024)));
        assert_eq!(image_dimensions(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml").as_path()), None);
    }

    #[test]
    fn renders_previews() {
        let icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/AppIcon.icns");
        let png = image_preview_png(&icon, 256.0).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        assert!(image_preview_png(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml").as_path(), 256.0).is_none());
    }

    #[test]
    fn boot_volume_has_capacity() {
        let (available, total) = volume_capacity(Path::new("/")).unwrap();
        assert!(total > 0 && available > 0 && available <= total);
        assert!(volume_capacity(Path::new("/no/such/place")).is_none_or(|(a, t)| a <= t));
    }
}
