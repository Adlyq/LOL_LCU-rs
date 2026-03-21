//! Overlay 窗口与系统托盘管理
//! 
//! 架构设计：
//! 1. HUD 窗口：显示状态信息，锁定屏幕 (0,0)，70% 不透明度，全点击穿透。
//! 2. Bench 窗口：大乱斗板凳席交互，固定 10 席位，透明背景，跟随 LCU 窗口。
//! 3. Tray 窗口：独立消息窗口，处理托盘与菜单。

use std::thread;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::UI::Shell::*;

use crate::app::config::SharedConfig;
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
const WM_CMD_WAKEUP: u32 = WM_USER + 101;
const TRAY_UID: u32 = 1;

const ID_QUIT: usize = 1001;
const ID_RELOAD_UX: usize = 1002;
const ID_PLAY_AGAIN: usize = 1003;

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

// ── 指令类型 ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum OverlayCmd {
    UpdateHud(String, String),
    ShowBench(bool),          
    SetSelectedSlot(usize),   
    ClearSelectedSlot,        
    AutoFixWindow(f64),       
    Quit,
}

#[derive(Debug, Clone)]
pub enum TrayAction {
    ReloadUx,
    PlayAgain,
}

#[derive(Clone)]
pub struct OverlaySender {
    tx: tokio::sync::mpsc::Sender<OverlayCmd>,
    hud_hwnd: SendHwnd,
}

impl OverlaySender {
    pub async fn send(&self, cmd: OverlayCmd) -> Result<(), tokio::sync::mpsc::error::SendError<OverlayCmd>> {
        let tx = self.tx.clone();
        tx.send(cmd).await?;
        unsafe { let _ = PostMessageW(self.hud_hwnd.0, WM_CMD_WAKEUP, WPARAM(0), LPARAM(0)); }
        Ok(())
    }
}

// ── 状态结构 ─────────────────────────────────────────────────────

struct WndState {
    connection: String,
    premade: String,
    config: SharedConfig,
    action_tx: tokio::sync::mpsc::Sender<TrayAction>,
    click_tx: tokio::sync::mpsc::Sender<usize>,
    
