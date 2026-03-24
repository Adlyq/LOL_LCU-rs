//! Overlay 窗口与系统托盘管理
//! 
//! 架构设计：
//! 1. HUD 窗口：显示状态信息，锁定屏幕 (0,0)，70% 不透明度，全点击穿透。
//! 2. Bench 窗口：大乱斗板凳席交互，固定 10 席位，透明背景，跟随 LCU 窗口。
//! 3. Tray 窗口：独立消息窗口，处理托盘与菜单。

use std::thread;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicIsize, Ordering};
use tracing::{info, warn, debug, trace};

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::UI::Shell::*;

use crate::app::config::SharedConfig;
use crate::app::event::{AppEvent, TrayAction};
use crate::app::viewmodel::ViewModel;
use crate::win::winapi::{self, to_wide};

// ── 布局常量 (1920x1080 模板) ──────────────────────────────────

const TEMPLATE_W: f64 = 1920.0;
const TEMPLATE_H: f64 = 1080.0;
const BENCH_L: f64 = 528.0;
const BENCH_T: f64 = 14.0;
const BENCH_R: f64 = 1392.0;
const BENCH_B: f64 = 90.0;
const SLOT_SIZE: f64 = 70.0;
const BENCH_SLOT_COUNT: usize = 10;

// ── 常量定义 ─────────────────────────────────────────────────────

const WM_TRAY_ICON: u32 = WM_USER + 100;
const TRAY_UID: u32 = 1;

const ID_QUIT: usize = 1001;
const ID_RELOAD_UX: usize = 1002;
const ID_PLAY_AGAIN: usize = 1003;
const ID_FIND_LOOT: usize = 1004;
const ID_FIX_WINDOW: usize = 1005;

const ID_AUTO_ACCEPT: usize = 2001;
const ID_AUTO_HONOR: usize = 2002;
const ID_PREMADE_CHAMP: usize = 2003;
const ID_MEMORY_MONITOR: usize = 2004;

// ── 线程安全包装 ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct SendHwnd(HWND);
unsafe impl Send for SendHwnd {}
unsafe impl Sync for SendHwnd {}

// ── 几何类型 ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default)]
struct FRect { x: f64, y: f64, w: f64, h: f64 }
impl FRect {
    fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    fn right(&self) -> f64 { self.x + self.w }
    fn bottom(&self) -> f64 { self.y + self.h }
}

#[derive(Clone)]
pub struct OverlaySender {
    _tx: tokio::sync::mpsc::Sender<AppEvent>,
    _hud_hwnd: SendHwnd,
}

pub fn spawn_overlay_thread(
    config: SharedConfig,
    event_tx: tokio::sync::mpsc::Sender<AppEvent>,
    vm_rx: tokio::sync::watch::Receiver<ViewModel>,
) -> OverlaySender {
    let (hwnd_tx, hwnd_rx) = std_mpsc::channel();
    let event_tx_c = event_tx.clone();

    info!("正在启动 Overlay 线程...");
    thread::spawn(move || {
        overlay_message_loop(config, event_tx_c, vm_rx, hwnd_tx);
    });

    let hud_hwnd = hwnd_rx.recv().expect("无法获取 Overlay HWND");
    OverlaySender { _tx: event_tx, _hud_hwnd: hud_hwnd }
}

// ── 状态结构 ─────────────────────────────────────────────────────

struct WndState {
    vm: ViewModel,
    config: SharedConfig,
    event_tx: tokio::sync::mpsc::Sender<AppEvent>,
    
