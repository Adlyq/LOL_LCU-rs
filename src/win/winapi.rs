//! Win32 API 封装
//!
//! 对应 Python `ui/overlay.py` 中的 `WinApi` 类。
//!
//! 所有函数均为纯 Win32 调用，不依赖任何 GUI 框架。

#![allow(dead_code)]

use std::ffi::OsStr;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    RedrawWindow, RDW_ALLCHILDREN, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW, UpdateWindow,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

// ── 工具函数 ──────────────────────────────────────────────────────

pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(once(0)).collect()
}

/// 获取窗口标题（UTF-16 → String）。
fn get_window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf: Vec<u16> = vec![0u16; (len + 1) as usize];
        GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..len as usize]).trim().to_owned()
    }
}

fn is_window_visible(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd).as_bool() }
}

// ── 公开 API ──────────────────────────────────────────────────────

/// 查找 League of Legends 客户端窗口句柄。
///
/// 对应 Python `WinApi.find_lcu_window()`。
pub fn find_lcu_window() -> Option<HWND> {
    unsafe {
        // 精确类名匹配（最快路径）
        let class = to_wide("RCLIENT");
        let title = to_wide("League of Legends");
        let exact = FindWindowW(PCWSTR(class.as_ptr()), PCWSTR(title.as_ptr()));
        if let Ok(h) = exact {
            if !h.is_invalid() {
                return Some(h);
            }
        }
    }

    // 枚举所有顶层窗口
    struct Ctx {
        candidates: Vec<(HWND, String)>,
    }

    let mut ctx = Ctx { candidates: vec![] };

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let title = get_window_title(hwnd);
        if title.is_empty() {
            return BOOL(1);
        }
        let lower = title.to_lowercase();
        let keywords = [
            "league of legends",
            "leagueclientux",
            "league client",
            "英雄联盟",
        ];
        if keywords.iter().any(|kw| lower.contains(kw)) {
            ctx.candidates.push((hwnd, title));
        }
        BOOL(1)
    }

    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut ctx as *mut Ctx as isize));
    }

    if ctx.candidates.is_empty() {
        return None;
    }

    // 优先包含 "League of Legends" 或 "英雄联盟" 的窗口
    for (hwnd, title) in &ctx.candidates {
        let lower = title.to_lowercase();
        if lower.contains("league of legends") || title.contains("英雄联盟") {
            return Some(*hwnd);
        }
    }
    Some(ctx.candidates[0].0)
}

/// 获取窗口矩形（屏幕坐标）。
pub fn get_window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe {
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            Some(rect)
        } else {
            None
        }
    }
}

/// 获取客户区矩形（窗口内坐标，左上角始终为 0,0）。
pub fn get_client_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe {
        if GetClientRect(hwnd, &mut rect).is_ok() {
            Some(rect)
        } else {
            None
        }
    }
}

/// 判断窗口是否最小化。
pub fn is_minimized(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd).as_bool() }
}

/// 查找子窗口（按类名）。
pub fn find_child_window(hwnd: HWND, class_name: &str) -> Option<HWND> {
    let class = to_wide(class_name);
    unsafe {
        match FindWindowExW(hwnd, HWND::default(), PCWSTR(class.as_ptr()), PCWSTR::null()) {
            Ok(h) if !h.is_invalid() => Some(h),
            _ => None,
        }
    }
}

/// 重绘顶层窗口（含 FrameChanged）。
///
/// 对应 Python `WinApi.redraw_top_level_window()`。
pub fn redraw_top_level_window(hwnd: HWND) -> bool {
    let rect = match get_window_rect(hwnd) {
        Some(r) => r,
        None => return false,
    };
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 0 || h <= 0 {
        return false;
    }
    unsafe {
        let set_ok = SetWindowPos(
            hwnd,
            HWND::default(),
            rect.left,
            rect.top,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
        );
        let rdw_ok = RedrawWindow(
            hwnd,
            None,
            None,
            RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN | RDW_FRAME,
        );
        let _ = UpdateWindow(hwnd);
        set_ok.is_ok() && rdw_ok.as_bool()
    }
}

