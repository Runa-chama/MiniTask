//! Windows の実行ファイルアイコンを取得して egui のテクスチャに変換する。

use std::collections::HashMap;
use std::ffi::c_void;

use egui::{ColorImage, TextureHandle, Vec2};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, DIB_RGB_COLORS,
};
use windows_sys::Win32::UI::Shell::{
    ExtractIconExW, SHGetFileInfoW, SHGFI_ICON, SHGFI_SMALLICON, SHFILEINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

type Handle = *mut c_void;
type Rgba = (u32, u32, Vec<u8>);

unsafe fn bitmap_to_rgba(dc: Handle, bitmap: Handle, w: usize, h: usize) -> Option<Vec<u8>> { unsafe {
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            ..Default::default()
        },
        bmiColors: [Default::default()],
    };
    let mut pixels = vec![0u8; w * h * 4];
    if GetDIBits(
        dc,
        bitmap,
        0,
        h as u32,
        pixels.as_mut_ptr().cast(),
        &mut bi,
        DIB_RGB_COLORS,
    ) == 0
    {
        return None;
    }
    for px in pixels.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Some(pixels)
}}

unsafe fn icon_to_rgba(hicon: Handle, fallback_size: u32) -> Option<Rgba> { unsafe {
    let mut info: ICONINFO = Default::default();
    if GetIconInfo(hicon, &mut info) == 0 {
        return None;
    }
    let mut bmp: BITMAP = Default::default();
    let has_color = !info.hbmColor.is_null()
        && GetObjectW(
            info.hbmColor,
            std::mem::size_of::<BITMAP>() as i32,
            &mut bmp as *mut _ as *mut c_void,
        ) != 0;
    let (w, h) = if has_color && bmp.bmWidth > 0 && bmp.bmHeight > 0 {
        (bmp.bmWidth as u32, bmp.bmHeight as u32)
    } else {
        (fallback_size, fallback_size)
    };
    if w == 0 || h == 0 || w > 256 || h > 256 {
        let _ = DeleteObject(info.hbmColor);
        let _ = DeleteObject(info.hbmMask);
        return None;
    }
    let dc = CreateCompatibleDC(std::ptr::null_mut());
    let rgba = if has_color {
        bitmap_to_rgba(dc, info.hbmColor, w as usize, h as usize)
    } else {
        None
    };
    let _ = DeleteDC(dc);
    let mut pixels = rgba.unwrap_or_else(|| vec![0u8; (w * h * 4) as usize]);
    if pixels.chunks_exact(4).all(|px| px[3] == 0) && !info.hbmMask.is_null() {
        let dc2 = CreateCompatibleDC(std::ptr::null_mut());
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -((h * 2) as i32),
                biPlanes: 1,
                biBitCount: 1,
                biCompression: 0,
                ..Default::default()
            },
            bmiColors: [Default::default()],
        };
        let stride = (((w + 31) / 32) * 4) as usize;
        let mut mask = vec![0u8; stride * h as usize * 2];
        let ok = GetDIBits(
            dc2,
            info.hbmMask,
            0,
            h,
            mask.as_mut_ptr().cast(),
            &mut bi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc2);
        if ok != 0 {
            for y in 0..h as usize {
                let row = &mask[y * stride..y * stride + stride];
                for x in 0..w as usize {
                    let bit = (row[x / 8] >> (7 - (x % 8))) & 1;
                    pixels[(y * w as usize + x) * 4 + 3] = if bit == 0 { 255 } else { 0 };
                }
            }
        }
    }
    let _ = DeleteObject(info.hbmColor);
    let _ = DeleteObject(info.hbmMask);
    Some((w, h, pixels))
}}

pub fn extract_icon(path: &str, large: bool) -> Option<Rgba> {
    if path.is_empty() {
        return None;
    }
    unsafe {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sfi: SHFILEINFOW = std::mem::zeroed();
        let flags = SHGFI_ICON | if large { 0 } else { SHGFI_SMALLICON };
        let result = SHGetFileInfoW(
            wide.as_ptr(),
            0,
            &mut sfi,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        );
        if result != 0 && !sfi.hIcon.is_null() {
            let image = icon_to_rgba(sfi.hIcon, if large { 32 } else { 16 });
            let _ = DestroyIcon(sfi.hIcon);
            if image.is_some() {
                return image;
            }
        }
        let mut icon: Handle = std::ptr::null_mut();
        let n = ExtractIconExW(wide.as_ptr(), 0, &mut icon, std::ptr::null_mut(), 1);
        if n > 0 && !icon.is_null() {
            let image = icon_to_rgba(icon, 16);
            let _ = DestroyIcon(icon);
            return image;
        }
    }
    None
}

pub fn default_exe_icon() -> Option<Rgba> {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_owned());
    extract_icon(&format!("{windir}\\System32\\shell32.dll"), false)
}

pub fn exe_path_from_command(command: &str) -> String {
    let s = command.trim();
    if s.is_empty() {
        return String::new();
    }
    if s.starts_with('"') {
        if let Some(end) = s[1..].find('"') {
            return s[1..1 + end].to_owned();
        }
    }
    s.split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches('"')
        .to_owned()
}

pub struct IconCache {
    cache: HashMap<String, Option<TextureHandle>>,
    pub per_frame_limit: usize,
}

impl Default for IconCache {
    fn default() -> Self {
        Self {
            cache: HashMap::new(),
            per_frame_limit: 8,
        }
    }
}

impl IconCache {
    pub fn get(&mut self, ctx: &egui::Context, key: &str, path: &str) -> Option<TextureHandle> {
        if let Some(tex) = self.cache.get(key) {
            return tex.clone();
        }
        if self.per_frame_limit == 0 {
            return None;
        }
        self.per_frame_limit -= 1;
        let image = extract_icon(path, false).or_else(default_exe_icon);
        let tex = image.map(|(w, h, rgba)| {
            ctx.load_texture(
                format!("icon:{key}"),
                ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba),
                egui::TextureOptions::LINEAR,
            )
        });
        self.cache.insert(key.to_owned(), tex.clone());
        tex
    }

    pub fn reset_frame(&mut self) {
        self.per_frame_limit = 8;
    }
}

pub fn window_icon() -> Option<egui::IconData> {
    let exe = std::env::current_exe().ok()?;
    let (w, h, rgba) = extract_icon(&exe.to_string_lossy(), true)?;
    Some(egui::IconData {
        rgba,
        width: w,
        height: h,
    })
}

pub fn paint(ui: &mut egui::Ui, tex: &TextureHandle) {
    ui.add(egui::Image::from_texture(tex).fit_to_exact_size(Vec2::splat(16.0)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_extraction() {
        assert_eq!(exe_path_from_command(r#""C:\Program Files\App\a.exe" --flag"#), r"C:\Program Files\App\a.exe");
        assert_eq!(exe_path_from_command(r"C:\Windows\System32\foo.exe -x"), r"C:\Windows\System32\foo.exe");
        assert_eq!(exe_path_from_command("  "), "");
    }
}