    win_w: i32,
    win_h: i32,
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

// ── 几何计算 ─────────────────────────────────────────────────────

fn get_bench_container_rect(win_w: i32, win_h: i32) -> FRect {
    let scale_x = win_w as f64 / TEMPLATE_W;
    let scale_y = win_h as f64 / TEMPLATE_H;
    FRect {
        x: BENCH_L * scale_x,
        y: BENCH_T * scale_y,
        w: (BENCH_R - BENCH_L) * scale_x,
        h: (BENCH_B - BENCH_T) * scale_y,
    }
}

fn get_slot_rect(index: usize, _count: usize, container: FRect, win_w: i32, win_h: i32) -> FRect {
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
    if !state.vm.hud2_visible { return None; }
    let container = get_bench_container_rect(state.win_w, state.win_h);
    if !container.contains(px, py) { return None; }
    for i in 0..BENCH_SLOT_COUNT {
        if get_slot_rect(i, BENCH_SLOT_COUNT, container, state.win_w, state.win_h).contains(px, py) {
            return Some(i);
        }
    }
    None
}

// ── 绘制逻辑 ─────────────────────────────────────────────────────

unsafe fn paint_hud(hwnd: HWND, state: &WndState) {
    trace!("重绘 HUD1...");
    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    let win_w = rect.right - rect.left;
    let win_h = rect.bottom - rect.top;
    if win_w <= 0 || win_h <= 0 { return; }

    let hdc_screen = GetDC(HWND::default());
    let hdc_mem = CreateCompatibleDC(hdc_screen);

    let bi = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: win_w, biHeight: -win_h, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default()
    };
    let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    let hbm = CreateDIBSection(hdc_mem, &BITMAPINFO { bmiHeader: bi, ..Default::default() }, DIB_RGB_COLORS, &mut bits_ptr, HANDLE::default(), 0).unwrap();
    let old_bm = SelectObject(hdc_mem, hbm);
    std::ptr::write_bytes(bits_ptr, 0, (win_w * win_h * 4) as usize);

