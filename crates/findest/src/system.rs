//! Volume information from Foundation.

use std::path::Path;

use objc2::rc::autoreleasepool;
use objc2_foundation::{
    NSArray, NSNumber, NSString, NSURL, NSURLResourceKey, NSURLVolumeAvailableCapacityForImportantUsageKey,
    NSURLVolumeAvailableCapacityKey, NSURLVolumeTotalCapacityKey,
};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_image_dimensions() {
        let icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/AppIcon.png");
        assert_eq!(image_dimensions(&icon), Some((1024, 1024)));
        assert_eq!(image_dimensions(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml").as_path()), None);
    }

    #[test]
    fn boot_volume_has_capacity() {
        let (available, total) = volume_capacity(Path::new("/")).unwrap();
        assert!(total > 0 && available > 0 && available <= total);
        assert!(volume_capacity(Path::new("/no/such/place")).is_none_or(|(a, t)| a <= t));
    }
}
