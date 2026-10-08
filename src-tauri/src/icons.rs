use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject,
    GetDC, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, RGBQUAD,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHGetImageList, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SYSICONINDEX,
    SHIL_EXTRALARGE, SHFILEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, DrawIconEx, HICON, DI_NORMAL,
};
use std::io::Cursor;
use base64::{Engine as _, engine::general_purpose};
use image::{RgbaImage, ImageFormat, imageops::FilterType};

/// Native icon size we extract. SHIL_EXTRALARGE is a true 48x48 from the
/// system image list - the old code asked for SHGFI_LARGEICON (32x32) and then
/// read it into a 48x48 buffer, which produced tiny/garbled shortcut icons.
const ICON_PX: i32 = 48;
/// Longest side of the glyph after margin trimming - fills the tile while
/// leaving a hair of breathing room so rounded art never clips.
const GLYPH_PX: u32 = 44;

/// Crops transparent margins so the glyph fills the canvas. Many shell icons
/// carry baked-in padding (or center smaller art on the tile) - without this
/// the art floats small inside its square no matter how large it is drawn.
/// The content is fitted to GLYPH_PX on its longest side and centered on a
/// transparent 48x48 tile.
fn fill_canvas(img: RgbaImage) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    let (mut min_x, mut min_y) = (w, h);
    let (mut max_x, mut max_y) = (0u32, 0u32);
    for (x, y, px) in img.enumerate_pixels() {
        if px[3] > 8 {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if max_x < min_x || max_y < min_y {
        return img; // fully transparent - nothing to fill
    }
    // Small breathing room, clamped to the canvas.
    let min_x = min_x.saturating_sub(2);
    let min_y = min_y.saturating_sub(2);
    let max_x = (max_x + 2).min(w - 1);
    let max_y = (max_y + 2).min(h - 1);
    let (cw, ch) = (max_x - min_x + 1, max_y - min_y + 1);
    if cw >= GLYPH_PX && ch >= GLYPH_PX {
        return img; // already fills the canvas
    }
    let cropped = image::imageops::crop_imm(&img, min_x, min_y, cw, ch).to_image();
    let scale = GLYPH_PX as f32 / cw.max(ch) as f32;
    let (nw, nh) = (
        ((cw as f32 * scale).round() as u32).max(1),
        ((ch as f32 * scale).round() as u32).max(1),
    );
    let resized = image::imageops::resize(&cropped, nw, nh, FilterType::Lanczos3);
    let mut canvas = RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 0]));
    image::imageops::overlay(
        &mut canvas,
        &resized,
        ((w - nw) / 2) as i64,
        ((h - nh) / 2) as i64,
    );
    canvas
}

fn com_guard() -> bool {
    // The image-list APIs want COM on the calling thread; the indexer runs on
    // a plain background thread. S_FALSE just means "already initialized".
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok() }
}

/// Renders an HICON into a 48x48 RGBA PNG (base64). DrawIconEx composites the
/// color + mask with real per-pixel alpha - unlike reading hbmColor directly,
/// which loses alpha and clips at the source bitmap size.
fn hicon_to_base64(hicon: HICON) -> Option<String> {
    unsafe {
        let hdc_screen = GetDC(HWND::default());
        if hdc_screen.0.is_null() {
            return None;
        }
        let hdc_mem = CreateCompatibleDC(hdc_screen);
        if hdc_mem.0.is_null() {
            let _ = ReleaseDC(HWND::default(), hdc_screen);
            return None;
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: ICON_PX,
                biHeight: -ICON_PX, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default(); 1],
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(hdc_screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let old = SelectObject(hdc_mem, hbmp);
        let drawn = DrawIconEx(
            hdc_mem,
            0,
            0,
            hicon,
            ICON_PX,
            ICON_PX,
            0,
            None,
            DI_NORMAL,
        )
        .is_ok();

        // Copy the pixels BEFORE releasing any GDI object - `bits` points
        // into the DIB section and dies with DeleteObject(hbmp).
        let mut buffer = vec![0u8; (ICON_PX * ICON_PX * 4) as usize];
        if drawn && !bits.is_null() {
            std::ptr::copy_nonoverlapping(
                bits as *const u8,
                buffer.as_mut_ptr(),
                buffer.len(),
            );
        }

        let _ = SelectObject(hdc_mem, old);
        let _ = DeleteObject(hbmp);
        let _ = DeleteDC(hdc_mem);
        let _ = ReleaseDC(HWND::default(), hdc_screen);

        if !drawn || bits.is_null() {
            return None;
        }

        // GDI hands us BGRA - PNG wants RGBA.
        for pixel in buffer.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }

        let img = fill_canvas(
            RgbaImage::from_raw(ICON_PX as u32, ICON_PX as u32, buffer)?,
        );
        let mut png_data = Vec::new();
        img.write_to(&mut Cursor::new(&mut png_data), ImageFormat::Png)
            .ok()?;
        Some(general_purpose::STANDARD.encode(png_data))
    }
}