    let face_name = to_wide("Microsoft YaHei");
    let hfont = CreateFontW(22, 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32, ANTIALIASED_QUALITY.0 as u32, (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32, PCWSTR(face_name.as_ptr()));
    let hfont_small = CreateFontW(16, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32, ANTIALIASED_QUALITY.0 as u32, (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32, PCWSTR(face_name.as_ptr()));
    let hfont_prophet = CreateFontW(20, 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32, ANTIALIASED_QUALITY.0 as u32, (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32, PCWSTR(face_name.as_ptr()));

    let old_font = SelectObject(hdc_mem, hfont);
    SetBkMode(hdc_mem, TRANSPARENT);
    
    let mut y = 40; 
    let x = 10;
    
    if !state.vm.hud1_title.is_empty() {
        draw_stroked_text(hdc_mem, &state.vm.hud1_title, x, y, rgb(0, 255, 0));
        y += 32;
    }
    
    for line in &state.vm.hud1_lines {
        let color = if line.contains("通天代") { rgb(255, 215, 0) }
            else if line.contains("小代") { rgb(255, 100, 255) }
            else if line.contains("上等马") { rgb(255, 80, 80) }
            else if line.contains("中等马") { rgb(100, 255, 100) }
            else if line.contains("获取失败") || line.contains("加载中") { rgb(120, 120, 120) }
            else if line.contains("[蓝方]") || line.contains("[我方评分]") { rgb(100, 149, 237) }
            else if line.contains("[红方]") || line.contains("[敌方评分]") { rgb(255, 69, 0) }
            else if line.starts_with('[') { rgb(0, 255, 255) }
            else { rgb(200, 200, 200) };

        if line.contains("评分:") {
            SelectObject(hdc_mem, hfont_prophet);
        } else {
            SelectObject(hdc_mem, hfont);
        }
            
        draw_stroked_text(hdc_mem, line, x, y, color);
        y += 24;
    }

    let pixels = std::slice::from_raw_parts_mut(bits_ptr as *mut u8, (win_w * win_h * 4) as usize);
    for chunk in pixels.chunks_exact_mut(4) {
        if chunk[0] > 0 || chunk[1] > 0 || chunk[2] > 0 { chunk[3] = 255; }
    }

    let pt_dst = POINT { x: 0, y: 0 };
    let pt_src = POINT { x: 0, y: 0 };
    let size_dst = SIZE { cx: win_w, cy: win_h };
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 178, AlphaFormat: AC_SRC_ALPHA as u8 };
    let _ = UpdateLayeredWindow(hwnd, hdc_screen, Some(&pt_dst), Some(&size_dst), hdc_mem, Some(&pt_src), COLORREF(0), Some(&blend), ULW_ALPHA);

    SelectObject(hdc_mem, old_font);
    let _ = DeleteObject(hfont);
    let _ = DeleteObject(hfont_small);
    let _ = DeleteObject(hfont_prophet);
    SelectObject(hdc_mem, old_bm);
    let _ = DeleteObject(hbm);
    let _ = DeleteDC(hdc_mem);
    ReleaseDC(HWND::default(), hdc_screen);
}

unsafe fn paint_bench(hwnd: HWND, state: &WndState) {
    trace!("重绘 HUD2 (板凳席)...");
    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    let win_w = rect.right - rect.left;
    let win_h = rect.bottom - rect.top;
    if win_w <= 0 || win_h <= 0 { return; }

    let hdc_screen = GetDC(HWND::default());
    let hdc_mem = CreateCompatibleDC(hdc_screen);

    let bi = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: win_w, biHeight: -win_h, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default()
    };
    let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    let hbm = CreateDIBSection(hdc_mem, &BITMAPINFO { bmiHeader: bi, ..Default::default() }, DIB_RGB_COLORS, &mut bits_ptr, HANDLE::default(), 0).unwrap();
    let old_bm = SelectObject(hdc_mem, hbm);
    
    std::ptr::write_bytes(bits_ptr, 0, (win_w * win_h * 4) as usize);
    let pixels = std::slice::from_raw_parts_mut(bits_ptr as *mut u32, (win_w * win_h) as usize);

    if state.vm.hud2_visible {
        let container = get_bench_container_rect(win_w, win_h);
        let scale_y = win_h as f64 / TEMPLATE_H;
        
        fill_rect_alpha(pixels, win_w, win_h, container, 0, 0, 0, 2);
        
        let pen_gray = CreatePen(PS_SOLID, 1, rgb(128, 128, 128));
        let old_pen = SelectObject(hdc_mem, pen_gray);
        let old_brush = SelectObject(hdc_mem, GetStockObject(NULL_BRUSH));
        let _ = RoundRect(hdc_mem, container.x as i32, container.y as i32, container.right() as i32, container.bottom() as i32, (10.0 * scale_y) as i32, (10.0 * scale_y) as i32);
        
        let pen_slot = CreatePen(PS_SOLID, 1, rgb(160, 160, 160));
        SelectObject(hdc_mem, pen_slot);
        for i in 0..BENCH_SLOT_COUNT {
            let sr = get_slot_rect(i, BENCH_SLOT_COUNT, container, win_w, win_h);
            if state.vm.hud2_selected_slot == Some(i) {
                fill_rect_alpha(pixels, win_w, win_h, sr, 130, 255, 130, 65);
            } else {
                fill_rect_alpha(pixels, win_w, win_h, sr, 0, 0, 0, 2);
            }
            let _ = RoundRect(hdc_mem, sr.x as i32, sr.y as i32, sr.right() as i32, sr.bottom() as i32, (8.0 * scale_y) as i32, (8.0 * scale_y) as i32);
        }
        
        SelectObject(hdc_mem, old_pen);
        SelectObject(hdc_mem, old_brush);
        let _ = DeleteObject(pen_gray);
        let _ = DeleteObject(pen_slot);
    }

    let pt_dst = POINT { x: rect.left, y: rect.top };
    let pt_src = POINT { x: 0, y: 0 };
    let size_dst = SIZE { cx: win_w, cy: win_h };
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    let _ = UpdateLayeredWindow(hwnd, hdc_screen, Some(&pt_dst), Some(&size_dst), hdc_mem, Some(&pt_src), COLORREF(0), Some(&blend), ULW_ALPHA);

    SelectObject(hdc_mem, old_bm);
    let _ = DeleteObject(hbm);
    let _ = DeleteDC(hdc_mem);
    ReleaseDC(HWND::default(), hdc_screen);
}

fn fill_rect_alpha(pixels: &mut [u32], win_w: i32, win_h: i32, rect: FRect, r: u8, g: u8, b: u8, a: u8) {
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

unsafe fn draw_stroked_text(hdc: HDC, text: &str, x: i32, y: i32, color: COLORREF) {
    let wide_text = to_wide(text);
    let slice = &wide_text[..wide_text.len() - 1]; 
    SetTextColor(hdc, rgb(10, 10, 10));
    for dx in -2..=2 {
        for dy in -2..=2 {
            if dx == 0 && dy == 0 { continue; }
            let _ = TextOutW(hdc, x + dx, y + dy, slice);
        }
    }
    SetTextColor(hdc, color);
    let _ = TextOutW(hdc, x, y, slice);
}

// ── 窗口过程 ─────────────────────────────────────────────────────

unsafe extern "system" fn tray_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_TRAY_ICON {
        let event = (lparam.0 as u32) & 0xFFFF;
        if event == WM_RBUTTONUP || event == WM_CONTEXTMENU { show_tray_menu(hwnd); }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn hud_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn bench_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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
                    if hit_slot(px, py, state).is_some() { return LRESULT(HTCLIENT as isize); }
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

// ── 菜单逻辑 ─────────────────────────────────────────────────────

unsafe fn show_tray_menu(tray_hwnd: HWND) {
    let ptr = GetWindowLongPtrW(tray_hwnd, GWLP_USERDATA) as *const WndState;
    if ptr.is_null() { return; }
    let state = &*ptr;
    let hmenu = CreatePopupMenu().unwrap();
    let _ = AppendMenuW(hmenu, MF_STRING, ID_FIX_WINDOW, PCWSTR(to_wide("修复客户端窗口比例").as_ptr()));
    let _ = AppendMenuW(hmenu, MF_STRING, ID_FIND_LOOT, PCWSTR(to_wide("找回一些遗忘的东西").as_ptr()));
    let _ = AppendMenuW(hmenu, MF_STRING, ID_PLAY_AGAIN, PCWSTR(to_wide("退出结算页面").as_ptr()));
    let _ = AppendMenuW(hmenu, MF_STRING, ID_RELOAD_UX, PCWSTR(to_wide("热重载客户端").as_ptr()));
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
    {
        let config = state.config.lock();
        let add_item = |id, text: &str, checked| {
            let mut flags = MF_STRING;
            if checked { flags |= MF_CHECKED; }
            let title = to_wide(text);
            let _ = AppendMenuW(hmenu, flags, id, PCWSTR(title.as_ptr()));
        };
        add_item(ID_AUTO_ACCEPT, "自动接受对局", config.auto_accept_enabled);
        add_item(ID_AUTO_HONOR, "自动点赞跳过", config.auto_honor_skip);
        add_item(ID_PREMADE_CHAMP, "选人阶段组队分析", config.premade_champ_select);
        add_item(ID_MEMORY_MONITOR, "内存监控自动重载", config.memory_monitor);
    }
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
    let quit_title = to_wide("退出程序");
    let _ = AppendMenuW(hmenu, MF_STRING, ID_QUIT, PCWSTR(quit_title.as_ptr()));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(tray_hwnd);
    let cmd = TrackPopupMenu(hmenu, TPM_RIGHTBUTTON | TPM_RETURNCMD, pt.x, pt.y, 0, tray_hwnd, None);
    let _ = PostMessageW(tray_hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(hmenu);
    if cmd.0 != 0 { handle_menu_command(tray_hwnd, cmd.0 as usize); }
}

unsafe fn handle_menu_command(hwnd: HWND, id: usize) {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WndState;
    if ptr.is_null() { return; }
    let state = &*ptr;
    match id {
        ID_FIX_WINDOW => { let _ = state.event_tx.try_send(AppEvent::TrayAction(TrayAction::FixWindow)); }
        ID_FIND_LOOT => { let _ = state.event_tx.try_send(AppEvent::TrayAction(TrayAction::FindForgottenLoot)); }
        ID_PLAY_AGAIN => { let _ = state.event_tx.try_send(AppEvent::TrayAction(TrayAction::PlayAgain)); }
        ID_RELOAD_UX => { let _ = state.event_tx.try_send(AppEvent::TrayAction(TrayAction::ReloadUx)); }
        ID_AUTO_ACCEPT => { let _ = state.event_tx.try_send(AppEvent::TrayAction(TrayAction::ToggleAutoAccept)); }
        ID_AUTO_HONOR => { let _ = state.event_tx.try_send(AppEvent::TrayAction(TrayAction::ToggleAutoHonor)); }
        ID_PREMADE_CHAMP => { let _ = state.event_tx.try_send(AppEvent::TrayAction(TrayAction::TogglePremadeChamp)); }
        ID_MEMORY_MONITOR => { let _ = state.event_tx.try_send(AppEvent::TrayAction(TrayAction::ToggleMemoryMonitor)); } 
        ID_QUIT => { let _ = state.event_tx.try_send(AppEvent::TrayAction(TrayAction::Exit)); }
        _ => {}
    }
}

// ── 核心消息循环 ─────────────────────────────────────────────

static UI_HWND: AtomicIsize = AtomicIsize::new(0);
static KEYBOARD_HOOK: AtomicIsize = AtomicIsize::new(0);

fn overlay_message_loop(
    config: SharedConfig,
    event_tx: tokio::sync::mpsc::Sender<AppEvent>,
    mut vm_rx: tokio::sync::watch::Receiver<ViewModel>,
    hwnd_tx: std_mpsc::Sender<SendHwnd>,
) {
    let hinstance: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap().into() };
    
    let hud_class = to_wide("LOL_HUD_CLASS");
    let mut wc = WNDCLASSW::default();
    wc.lpfnWndProc = Some(hud_wnd_proc);
    wc.hInstance = hinstance;
    wc.lpszClassName = PCWSTR(hud_class.as_ptr());
    wc.hCursor = unsafe { LoadCursorW(None, IDC_ARROW).unwrap() };
    unsafe { RegisterClassW(&wc); }

    let bench_class = to_wide("LOL_BENCH_CLASS");
    wc.lpfnWndProc = Some(bench_wnd_proc);
    wc.lpszClassName = PCWSTR(bench_class.as_ptr());
    unsafe { RegisterClassW(&wc); }

    let tray_class = to_wide("LOL_TRAY_CLASS");
    wc.lpfnWndProc = Some(tray_wnd_proc);
    wc.lpszClassName = PCWSTR(tray_class.as_ptr());
    unsafe { RegisterClassW(&wc); }

    let hud_hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            PCWSTR(hud_class.as_ptr()), PCWSTR(to_wide("LOL_HUD").as_ptr()),
            WS_POPUP, 0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN),
            None, None, hinstance, None
        ).unwrap()
    };

