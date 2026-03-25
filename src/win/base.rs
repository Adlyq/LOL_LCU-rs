use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::COLORREF;

#[derive(Clone, Copy, Debug)]
pub struct SendHwnd(pub HWND);
unsafe impl Send for SendHwnd {}
unsafe impl Sync for SendHwnd {}

pub fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

pub fn hex_to_rgb(hex: u32) -> COLORREF {
    COLORREF(hex)
}
