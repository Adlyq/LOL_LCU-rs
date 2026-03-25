//! HUD2: 大乱斗板凳席交互层

use tracing::trace;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

use crate::app::event::AppEvent;
use crate::win::base::rgb;
use crate::win::overlay::WndState;

// 布局常量 (1920x1080 模板)
const TEMPLATE_W: f64 = 1920.0;
const TEMPLATE_H: f64 = 1080.0;
const BENCH_L: f64 = 528.0;
const BENCH_T: f64 = 14.0;
const BENCH_R: f64 = 1392.0;
const BENCH_B: f64 = 90.0;
const SLOT_SIZE: f64 = 70.0;
const BENCH_SLOT_COUNT: usize = 10;

#[derive(Clone, Copy, Debug, Default)]
pub struct FRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}
impl FRect {
    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    pub fn right(&self) -> f64 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }
}

pub unsafe extern "system" fn bench_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WndState;
            if !ptr.is_null() {
                let state = &*ptr;
                let sx = (lparam.0 & 0xFFFF) as i16 as i32;
                let sy = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let mut wr = RECT::default();
                if GetWindowRect(hwnd, &mut wr).is_ok() {
                    let px = (sx - wr.left) as f64;
                    let py = (sy - wr.top) as f64;
                    if hit_slot(px, py, state).is_some() {
                        return LRESULT(HTCLIENT as isize);
                    }
                }
            }
            LRESULT(HTTRANSPARENT as isize)
        }
        WM_LBUTTONDOWN => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WndState;
            if !ptr.is_null() {
                let state = &*ptr;
                let cx = (lparam.0 & 0xFFFF) as i16 as i32;
                let cy = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if let Some(idx) = hit_slot(cx as f64, cy as f64, state) {
                    let _ = state.event_tx.try_send(AppEvent::BenchClick(idx));
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub fn get_bench_container_rect(win_w: i32, win_h: i32) -> FRect {
    let scale_x = win_w as f64 / TEMPLATE_W;
    let scale_y = win_h as f64 / TEMPLATE_H;
    FRect {
        x: BENCH_L * scale_x,
        y: BENCH_T * scale_y,
        w: (BENCH_R - BENCH_L) * scale_x,
        h: (BENCH_B - BENCH_T) * scale_y,
    }
}

pub fn get_slot_rect(index: usize, container: FRect, win_w: i32, win_h: i32) -> FRect {
    let scale_x = win_w as f64 / TEMPLATE_W;
    let scale_y = win_h as f64 / TEMPLATE_H;
    let scale = f64::min(scale_x, scale_y);
    let slot_w = SLOT_SIZE * scale;
    let slot_h = SLOT_SIZE * scale;
    let edge_inset = f64::max(0.0, 1.5 * scale);

    FRect {
        x: container.x + (index as f64 * (slot_w + edge_inset)),
        y: container.y,
        w: slot_w,
        h: slot_h,
    }
}

fn hit_slot(px: f64, py: f64, state: &WndState) -> Option<usize> {
    if !state.vm.hud2_visible {
        return None;
    }
    let container = get_bench_container_rect(state.win_w, state.win_h);
    if !container.contains(px, py) {
        return None;
    }
    for i in 0..BENCH_SLOT_COUNT {
        if get_slot_rect(i, container, state.win_w, state.win_h).contains(px, py) {
            return Some(i);
        }
    }
    None
}

pub unsafe fn paint_bench(hwnd: HWND, state: &WndState) {
    trace!("渲染 HUD2 (板凳席交互层)");
    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    let win_w = rect.right - rect.left;
    let win_h = rect.bottom - rect.top;
    if win_w <= 0 || win_h <= 0 {
        return;
    }

    let hdc_screen = GetDC(HWND::default());
    let hdc_mem = CreateCompatibleDC(hdc_screen);

    let bi = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: win_w,
        biHeight: -win_h,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    let hbm = CreateDIBSection(
        hdc_mem,
        &BITMAPINFO {
            bmiHeader: bi,
            ..Default::default()
        },
        DIB_RGB_COLORS,
        &mut bits_ptr,
        HANDLE::default(),
        0,
    )
    .expect("无法创建 DIBSection");
    let old_bm = SelectObject(hdc_mem, hbm);

    // 初始化全透明
    std::ptr::write_bytes(bits_ptr, 0, (win_w * win_h * 4) as usize);
    let pixels = std::slice::from_raw_parts_mut(bits_ptr as *mut u32, (win_w * win_h) as usize);

    if state.vm.hud2_visible {
        let container = get_bench_container_rect(win_w, win_h);
        let scale_y = win_h as f64 / TEMPLATE_H;

        // 填充容器背景 (Alpha=2)
        fill_rect_alpha(pixels, win_w, win_h, container, 0, 0, 0, 2);

        let pen_gray = CreatePen(PS_SOLID, 1, rgb(128, 128, 128));
        let old_pen = SelectObject(hdc_mem, pen_gray);
        let old_brush = SelectObject(hdc_mem, GetStockObject(NULL_BRUSH));
        let _ = RoundRect(
            hdc_mem,
            container.x as i32,
            container.y as i32,
            container.right() as i32,
            container.bottom() as i32,
            (10.0 * scale_y) as i32,
            (10.0 * scale_y) as i32,
        );

        let pen_slot = CreatePen(PS_SOLID, 1, rgb(160, 160, 160));
        SelectObject(hdc_mem, pen_slot);
        for i in 0..BENCH_SLOT_COUNT {
            let sr = get_slot_rect(i, container, win_w, win_h);
            if state.vm.hud2_selected_slot == Some(i) {
                fill_rect_alpha(pixels, win_w, win_h, sr, 130, 255, 130, 65);
            } else {
                fill_rect_alpha(pixels, win_w, win_h, sr, 0, 0, 0, 2);
            }
            let _ = RoundRect(
                hdc_mem,
                sr.x as i32,
                sr.y as i32,
                sr.right() as i32,
                sr.bottom() as i32,
                (8.0 * scale_y) as i32,
                (8.0 * scale_y) as i32,
            );
        }

        SelectObject(hdc_mem, old_pen);
        SelectObject(hdc_mem, old_brush);
        let _ = DeleteObject(pen_gray);
        let _ = DeleteObject(pen_slot);
    }

    let pt_dst = POINT {
        x: rect.left,
        y: rect.top,
    };
    let pt_src = POINT { x: 0, y: 0 };
    let size_dst = SIZE {
        cx: win_w,
        cy: win_h,
    };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let _ = UpdateLayeredWindow(
        hwnd,
        hdc_screen,
        Some(&pt_dst),
        Some(&size_dst),
        hdc_mem,
        Some(&pt_src),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );

    SelectObject(hdc_mem, old_bm);
    let _ = DeleteObject(hbm);
    let _ = DeleteDC(hdc_mem);
    ReleaseDC(HWND::default(), hdc_screen);
}

fn fill_rect_alpha(
    pixels: &mut [u32],
    win_w: i32,
    win_h: i32,
    rect: FRect,
    r: u8,
    g: u8,
    b: u8,
    a: u8,
) {
    let x0 = rect.x.round() as i32;
    let y0 = rect.y.round() as i32;
    let x1 = (rect.x + rect.w).round() as i32;
    let y1 = (rect.y + rect.h).round() as i32;

    let alpha_f = a as f32 / 255.0;
    let pr = (r as f32 * alpha_f) as u32;
    let pg = (g as f32 * alpha_f) as u32;
    let pb = (b as f32 * alpha_f) as u32;
    let color = (a as u32) << 24 | pr << 16 | pg << 8 | pb;

    for y in y0.max(0)..y1.min(win_h) {
        for x in x0.max(0)..x1.min(win_w) {
            pixels[(y * win_w + x) as usize] = color;
        }
    }
}