    let bench_hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            PCWSTR(bench_class.as_ptr()), PCWSTR(to_wide("LOL_BENCH").as_ptr()),
            WS_POPUP, 0, 0, 1, 1, None, None, hinstance, None
        ).unwrap()
    };

    let tray_hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE, PCWSTR(tray_class.as_ptr()), PCWSTR(to_wide("LOL_TRAY").as_ptr()),
            WS_POPUP, 0, 0, 0, 0, None, None, hinstance, None
        ).unwrap()
    };

    let state = Box::into_raw(Box::new(WndState {
        vm: ViewModel::default(),
        config: config.clone(),
        event_tx: event_tx.clone(),
        win_w: 1, win_h: 1,
    }));

    unsafe {
        SetWindowLongPtrW(hud_hwnd, GWLP_USERDATA, state as isize);
        SetWindowLongPtrW(bench_hwnd, GWLP_USERDATA, state as isize);
        SetWindowLongPtrW(tray_hwnd, GWLP_USERDATA, state as isize);
        
        add_tray_icon(tray_hwnd);

        UI_HWND.store(hud_hwnd.0 as isize, Ordering::SeqCst);
        let event_tx_hook = event_tx.clone();
        thread::Builder::new().name("keyboard-hook".to_owned()).spawn(move || {
            keyboard_hook_loop(event_tx_hook);
        }).expect("启动监控线程失败");
    }
    let _ = hwnd_tx.send(SendHwnd(hud_hwnd));

    let mut last_sync = Instant::now();
    let mut force_sync = true; 
    let mut needs_paint_hud = false;
    let mut needs_paint_bench = false;

    unsafe { SetTimer(hud_hwnd, 1, 30, None); }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        if msg.message == WM_TIMER && msg.wParam.0 == 1 {
            if vm_rx.has_changed().unwrap_or(false) {
                let new_vm = vm_rx.borrow_and_update().clone();
                let s = unsafe { &mut *state };
                
                if s.vm.hud1_visible != new_vm.hud1_visible {
                    debug!("UI: HUD1 可见性变更 -> {}", new_vm.hud1_visible);
                    unsafe { let _ = ShowWindow(hud_hwnd, if new_vm.hud1_visible { SW_SHOWNOACTIVATE } else { SW_HIDE }); }
                }
                if s.vm.hud2_visible != new_vm.hud2_visible {
                    debug!("UI: HUD2 可见性变更 -> {}", new_vm.hud2_visible);
                    unsafe { let _ = ShowWindow(bench_hwnd, if new_vm.hud2_visible { SW_SHOWNOACTIVATE } else { SW_HIDE }); }
                }
                
                if s.vm.hud1_lines != new_vm.hud1_lines || s.vm.hud1_title != new_vm.hud1_title {
                    needs_paint_hud = true;
                }
                if s.vm.hud2_selected_slot != new_vm.hud2_selected_slot {
                    needs_paint_bench = true;
                }
                
                s.vm = new_vm;
                force_sync = true;
            }

            if Instant::now().duration_since(last_sync) >= Duration::from_millis(150) || force_sync {
                last_sync = Instant::now();
                let s = unsafe { &mut *state };
                
                if s.vm.lcu_rect.width > 0 {
                    let nw = s.vm.lcu_rect.width;
                    let nh = s.vm.lcu_rect.height;
                    
                    if nw != s.win_w || nh != s.win_h || force_sync {
                        s.win_w = nw;
                        s.win_h = nh;
                        needs_paint_hud = true;
                        needs_paint_bench = true;
                        
                        let r = RECT { left: s.vm.lcu_rect.x, top: s.vm.lcu_rect.y, right: s.vm.lcu_rect.x + nw, bottom: s.vm.lcu_rect.y + nh };
                        if s.vm.hud2_visible {
                            let bench_r = get_bench_container_rect(nw, nh);
                            let bx = r.left + bench_r.x.round() as i32;
                            let by = r.top + bench_r.y.round() as i32;
                            let bw = bench_r.w.round() as i32;
                            let bh = bench_r.h.round() as i32;
                            unsafe {
                                let _ = SetWindowPos(bench_hwnd, HWND_TOPMOST, bx, by, bw, bh, SWP_NOACTIVATE);
                            }
                        }
                    }
                }
                force_sync = false;
            }

            if needs_paint_hud && unsafe { (*state).vm.hud1_visible } {
                unsafe { paint_hud(hud_hwnd, &*state); }
                needs_paint_hud = false;
            }
            if needs_paint_bench && unsafe { (*state).vm.hud2_visible } {
                unsafe { paint_bench(bench_hwnd, &*state); }
                needs_paint_bench = false;
            }
        }
    }
    info!("UI 消息循环已退出");
}