    show_bench: bool,
    selected_slot: Option<usize>,
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

fn get_slot_rect(index: usize, count: usize, container: FRect, win_w: i32, win_h: i32) -> FRect {
    let scale_x = win_w as f64 / TEMPLATE_W;
    let scale_y = win_h as f64 / TEMPLATE_H;
    let scale = f64::min(scale_x, scale_y);
    let slot_w = SLOT_SIZE * scale;
    let slot_h = SLOT_SIZE * scale;
    let edge_inset = f64::max(0.0, 1.5 * scale);
    let avail_w = f64::max(1.0, container.w - 2.0 * edge_inset);
    let gap = if count <= 1 { 0.0 } else {
        f64::max(0.0, (avail_w - slot_w * count as f64) / (count - 1) as f64)
    };
    let x = container.x + edge_inset + index as f64 * (slot_w + gap);
    let y = container.y + (container.h - slot_h) / 2.0;
    FRect { x, y, w: slot_w, h: slot_h }
}

fn hit_slot(px: f64, py: f64, state: &WndState) -> Option<usize> {
    if !state.show_bench { return None; }
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
    let hfont = CreateFontW(
        22, 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 
        DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32, 
        CLIP_DEFAULT_PRECIS.0 as u32, ANTIALIASED_QUALITY.0 as u32, 
        (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32, PCWSTR(face_name.as_ptr())
    );
    let old_font = SelectObject(hdc_mem, hfont);
    SetBkMode(hdc_mem, TRANSPARENT);
    
    let mut y = 40; 
    let x = 10;
    if !state.connection.is_empty() { draw_stroked_text(hdc_mem, &state.connection, x, y, rgb(0, 255, 0)); y += 32; }
    if !state.premade.is_empty() {
        for line in state.premade.lines() { draw_stroked_text(hdc_mem, line, x, y, rgb(255, 255, 255)); y += 26; }
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
    SelectObject(hdc_mem, old_bm);
    let _ = DeleteObject(hbm);
    let _ = DeleteDC(hdc_mem);
    ReleaseDC(HWND::default(), hdc_screen);
}

unsafe fn paint_bench(hwnd: HWND, state: &WndState) {
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
    
    // 初始化全透明
    std::ptr::write_bytes(bits_ptr, 0, (win_w * win_h * 4) as usize);
    let pixels = std::slice::from_raw_parts_mut(bits_ptr as *mut u32, (win_w * win_h) as usize);

    if state.show_bench {
        let container = get_bench_container_rect(win_w, win_h);
        let scale_y = win_h as f64 / TEMPLATE_H;
        
        // 1. 填充容器背景 (Alpha=2, 近似透明但可捕获鼠标)
        fill_rect_alpha(pixels, win_w, win_h, container, 0, 0, 0, 2);
        
        // 2. 绘制容器边框
        let pen_gray = CreatePen(PS_SOLID, 1, rgb(128, 128, 128));
        let old_pen = SelectObject(hdc_mem, pen_gray);
        let old_brush = SelectObject(hdc_mem, GetStockObject(NULL_BRUSH));
        let _ = round_rect(hdc_mem, container, (5.0 * scale_y) as i32);
        
        // 3. 绘制槽位
        let pen_slot = CreatePen(PS_SOLID, 1, rgb(160, 160, 160));
        SelectObject(hdc_mem, pen_slot);
        for i in 0..BENCH_SLOT_COUNT {
            let sr = get_slot_rect(i, BENCH_SLOT_COUNT, container, win_w, win_h);
            // 选中槽位填充绿色遮罩，否则填充 Alpha=2 以捕获鼠标
            if state.selected_slot == Some(i) {
                fill_rect_alpha(pixels, win_w, win_h, sr, 130, 255, 130, 65);
            } else {
                fill_rect_alpha(pixels, win_w, win_h, sr, 0, 0, 0, 2);
            }
            let _ = round_rect(hdc_mem, sr, (4.0 * scale_y) as i32);
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

/// 填充矩形区域的 Alpha 值（使用预乘颜色）。
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

unsafe fn round_rect(hdc: HDC, r: FRect, rad: i32) -> BOOL {
    RoundRect(hdc, r.x as i32, r.y as i32, r.right() as i32, r.bottom() as i32, rad * 2, rad * 2)
}

unsafe fn draw_stroked_text(hdc: HDC, text: &str, x: i32, y: i32, color: COLORREF) {
    let wide_text = to_wide(text);
    let slice = &wide_text[..wide_text.len() - 1]; // 排除空终止符
    SetTextColor(hdc, rgb(10, 10, 10));
    for dx in -2..=2 {
        for dy in -2..=2 {
            if dx == 0 && dy == 0 {
                continue;
            }
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
                    let _ = state.click_tx.try_send(idx);
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
        ID_PLAY_AGAIN => { let _ = state.action_tx.try_send(TrayAction::PlayAgain); }
        ID_RELOAD_UX => { let _ = state.action_tx.try_send(TrayAction::ReloadUx); }
        ID_AUTO_ACCEPT => { let mut c = state.config.lock(); c.auto_accept_enabled = !c.auto_accept_enabled; c.save(); }
        ID_AUTO_HONOR => { let mut c = state.config.lock(); c.auto_honor_skip = !c.auto_honor_skip; c.save(); }
        ID_PREMADE_CHAMP => { let mut c = state.config.lock(); c.premade_champ_select = !c.premade_champ_select; c.save(); }
        ID_MEMORY_MONITOR => { let mut c = state.config.lock(); c.memory_monitor = !c.memory_monitor; c.save(); }
        ID_QUIT => { std::process::exit(0); }
        _ => {}
    }
}

pub fn spawn_overlay_thread(
    config: SharedConfig,
    action_tx: tokio::sync::mpsc::Sender<TrayAction>,
    click_tx: tokio::sync::mpsc::Sender<usize>,
) -> OverlaySender {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<OverlayCmd>(256);
    let (hwnd_tx, hwnd_rx) = std_mpsc::channel::<SendHwnd>();
    thread::Builder::new().name("overlay-win32".to_owned()).spawn(move || {
        overlay_message_loop(cmd_rx, hwnd_tx, action_tx, click_tx, config);
    }).expect("启动 overlay 线程失败");
    let hud_hwnd = hwnd_rx.recv().expect("无法获取 HUD 窗口句柄");
    OverlaySender { tx: cmd_tx, hud_hwnd }
}

fn overlay_message_loop(
    mut cmd_rx: tokio::sync::mpsc::Receiver<OverlayCmd>,
    hwnd_tx: std_mpsc::Sender<SendHwnd>,
    action_tx: tokio::sync::mpsc::Sender<TrayAction>,
    click_tx: tokio::sync::mpsc::Sender<usize>,
    config: SharedConfig,
) {
    let hinstance = unsafe { GetModuleHandleW(None).unwrap() };
    
    let hud_class_w = to_wide("LOL_LCU_HUD");
    let hud_wc = WNDCLASSW { lpfnWndProc: Some(hud_wnd_proc), hInstance: hinstance.into(), lpszClassName: PCWSTR(hud_class_w.as_ptr()), ..Default::default() };
    unsafe { RegisterClassW(&hud_wc); }

    let bench_class_w = to_wide("LOL_LCU_BENCH");
    let bench_wc = WNDCLASSW { lpfnWndProc: Some(bench_wnd_proc), hInstance: hinstance.into(), lpszClassName: PCWSTR(bench_class_w.as_ptr()), ..Default::default() };
    unsafe { RegisterClassW(&bench_wc); }

    let tray_class_w = to_wide("LOL_LCU_TRAY");
    let tray_wc = WNDCLASSW { lpfnWndProc: Some(tray_wnd_proc), hInstance: hinstance.into(), lpszClassName: PCWSTR(tray_class_w.as_ptr()), ..Default::default() };
    unsafe { RegisterClassW(&tray_wc); }

    let hud_hwnd = unsafe {
        CreateWindowExW(WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            PCWSTR(hud_class_w.as_ptr()), PCWSTR(hud_class_w.as_ptr()), WS_POPUP,
            0, 0, 1200, 900, None, None, hinstance, None).unwrap()
    };

    let bench_hwnd = unsafe {
        CreateWindowExW(WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR(bench_class_w.as_ptr()), PCWSTR(bench_class_w.as_ptr()), WS_POPUP,
            0, 0, 1920, 1080, None, None, hinstance, None).unwrap()
    };

    let tray_hwnd = unsafe {
        CreateWindowExW(Default::default(), PCWSTR(tray_class_w.as_ptr()), PCWSTR(tray_class_w.as_ptr()),
            Default::default(), 0, 0, 0, 0, HWND_MESSAGE, None, hinstance, None).unwrap()
    };

    let state = Box::new(WndState {
        connection: "等待连接...".to_owned(), premade: String::new(), config, action_tx, click_tx,
        show_bench: false, selected_slot: None, win_w: 1920, win_h: 1080,
    });
    let state_ptr = Box::into_raw(state);
    unsafe {
        SetWindowLongPtrW(hud_hwnd, GWLP_USERDATA, state_ptr as isize);
        SetWindowLongPtrW(bench_hwnd, GWLP_USERDATA, state_ptr as isize);
        SetWindowLongPtrW(tray_hwnd, GWLP_USERDATA, state_ptr as isize);
        
        let _ = ShowWindow(hud_hwnd, SW_SHOWNOACTIVATE);
        add_tray_icon(tray_hwnd);
    }
    let _ = hwnd_tx.send(SendHwnd(hud_hwnd));

    let mut last_sync = Instant::now();
    loop {
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() } {
            if msg.message == WM_QUIT { return; }
            unsafe { let _ = TranslateMessage(&msg); DispatchMessageW(&msg); }
        }

        let mut needs_paint_hud = false;
        let mut needs_paint_bench = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            let s = unsafe { &mut *state_ptr };
            match cmd {
                OverlayCmd::UpdateHud(conn, prem) => {
                    if !conn.is_empty() { s.connection = conn; }
                    s.premade = prem; needs_paint_hud = true;
                }
                OverlayCmd::ShowBench(show) => { 
                    s.show_bench = show; needs_paint_bench = true;
                    unsafe { let _ = ShowWindow(bench_hwnd, if show { SW_SHOWNOACTIVATE } else { SW_HIDE }); }
                }
                OverlayCmd::SetSelectedSlot(idx) => {
                    s.selected_slot = Some(idx); needs_paint_bench = true;
                }
                OverlayCmd::ClearSelectedSlot => {
                    s.selected_slot = None; needs_paint_bench = true;
                }
                OverlayCmd::AutoFixWindow(zoom) => {
                    if let Some(target) = winapi::find_lcu_window() {
                        winapi::fix_lcu_window_by_zoom(target, zoom, false);
                    }
                }
                OverlayCmd::Quit => return,
            }
        }

        if Instant::now().duration_since(last_sync) >= Duration::from_millis(150) {
            last_sync = Instant::now();
            if let Some(target) = winapi::find_lcu_window() {
                if let Some(r) = winapi::get_window_rect(target) {
                    let s = unsafe { &mut *state_ptr };
                    let nw = r.right - r.left; let nh = r.bottom - r.top;
                    if nw != s.win_w || nh != s.win_h { 
                        s.win_w = nw; s.win_h = nh; 
                        needs_paint_hud = true; 
                        needs_paint_bench = true;
                    }
                    // HUD 文字固定屏幕 (0,0)，不需要 place_window_above_target
                    if s.show_bench {
                        winapi::place_window_above_target(bench_hwnd, target, &r);
                    }
                }
            }
        }

        if needs_paint_hud { unsafe { paint_hud(hud_hwnd, &*state_ptr); } }
        if needs_paint_bench { unsafe { paint_bench(bench_hwnd, &*state_ptr); } }
        
        thread::sleep(Duration::from_millis(30));
    }
}

unsafe fn add_tray_icon(hwnd: HWND) {
    let hinstance = GetModuleHandleW(None).unwrap();
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
}
