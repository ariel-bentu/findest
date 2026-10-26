//! PDF pages for the viewer, drawn by macOS (Core Graphics) or Poppler
//! (`pdfinfo` / `pdftoppm`) on Linux. Safe to call from a background thread:
//! each call opens the document itself.

use std::path::Path;
use std::sync::{Arc, Mutex};

use gpui::{App, RenderImage, Window};

/// Pages get at most this many pixels on their longer side.
const MAX_PIXELS: f64 = 4096.0;

pub fn is_pdf(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

/// Page images no longer shown, to free from the GPU on the next frame.
static RETIRED: Mutex<Vec<Arc<RenderImage>>> = Mutex::new(Vec::new());

pub fn retire(image: Arc<RenderImage>) {
    RETIRED.lock().unwrap_or_else(|e| e.into_inner()).push(image);
}

/// Frees the retired page images (called each frame).
pub fn drop_retired(window: &mut Window, cx: &mut App) {
    let retired = std::mem::take(&mut *RETIRED.lock().unwrap_or_else(|e| e.into_inner()));
    for image in retired {
        cx.drop_image(image, Some(&mut *window));
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use objc2_core_foundation::{CFURL, CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{
        CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGImageByteOrderInfo, CGPDFBox, CGPDFDocument, CGPDFPage,
    };

    fn open(path: &Path) -> Result<objc2_core_foundation::CFRetained<CGPDFDocument>, String> {
        let url = CFURL::from_file_path(path).ok_or("unusual path")?;
        let doc = CGPDFDocument::with_url(Some(&url)).ok_or("not a readable PDF")?;
        if !CGPDFDocument::is_unlocked(Some(&doc)) {
            return Err("the PDF is password-protected".into());
        }
        Ok(doc)
    }

    /// The page's size in points as shown, i.e. its crop box turned by its rotation.
    fn page_size(page: &CGPDFPage) -> (f64, f64) {
        let rect = CGPDFPage::box_rect(Some(page), CGPDFBox::CropBox);
        let (w, h) = (rect.size.width.abs(), rect.size.height.abs());
        if CGPDFPage::rotation_angle(Some(page)).rem_euclid(180) == 90 { (h, w) } else { (w, h) }
    }

    /// Each page's size in points (width, height).
    pub fn page_sizes(path: &Path) -> Result<Vec<(f32, f32)>, String> {
        let doc = open(path)?;
        let count = CGPDFDocument::number_of_pages(Some(&doc));
        if count == 0 {
            return Err("the PDF has no pages".into());
        }
        // Pages are numbered from 1.
        Ok((1..=count)
            .map(|n| match CGPDFDocument::page(Some(&doc), n) {
                Some(page) => {
                    let (w, h) = page_size(&page);
                    (w.max(1.0) as f32, h.max(1.0) as f32)
                }
                None => (612.0, 792.0),
            })
            .collect())
    }

    /// Page `index` (from 0) drawn `width` pixels wide on white, as gpui's BGRA image.
    pub fn render_page(path: &Path, index: usize, width: u32) -> Option<RenderImage> {
        let doc = open(path).ok()?;
        let page = CGPDFDocument::page(Some(&doc), index + 1)?;
        let (pw, ph) = page_size(&page);
        let scale = (width as f64 / pw).min(MAX_PIXELS / pw.max(ph));
        let (w, h) = ((pw * scale).round().max(1.0) as usize, (ph * scale).round().max(1.0) as usize);
        let mut pixels = vec![0u8; w * h * 4];
        let space = CGColorSpace::new_device_rgb()?;
        // BGRA, premultiplied: what gpui takes.
        let info = CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0;
        // SAFETY: `pixels` holds `h` rows of `w * 4` bytes and outlives the context.
        let bitmap = unsafe { CGBitmapContextCreate(pixels.as_mut_ptr().cast(), w, h, 8, w * 4, Some(&space), info) }?;
        let ctx = Some(&*bitmap);
        CGContext::set_rgb_fill_color(ctx, 1.0, 1.0, 1.0, 1.0);
        CGContext::fill_rect(ctx, CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64)));
        CGContext::scale_ctm(ctx, scale, scale);
        // At 1:1 this only moves and turns the crop box into place (it never scales up).
        let place = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(pw, ph));
        CGContext::concat_ctm(ctx, CGPDFPage::drawing_transform(Some(&page), CGPDFBox::CropBox, place, 0, true));
        CGContext::draw_pdf_page(ctx, Some(&page));
        drop(bitmap);
        let buffer = image::RgbaImage::from_raw(w as u32, h as u32, pixels)?;
        Some(RenderImage::new([image::Frame::new(buffer)]))
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;
    use std::process::{Command, Stdio};

    /// Each page's size in points (width, height), read via `pdfinfo`.
    pub fn page_sizes(path: &Path) -> Result<Vec<(f32, f32)>, String> {
        if !path.is_file() {
            return Err("not a readable PDF".into());
        }
        let output = Command::new("pdfinfo")
            .args(["-f", "1", "-l", "9999"])
            .arg(path)
            .stdin(Stdio::null())
            .output()
            .map_err(|_| "pdfinfo (poppler-utils) is not installed".to_string())?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.to_ascii_lowercase().contains("incorrect password") {
                return Err("the PDF is password-protected".into());
            }
            return Err("not a readable PDF".into());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut count = 0usize;
        let mut raw_sizes: std::collections::BTreeMap<usize, (f32, f32)> = std::collections::BTreeMap::new();
        let mut rots: std::collections::BTreeMap<usize, i32> = std::collections::BTreeMap::new();

        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Pages:") {
                count = rest.trim().parse().unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("Page") {
                let rest = rest.trim_start();
                let Some((num_str, tail)) = rest.split_once(char::is_whitespace) else { continue };
                let Ok(page_num) = num_str.parse::<usize>() else { continue };
                let tail = tail.trim_start();
                if let Some(size_str) = tail.strip_prefix("size:") {
                    // e.g. "612 x 792 pts (letter)"
                    let parts: Vec<&str> = size_str.split_whitespace().collect();
                    if parts.len() >= 3
                        && parts[1] == "x"
                        && let (Ok(w), Ok(h)) = (parts[0].parse::<f32>(), parts[2].parse::<f32>())
                    {
                        raw_sizes.insert(page_num, (w.max(1.0), h.max(1.0)));
                    }
                } else if let Some(rot_str) = tail.strip_prefix("rot:")
                    && let Ok(rot) = rot_str.trim().parse::<i32>()
                {
                    rots.insert(page_num, rot);
                }
            }
        }
        if count == 0 {
            return Err("the PDF has no pages".into());
        }
        Ok((1..=count)
            .map(|n| {
                let (w, h) = raw_sizes.get(&n).copied().unwrap_or((612.0, 792.0));
                let rot = rots.get(&n).copied().unwrap_or(0);
                if rot.rem_euclid(180) == 90 { (h, w) } else { (w, h) }
            })
            .collect())
    }

    /// Page `index` (from 0) drawn `width` pixels wide on white via `pdftoppm`, as gpui's BGRA image.
    pub fn render_page(path: &Path, index: usize, width: u32) -> Option<RenderImage> {
        let sizes = page_sizes(path).ok()?;
        let &(pw, ph) = sizes.get(index)?;
        let (pw, ph) = (pw as f64, ph as f64);
        let scale = (width as f64 / pw).min(MAX_PIXELS / pw.max(ph));
        let (w, h) = ((pw * scale).round().max(1.0) as u32, (ph * scale).round().max(1.0) as u32);
        let longer = w.max(h);
        let page_num = (index + 1).to_string();
        let scale_str = longer.to_string();

        let output = Command::new("pdftoppm")
            .args(["-png", "-cropbox", "-f", &page_num, "-l", &page_num, "-scale-to", &scale_str])
            .arg(path)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() || output.stdout.is_empty() {
            return None;
        }
        let decoded = image::load_from_memory_with_format(&output.stdout, image::ImageFormat::Png).ok()?;
        let mut rgba = if decoded.width() != w || decoded.height() != h {
            decoded.resize_exact(w, h, image::imageops::FilterType::Triangle).into_rgba8()
        } else {
            decoded.into_rgba8()
        };
        // gpui's RenderImage takes premultiplied BGRA.
        for pixel in rgba.pixels_mut() {
            pixel.0.swap(0, 2);
        }
        Some(RenderImage::new([image::Frame::new(rgba)]))
    }
}

pub use imp::{page_sizes, render_page};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_pages() {
        // A one-page US Letter PDF, as the minimal file the spec allows.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.PDF");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Rotate 90 >>",
        ];
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (n, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf += &format!("{} 0 obj\n{body}\nendobj\n", n + 1);
        }
        let xref = pdf.len();
        pdf += &format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
        for offset in offsets {
            pdf += &format!("{offset:010} 00000 n \n");
        }
        pdf += &format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1);
        std::fs::write(&path, pdf).unwrap();

        assert!(is_pdf(&path));
        assert_eq!(page_sizes(&path).unwrap(), [(792.0, 612.0)], "rotated a quarter turn");
        let image = render_page(&path, 0, 396).unwrap();
        let size = image.size(0);
        assert_eq!((size.width.0, size.height.0), (396, 306));
        assert!(image.as_bytes(0).unwrap().iter().all(|&b| b == 255), "a blank page on white");
        assert!(render_page(&path, 1, 100).is_none());
        assert!(page_sizes(&tmp.path().join("missing.pdf")).is_err());
    }
}