/// 重绘窗口（NOMOVE|NOSIZE|FrameChanged）。
///
/// 对应 Python `WinApi.redraw_window()`。
pub fn redraw_window(hwnd: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    unsafe {
        let set_ok = SetWindowPos(
            hwnd,
            HWND::default(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE
                | SWP_NOSIZE
                | SWP_NOZORDER
                | SWP_NOACTIVATE
                | SWP_FRAMECHANGED,
        );
        let rdw_ok = RedrawWindow(
            hwnd,
            None,
            None,
            RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN | RDW_FRAME,
        );
        let _ = UpdateWindow(hwnd);
        set_ok.is_ok() && rdw_ok.as_bool()
    }
}

/// 自动重绘 LCU 窗口（主窗口 + CefBrowserWindow）。
pub fn auto_redraw_lcu_window(hwnd: HWND) -> bool {
    if hwnd.is_invalid() || is_minimized(hwnd) {
        return false;
    }
    let parent_ok = redraw_top_level_window(hwnd);
    match find_child_window(hwnd, "CefBrowserWindow") {
        Some(cef) => {
            let child_ok = redraw_window(cef);
            parent_ok && child_ok
        }
        None => parent_ok,
    }
}

/// 获取主屏幕分辨率。
pub fn get_primary_screen_size() -> (i32, i32) {
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        (w, h)
    }
}

/// 向窗口发送模拟 DPI 变化消息，触发 LCU 重新计算布局。
///
/// 对应 Python `WinApi.patch_dpi_changed_message()`。
pub fn patch_dpi_changed_message(hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        if dpi == 0 {
            return;
        }
        let wparam = WPARAM(((dpi << 16) | dpi) as usize);
        // l_param 传一个合法的 RECT 指针（内容不重要）
        let rect = RECT::default();
        let lparam = LPARAM(&rect as *const RECT as isize);
        const WM_DPICHANGED: u32 = 0x02E0;
        let _ = SendMessageW(hwnd, WM_DPICHANGED, wparam, lparam);
    }
}

/// SetWindowPos 封装。
pub fn set_window_pos(hwnd: HWND, x: i32, y: i32, w: i32, h: i32, flags: SET_WINDOW_POS_FLAGS) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    unsafe { SetWindowPos(hwnd, HWND::default(), x, y, w, h, flags).is_ok() }
}

/// 将 overlay 窗口放置在 LCU 窗口正上方（Z 序）。
///
/// 对应 Python `WinApi.place_window_above_target()`。
pub fn place_window_above_target(hwnd: HWND, target_hwnd: HWND, rect: &RECT) -> bool {
    if hwnd.is_invalid() || target_hwnd.is_invalid() {
        return false;
    }
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;

    unsafe {
        let above_hwnd = GetWindow(target_hwnd, GW_HWNDPREV);
        let above_hwnd = match above_hwnd {
            Ok(h) if !h.is_invalid() => h,
            _ => HWND::default(), // 0
        };

        let (insert_after, flags) = if above_hwnd.is_invalid() {
            // 目标已在顶部
            (
                HWND_TOP,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING | SWP_SHOWWINDOW,
            )
        } else if above_hwnd == hwnd {
            // overlay 本身已在目标上方，只同步几何
            (
                HWND::default(),
                SWP_NOACTIVATE
                    | SWP_NOOWNERZORDER
                    | SWP_NOSENDCHANGING
                    | SWP_NOZORDER
                    | SWP_SHOWWINDOW,
            )
        } else {
            (
                above_hwnd,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING | SWP_SHOWWINDOW,
            )
        };

        SetWindowPos(hwnd, insert_after, rect.left, rect.top, w, h, flags).is_ok()
    }
}

/// 通过窗口标题查找并设置窗口整体透明度（30–100%）。
/// 返回是否成功找到窗口并设置。
pub fn set_window_opacity_by_title(title: &str, percent: u8) -> bool {
    unsafe {
        let title_wide = to_wide(title);
        let hwnd = match FindWindowW(
            PCWSTR::null(),
            PCWSTR(title_wide.as_ptr()),
        ) {
            Ok(h) if !h.is_invalid() => h,
            _ => return false,
        };
        set_window_opacity(hwnd, percent)
    }
}

/// 设置指定窗口的透明度（30–100%）。
pub fn set_window_opacity(hwnd: HWND, percent: u8) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    unsafe {
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        if SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style | WS_EX_LAYERED.0 as i32) == 0 {
             // 检查错误？有些窗口可能不支持，但在 Windows 上通常 ok
        }
        let alpha = (percent.clamp(30, 100) as f32 / 100.0 * 255.0) as u8;
        SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA).is_ok()
    }
}

