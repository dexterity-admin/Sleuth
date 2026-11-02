//! Second-opinion reader: the OCR engine built into Windows 10/11
//! (`Windows.Media.Ocr`). It adds nothing to the installer and runs only on
//! lines the main model was unsure about.

use image::{RgbImage, imageops};

/// Read the text inside one box of a tile. `None` when Windows OCR is not
/// available on this PC or reads nothing.
pub fn read_crop(tile: &RgbImage, x0: f32, y0: f32, x1: f32, y1: f32) -> Option<String> {
    let pad = 6.0;
    let x = (x0 - pad).max(0.0) as u32;
    let y = (y0 - pad).max(0.0) as u32;
    let w = ((x1 + pad).min(tile.width() as f32) as u32).saturating_sub(x);
    let h = ((y1 + pad).min(tile.height() as f32) as u32).saturating_sub(y);
    if w < 4 || h < 4 {
        return None;
    }
    let mut crop = imageops::crop_imm(tile, x, y, w, h).to_image();
    // Vertical text: turn it to read left to right.
    if h > w * 2 {
        crop = imageops::rotate270(&crop);
    }
    // Windows OCR reads best with text around 40-60 px high.
    let ch = crop.height();
    if ch < 40 {
        let f = 48.0 / ch as f32;
        crop = imageops::resize(&crop, (crop.width() as f32 * f) as u32, 48, imageops::FilterType::CatmullRom);
    }
    imp::recognize(&crop)
}

#[cfg(windows)]
mod imp {
    use image::RgbImage;
    use std::cell::RefCell;
    use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::DataWriter;
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};

    thread_local! {
        static ENGINE: RefCell<Option<Option<OcrEngine>>> = const { RefCell::new(None) };
    }

    fn engine() -> Option<OcrEngine> {
        ENGINE.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                unsafe {
                    let _ = RoInitialize(RO_INIT_MULTITHREADED);
                }
                *slot = Some(OcrEngine::TryCreateFromUserProfileLanguages().ok());
            }
            slot.clone().flatten()
        })
    }

    pub fn recognize(img: &RgbImage) -> Option<String> {
        let engine = engine()?;
        let max = OcrEngine::MaxImageDimension().ok()?;
        if img.width() > max || img.height() > max {
            return None;
        }
        let mut bgra = Vec::with_capacity((img.width() * img.height() * 4) as usize);
        for p in img.pixels() {
            bgra.extend_from_slice(&[p[2], p[1], p[0], 255]);
        }
        let writer = DataWriter::new().ok()?;
        writer.WriteBytes(&bgra).ok()?;
        let buffer = writer.DetachBuffer().ok()?;
        let bitmap =
            SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, img.width() as i32, img.height() as i32).ok()?;
        let result = engine.RecognizeAsync(&bitmap).ok()?.join().ok()?;
        let text = result.Text().ok()?.to_string();
        let text = text.trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn recognize(_: &image::RgbImage) -> Option<String> {
        None
    }
}
