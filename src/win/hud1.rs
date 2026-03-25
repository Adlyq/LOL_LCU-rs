//! HUD1: 左上角信息面板

use tracing::trace;
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::win::base::rgb;
use crate::win::overlay::WndState;
use crate::win::winapi::to_wide;

pub unsafe extern "system" fn hud_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub unsafe fn paint_hud(hwnd: HWND, state: &WndState) {
    trace!("渲染 HUD1 (Path API 优化版)");
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
    std::ptr::write_bytes(bits_ptr, 0, (win_w * win_h * 4) as usize);

    let face_name = to_wide("Microsoft YaHei");
    let hfont = CreateFontW(
        22,
        0,
        0,
        0,
        FW_BOLD.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        ANTIALIASED_QUALITY.0 as u32,
        (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32,
        PCWSTR(face_name.as_ptr()),
    );
    let hfont_prophet = CreateFontW(
        20,
        0,
        0,
        0,
        FW_BOLD.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        ANTIALIASED_QUALITY.0 as u32,
        (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32,
        PCWSTR(face_name.as_ptr()),
    );

    SetBkMode(hdc_mem, TRANSPARENT);

    let mut y = 40;
    let x = 10;

    // 渲染标题
    if !state.vm.hud1_title.is_empty() {
        let old_font = SelectObject(hdc_mem, hfont);
        draw_optimized_stroked_text(hdc_mem, &state.vm.hud1_title, x, y, rgb(0, 255, 0));
        SelectObject(hdc_mem, old_font);
        y += 32;
    }

    // 渲染列表内容
    for line in &state.vm.hud1_lines {
        let color = if line.contains("通天代") {
            rgb(255, 215, 0)
        } else if line.contains("小代") {
            rgb(255, 100, 255)
        } else if line.contains("上等马") {
            rgb(255, 80, 80)
        } else if line.contains("中等马") {
            rgb(100, 255, 100)
        } else if line.contains("获取失败") || line.contains("加载中") {
            rgb(120, 120, 120)
        } else if line.contains("[蓝方]") || line.contains("[我方评分]") {
            rgb(100, 149, 237)
        } else if line.contains("[红方]") || line.contains("[敌方评分]") {
            rgb(255, 69, 0)
        } else if line.starts_with('[') {
            rgb(0, 255, 255)
        } else {
            rgb(200, 200, 200)
        };

        let font = if line.contains("评分:") {
            hfont_prophet
        } else {
            hfont
        };
        let old_font = SelectObject(hdc_mem, font);
        draw_optimized_stroked_text(hdc_mem, line, x, y, color);
        SelectObject(hdc_mem, old_font);
        y += 24;
    }

    // 后处理：将有像素的区域 Alpha 设为 255
    let pixels = std::slice::from_raw_parts_mut(bits_ptr as *mut u8, (win_w * win_h * 4) as usize);
    for chunk in pixels.chunks_exact_mut(4) {
        if chunk[0] > 0 || chunk[1] > 0 || chunk[2] > 0 {
            chunk[3] = 255;
        }
    }

    let pt_dst = POINT { x: 0, y: 0 };
    let pt_src = POINT { x: 0, y: 0 };
    let size_dst = SIZE {
        cx: win_w,
        cy: win_h,
    };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 178,
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

    let _ = DeleteObject(hfont);
    let _ = DeleteObject(hfont_prophet);
    SelectObject(hdc_mem, old_bm);
    let _ = DeleteObject(hbm);
    let _ = DeleteDC(hdc_mem);
    ReleaseDC(HWND::default(), hdc_screen);
}

/// 使用 GDI Path API 优化的高性能描边渲染
unsafe fn draw_optimized_stroked_text(hdc: HDC, text: &str, x: i32, y: i32, color: COLORREF) {
    let wide_text = to_wide(text);
    let slice = &wide_text[..wide_text.len() - 1];

    // 1. 绘制描边
    let pen = CreatePen(PS_SOLID, 3, rgb(10, 10, 10)); // 3像素宽的黑色描边
    let old_pen = SelectObject(hdc, pen);

    let _ = BeginPath(hdc);
    let _ = TextOutW(hdc, x, y, slice);
    let _ = EndPath(hdc);

    // 将路径转化为描边轮廓并填充
    let _ = StrokePath(hdc);

    // 2. 绘制主体文字
    SetTextColor(hdc, color);
    let _ = TextOutW(hdc, x, y, slice);

    SelectObject(hdc, old_pen);
    let _ = DeleteObject(pen);
}