/// 在 LCU 窗口客户区内按比例坐标发送鼠标点击消息。
///
/// 对应 Python `WinApi.click_client_relative()`。
pub fn click_client_relative(hwnd: HWND, x_ratio: f64, y_ratio: f64) -> bool {
    if hwnd.is_invalid() || is_minimized(hwnd) {
        return false;
    }

    let target = find_child_window(hwnd, "CefBrowserWindow").unwrap_or(hwnd);
    let client = match get_client_rect(target) {
        Some(r) => r,
        None => return false,
    };
    let cw = client.right - client.left;
    let ch = client.bottom - client.top;
    if cw <= 0 || ch <= 0 {
        return false;
    }

    let x = (cw as f64 * x_ratio.clamp(0.0, 1.0)) as i32;
    let y = (ch as f64 * y_ratio.clamp(0.0, 1.0)) as i32;
    let lparam = LPARAM(((y as u32 as u64) << 16 | (x as u32 as u64)) as isize);

    const WM_MOUSEMOVE: u32 = 0x0200;
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const MK_LBUTTON: usize = 0x0001;

    unsafe {
        let _ = SetForegroundWindow(hwnd);
        let move_ok = PostMessageW(target, WM_MOUSEMOVE, WPARAM(0), lparam);
        let down_ok = PostMessageW(target, WM_LBUTTONDOWN, WPARAM(MK_LBUTTON), lparam);
        let up_ok = PostMessageW(target, WM_LBUTTONUP, WPARAM(0), lparam);
        move_ok.is_ok() && down_ok.is_ok() && up_ok.is_ok()
    }
}

/// 点击游戏结束后的"继续"按钮。
///
/// 对应 Python `WinApi.click_postgame_continue()`。
pub fn click_postgame_continue(hwnd: Option<HWND>) -> bool {
    let target = match hwnd {
        Some(h) => h,
        None => match find_lcu_window() {
            Some(h) => h,
            None => return false,
        },
    };

    let x_ratio: f64 = std::env::var("POSTGAME_CONTINUE_X_RATIO")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.5);
    let y_ratio: f64 = std::env::var("POSTGAME_CONTINUE_Y_RATIO")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.935);

    click_client_relative(target, x_ratio, y_ratio)
}

/// 判断窗口比例是否需要修复（偏离 16:9 超过阈值）。
pub fn need_resize(rect: &RECT) -> bool {
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 0 {
        return false;
    }
    let ratio = h as f64 / w as f64;
    (ratio - (9.0 / 16.0)).abs() > 0.002
}

/// 递归对齐所有 CEF 相关子窗口，确保它们填满父级窗口客户区。
pub unsafe fn align_all_cef_windows(parent: HWND) {
    let mut child = HWND::default();
    loop {
        child = match FindWindowExW(parent, child, PCWSTR::null(), PCWSTR::null()) {
            Ok(h) if !h.is_invalid() => h,
            _ => break,
        };

        let class = get_window_class(child);
        // CEF 关键窗口类名：CefBrowserWindow, Chrome_WidgetWin_0, Chrome_RenderWidgetHostHWND
        if class.contains("Cef") || class.contains("Chrome") {
            let mut pr = RECT::default();
            if GetClientRect(parent, &mut pr).is_ok() {
                let _ = SetWindowPos(child, HWND::default(), 0, 0, pr.right, pr.bottom, SWP_NOACTIVATE | SWP_NOZORDER | SWP_ASYNCWINDOWPOS);
            }
        }
        align_all_cef_windows(child);
    }
}

/// 获取窗口类名。
fn get_window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len == 0 { return String::new(); }
    String::from_utf16_lossy(&buf[..len as usize])
}

/// 按 zoom_scale 修复 LCU 窗口尺寸（含 DPI patch + 深度对齐）。
///
/// 对应 Python `WinApi.fix_lcu_window_by_zoom()`。
pub fn fix_lcu_window_by_zoom(hwnd: HWND, zoom_scale: f64, forced: bool) -> bool {
    if hwnd.is_invalid() || is_minimized(hwnd) {
        return false;
    }

    let cef = match find_child_window(hwnd, "CefBrowserWindow") {
        Some(h) => h,
        None => return false,
    };

    let main_rect = match get_window_rect(hwnd) {
        Some(r) => r,
        None => return false,
    };

    if !forced && !need_resize(&main_rect) {
        return false;
    }

    let target_w = (1600.0 * zoom_scale) as i32;
    let target_h = (900.0 * zoom_scale) as i32;
    if target_w <= 0 || target_h <= 0 {
        return false;
    }

    let (screen_w, screen_h) = get_primary_screen_size();
    let target_x = (screen_w - target_w) / 2;
    let target_y = (screen_h - target_h) / 2;

    patch_dpi_changed_message(hwnd);
    patch_dpi_changed_message(cef);

    let main_ok = set_window_pos(hwnd, target_x, target_y, target_w, target_h, SWP_SHOWWINDOW);
    
    unsafe {
        // 关键增强：深度对齐所有渲染层
        align_all_cef_windows(hwnd);
    }

    redraw_window(hwnd);
    redraw_window(cef);

    main_ok
}