unsafe fn add_tray_icon(hwnd: HWND) {
    let hinstance: HINSTANCE = GetModuleHandleW(None).unwrap().into();
    let hicon = LoadImageW(hinstance, PCWSTR(1 as _), IMAGE_ICON, GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), LR_DEFAULTCOLOR).map(|h| HICON(h.0))
        .unwrap_or_else(|_| LoadIconW(HINSTANCE::default(), IDI_APPLICATION).unwrap());
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: TRAY_UID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP, uCallbackMessage: WM_TRAY_ICON, hIcon: hicon, ..Default::default()
    };
    let tip = to_wide("LOL_LCU 助手");
    let len = tip.len().min(nid.szTip.len() - 1);
    nid.szTip[..len].copy_from_slice(&tip[..len]);
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    info!("系统托盘图标已添加");
}

static mut HOOK_TX: Option<tokio::sync::mpsc::Sender<AppEvent>> = None;

fn keyboard_hook_loop(event_tx: tokio::sync::mpsc::Sender<AppEvent>) {
    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(None).unwrap().into();
        info!("安装底层键盘钩子...");
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), hinstance, 0).unwrap();
        KEYBOARD_HOOK.store(hook.0 as isize, Ordering::SeqCst);
        
        HOOK_TX = Some(event_tx);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
        info!("键盘钩子已卸载");
    }
}

unsafe extern "system" fn low_level_keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && wparam.0 as u32 == WM_KEYDOWN {
        let kbd = *(lparam.0 as *const KBDLLHOOKSTRUCT);
        if kbd.vkCode == VK_F1.0 as u32 {
            if let Some(ref tx) = HOOK_TX {
                let _ = tx.try_send(AppEvent::HotKeyF1);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}
