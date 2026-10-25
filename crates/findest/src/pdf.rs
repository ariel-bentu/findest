//! PDF pages for the viewer, drawn by macOS (Core Graphics, the renderer
//! Preview uses). Safe to call from a background thread: each call opens
//! the document itself.

use std::path::Path;
use std::sync::{Arc, Mutex};

use gpui::{App, RenderImage, Window};
use objc2_core_foundation::{CFURL, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGImageByteOrderInfo, CGPDFBox, CGPDFDocument, CGPDFPage,
};

/// Pages get at most this many pixels on their longer side.
const MAX_PIXELS: f64 = 4096.0;

pub fn is_pdf(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

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