/// System image-list icon index for a path. Works for .exe and .lnk
/// (the shell resolves shortcuts to their target's icon).
fn sysicon_index(path: &str) -> Option<i32> {
    unsafe {
        let mut shfi = SHFILEINFOW::default();
        let wide_path: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let result = SHGetFileInfoW(
            PCWSTR(wide_path.as_ptr()),
            Default::default(),
            Some(&mut shfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX,
        );
        if result == 0 {
            return None;
        }
        Some(shfi.iIcon)
    }
}

fn extralarge_icon(index: i32) -> Option<HICON> {
    unsafe {
        // Generic form: asks shell32 for the EXTRALARGE (48x48) image list
        // as a refcounted IImageList - no manual AddRef/Release needed.
        let list: IImageList = SHGetImageList(SHIL_EXTRALARGE as i32).ok()?;
        let hicon = list.GetIcon(index, ILD_TRANSPARENT.0 as u32).ok()?;
        if hicon.is_invalid() {
            return None;
        }
        Some(hicon)
    }
}

/// Legacy fallback: 32x32 icon upscaled into the 48x48 canvas. Only used when
/// the image-list path fails.
fn fallback_icon(path: &str) -> Option<HICON> {
    unsafe {
        let mut shfi = SHFILEINFOW::default();
        let wide_path: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let result = SHGetFileInfoW(
            PCWSTR(wide_path.as_ptr()),
            Default::default(),
            Some(&mut shfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if result == 0 || shfi.hIcon.is_invalid() {
            return None;
        }
        Some(shfi.hIcon)
    }
}

pub fn extract_icon_as_base64(path: &str) -> Option<String> {
    let com_init = com_guard();

    let result = (|| {
        // Preferred: true 48x48 from the system image list.
        if let Some(index) = sysicon_index(path) {
            if let Some(hicon) = extralarge_icon(index) {
                let out = hicon_to_base64(hicon);
                unsafe {
                    let _ = DestroyIcon(hicon);
                }
                if out.is_some() {
                    return out;
                }
            }
        }
        // Fallback: 32x32 upscaled.
        if let Some(hicon) = fallback_icon(path) {
            let out = hicon_to_base64(hicon);
            unsafe {
                let _ = DestroyIcon(hicon);
            }
            return out;
        }
        None
    })();

    if com_init {
        unsafe {
            CoUninitialize();
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the "tiny/garbled shortcut icon" regression class: a real
    /// system binary must yield a true 48x48 PNG with meaningful alpha
    /// coverage in its central region (not a 16x16 sprite in a corner).
    #[test]
    fn test_extract_icon_is_true_size_with_alpha() {
        let base64 = extract_icon_as_base64(r"C:\Windows\System32\notepad.exe");
        assert!(base64.is_some(), "expected an icon for notepad.exe");
        let png = general_purpose::STANDARD
            .decode(base64.unwrap())
            .expect("icon must be valid base64");
        assert_eq!(&png[1..4], b"PNG", "icon must be a PNG");
        let img = image::load_from_memory(&png).expect("PNG must decode");
        assert_eq!((img.width(), img.height()), (48, 48));
        let rgba = img.to_rgba8();
        // Central half must be substantially covered (alpha > 128).
        let (w, h) = (rgba.width(), rgba.height());
        let mut covered = 0u32;
        let mut total = 0u32;
        for y in h / 4..h * 3 / 4 {
            for x in w / 4..w * 3 / 4 {
                total += 1;
                if rgba.get_pixel(x, y)[3] > 128 {
                    covered += 1;
                }
            }
        }
        let ratio = covered as f32 / total as f32;
        assert!(
            ratio > 0.10,
            "icon looks empty (central alpha coverage {ratio:.2})"
        );
    }

    /// A small glyph centered on a large transparent canvas must come out
    /// filling the tile - this is what makes shortcut icons look full-size
    /// instead of floating small inside their square.
    #[test]
    fn test_fill_canvas_expands_small_glyph() {
        let mut img = RgbaImage::from_pixel(48, 48, image::Rgba([0, 0, 0, 0]));
        for y in 18..30 {
            for x in 18..30 {
                img.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            }
        }
        let filled = fill_canvas(img);
        assert_eq!((filled.width(), filled.height()), (48, 48));
        let (mut min_x, mut max_x) = (48u32, 0u32);
        let (mut min_y, mut max_y) = (48u32, 0u32);
        for (x, y, px) in filled.enumerate_pixels() {
            if px[3] > 8 {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
        assert!(
            max_x - min_x >= 40 && max_y - min_y >= 40,
            "glyph should fill the canvas"
        );
    }
}
