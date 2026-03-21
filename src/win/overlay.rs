//! Overlay 窗口与系统托盘管理
//! 
//! 修正：
//! 1. 独立托盘窗口与 HUD 窗口，互不干扰。
//! 2. 启用 DPI 感知 (Per-monitor V2)。
//! 3. 托盘图标加载逻辑优化。

use std::ffi::OsStr;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::thread;
use std::sync::mpsc as std_mpsc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::UI::Shell::*;

use crate::app::config::SharedConfig;

// ── 常量定义 ─────────────────────────────────────────────────────

const WM_TRAY_ICON: u32 = WM_USER + 100;
const WM_CMD_WAKEUP: u32 = WM_USER + 101;
const TRAY_UID: u32 = 1;

const ID_QUIT: usize = 1001;
const ID_RELOAD_UX: usize = 1002;
const ID_PLAY_AGAIN: usize = 1003;
const ID_AUTO_LOOT: usize = 1004;

const ID_AUTO_ACCEPT: usize = 2001;
const ID_AUTO_HONOR: usize = 2002;
const ID_PREMADE_CHAMP: usize = 2003;
const ID_MEMORY_MONITOR: usize = 2004;

// ── 线程安全包装 ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct SendHwnd(HWND);
unsafe impl Send for SendHwnd {}
unsafe impl Sync for SendHwnd {}

// ── 指令类型 ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum OverlayCmd {
    UpdateHud(String, String),
    Quit,
}

#[derive(Debug, Clone)]
pub enum TrayAction {
    ReloadUx,
    PlayAgain,
    AutoLoot,
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
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

// ── 绘制逻辑 ─────────────────────────────────────────────────────

unsafe fn paint_overlay(hwnd: HWND, state: &WndState) {
    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    let win_w = rect.right - rect.left;
    let win_h = rect.bottom - rect.top;

    if win_w <= 0 || win_h <= 0 { return; }

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
    let hbm = CreateDIBSection(hdc_mem, &BITMAPINFO { bmiHeader: bi, ..Default::default() }, DIB_RGB_COLORS, &mut bits_ptr, HANDLE::default(), 0).unwrap();
    let old_bm = SelectObject(hdc_mem, hbm);

    std::ptr::write_bytes(bits_ptr, 0, (win_w * win_h * 4) as usize);

    let hfont = CreateFontW(
        22, 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        ANTIALIASED_QUALITY.0 as u32,
        (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32,
        PCWSTR(to_wide("Microsoft YaHei").as_ptr()),
    );
    let old_font = SelectObject(hdc_mem, hfont);

    SetBkMode(hdc_mem, TRANSPARENT);

    let mut y = 40;
    let x = 10;

    if !state.connection.is_empty() {
        draw_stroked_text(hdc_mem, &state.connection, x, y, rgb(0, 255, 0));
        y += 32;
    }

    if !state.premade.is_empty() {
        for line in state.premade.lines() {
            draw_stroked_text(hdc_mem, line, x, y, rgb(255, 255, 255));
            y += 26;
        }
    }

    let pixels = std::slice::from_raw_parts_mut(bits_ptr as *mut u8, (win_w * win_h * 4) as usize);
    for chunk in pixels.chunks_exact_mut(4) {
        if chunk[0] > 0 || chunk[1] > 0 || chunk[2] > 0 {
            chunk[3] = 255;
        }
    }

    let pt_dst = POINT { x: rect.left, y: rect.top };
    let pt_src = POINT { x: 0, y: 0 };
    let size_dst = SIZE { cx: win_w, cy: win_h };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let _ = UpdateLayeredWindow(
        hwnd, hdc_screen, Some(&pt_dst), Some(&size_dst),
        hdc_mem, Some(&pt_src), COLORREF(0), Some(&blend), ULW_ALPHA
    );

    SelectObject(hdc_mem, old_font);
    let _ = DeleteObject(hfont);
    SelectObject(hdc_mem, old_bm);
    let _ = DeleteObject(hbm);
    let _ = DeleteDC(hdc_mem);
    ReleaseDC(HWND::default(), hdc_screen);
}

unsafe fn draw_stroked_text(hdc: HDC, text: &str, x: i32, y: i32, color: COLORREF) {
    let wide_text = to_wide(text);
    SetTextColor(hdc, rgb(10, 10, 10)); 
    for dx in -2i32..=2i32 {
        for dy in -2i32..=2i32 {
            if dx == 0 && dy == 0 { continue; }
            if dx.abs() == 2 && dy.abs() == 2 { continue; }
            let _ = TextOutW(hdc, x + dx, y + dy, &wide_text);
        }
    }
    SetTextColor(hdc, color);
    let _ = TextOutW(hdc, x, y, &wide_text);
}

// ── 窗口过程 ─────────────────────────────────────────────────────

unsafe extern "system" fn tray_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY_ICON => {
            let event = (lparam.0 as u32) & 0xFFFF;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                show_tray_menu(hwnd);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn hud_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CMD_WAKEUP => LRESULT(0),
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn show_tray_menu(tray_hwnd: HWND) {
    let ptr = GetWindowLongPtrW(tray_hwnd, GWLP_USERDATA) as *const WndState;
    if ptr.is_null() { return; }
    let state = &*ptr;

    let hmenu = CreatePopupMenu().unwrap();
    let _ = AppendMenuW(hmenu, MF_STRING, ID_PLAY_AGAIN, PCWSTR(to_wide("退出结算页面").as_ptr()));
    let _ = AppendMenuW(hmenu, MF_STRING, ID_RELOAD_UX, PCWSTR(to_wide("热重载客户端").as_ptr()));
    let _ = AppendMenuW(hmenu, MF_STRING, ID_AUTO_LOOT, PCWSTR(to_wide("领取任务与宝箱").as_ptr()));
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());

    {
        let config = state.config.lock();
        let add_item = |menu, id, text: &str, checked| {
            let mut flags = MF_STRING;
            if checked { flags |= MF_CHECKED; }
            let _ = AppendMenuW(menu, flags, id, PCWSTR(to_wide(text).as_ptr()));
        };
        add_item(hmenu, ID_AUTO_ACCEPT, "自动接受对局", config.auto_accept_enabled);
        add_item(hmenu, ID_AUTO_HONOR, "自动点赞跳过", config.auto_honor_skip);
        add_item(hmenu, ID_PREMADE_CHAMP, "选人阶段组队分析", config.premade_champ_select);
        add_item(hmenu, ID_MEMORY_MONITOR, "内存监控自动重载", config.memory_monitor);
    }
    
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(hmenu, MF_STRING, ID_QUIT, PCWSTR(to_wide("退出程序").as_ptr()));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(tray_hwnd);
    
    let cmd = TrackPopupMenu(hmenu, TPM_RIGHTBUTTON | TPM_RETURNCMD, pt.x, pt.y, 0, tray_hwnd, None);
    let _ = PostMessageW(tray_hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(hmenu);

    if cmd.0 != 0 {
        handle_menu_command(tray_hwnd, cmd.0 as usize);
    }
}

unsafe fn handle_menu_command(hwnd: HWND, id: usize) {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WndState;
    if ptr.is_null() { return; }
    let state = &*ptr;

    match id {
        ID_PLAY_AGAIN => { let _ = state.action_tx.try_send(TrayAction::PlayAgain); }
        ID_RELOAD_UX => { let _ = state.action_tx.try_send(TrayAction::ReloadUx); }
        ID_AUTO_LOOT => { let _ = state.action_tx.try_send(TrayAction::AutoLoot); }
        ID_AUTO_ACCEPT => { let mut c = state.config.lock(); c.auto_accept_enabled = !c.auto_accept_enabled; c.save(); }
        ID_AUTO_HONOR => { let mut c = state.config.lock(); c.auto_honor_skip = !c.auto_honor_skip; c.save(); }
        ID_PREMADE_CHAMP => { let mut c = state.config.lock(); c.premade_champ_select = !c.premade_champ_select; c.save(); }
        ID_MEMORY_MONITOR => { let mut c = state.config.lock(); c.memory_monitor = !c.memory_monitor; c.save(); }
        ID_QUIT => { std::process::exit(0); }
        _ => {}
    }
}

// ── 启动与消息循环 ─────────────────────────────────────────────────────

pub fn spawn_overlay_thread(
    config: SharedConfig,
    action_tx: tokio::sync::mpsc::Sender<TrayAction>,
) -> OverlaySender {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<OverlayCmd>(256);
    let (hwnd_tx, hwnd_rx) = std_mpsc::channel::<SendHwnd>();

    thread::Builder::new()
        .name("overlay-win32".to_owned())
        .spawn(move || {
            overlay_message_loop(cmd_rx, hwnd_tx, action_tx, config);
        })
        .expect("启动 overlay 线程失败");

    let hud_hwnd = hwnd_rx.recv().expect("无法获取 HUD 窗口句柄");
    OverlaySender { tx: cmd_tx, hud_hwnd }
}

fn overlay_message_loop(
    mut cmd_rx: tokio::sync::mpsc::Receiver<OverlayCmd>,
    hwnd_tx: std_mpsc::Sender<SendHwnd>,
    action_tx: tokio::sync::mpsc::Sender<TrayAction>,
    config: SharedConfig,
) {
    let hinstance = unsafe { GetModuleHandleW(None).unwrap() };

    // 1. 注册 HUD 类
    let hud_class = to_wide("LOL_LCU_HUD");
    let hud_wc = WNDCLASSW {
        lpfnWndProc: Some(hud_wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: PCWSTR(hud_class.as_ptr()),
        ..Default::default()
    };
    unsafe { RegisterClassW(&hud_wc); }

    // 2. 注册 Tray 类 (不可见)
    let tray_class = to_wide("LOL_LCU_TRAY");
    let tray_wc = WNDCLASSW {
        lpfnWndProc: Some(tray_wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: PCWSTR(tray_class.as_ptr()),
        ..Default::default()
    };
    unsafe { RegisterClassW(&tray_wc); }

    // 3. 创建 HUD 窗口
    let hud_hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            PCWSTR(hud_class.as_ptr()),
            PCWSTR(hud_class.as_ptr()),
            WS_POPUP,
            0, 0, 1200, 900,
            None, None, hinstance, None
        ).unwrap()
    };

    // 4. 创建 Tray 窗口 (不可见)
    let tray_hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            PCWSTR(tray_class.as_ptr()),
            PCWSTR(tray_class.as_ptr()),
            Default::default(),
            0, 0, 0, 0,
            HWND_MESSAGE, None, hinstance, None
        ).unwrap()
    };

    let state = Box::new(WndState {
        connection: "等待连接...".to_owned(),
        premade: String::new(),
        config,
        action_tx,
    });
    let state_ptr = Box::into_raw(state);
    
    unsafe {
        SetWindowLongPtrW(hud_hwnd, GWLP_USERDATA, state_ptr as isize);
        SetWindowLongPtrW(tray_hwnd, GWLP_USERDATA, state_ptr as isize);
        
        paint_overlay(hud_hwnd, &*state_ptr);
        let _ = ShowWindow(hud_hwnd, SW_SHOWNOACTIVATE);
        add_tray_icon(tray_hwnd);
    }

    let _ = hwnd_tx.send(SendHwnd(hud_hwnd));

    loop {
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() } {
            if msg.message == WM_QUIT { return; }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        let mut updated = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                OverlayCmd::UpdateHud(conn, prem) => {
                    let s = unsafe { &mut *state_ptr };
                    if !conn.is_empty() { s.connection = conn; }
                    s.premade = prem;
                    updated = true;
                }
                OverlayCmd::Quit => return,
            }
        }

        if updated {
            unsafe { paint_overlay(hud_hwnd, &*state_ptr); }
        }
        
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

unsafe fn add_tray_icon(hwnd: HWND) {
    let hinstance = GetModuleHandleW(None).unwrap();
    let hicon = LoadImageW(
        hinstance,
        PCWSTR(1 as _),
        IMAGE_ICON,
        GetSystemMetrics(SM_CXSMICON),
        GetSystemMetrics(SM_CYSMICON),
        LR_DEFAULTCOLOR
    ).map(|h| HICON(h.0)).unwrap_or_else(|_| {
        LoadIconW(HINSTANCE::default(), IDI_APPLICATION).unwrap()
    });

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY_ICON,
        hIcon: hicon,
        ..Default::default()
    };
    let tip = to_wide("LOL_LCU 助手");
    let len = tip.len().min(nid.szTip.len() - 1);
    nid.szTip[..len].copy_from_slice(&tip[..len]);
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(once(0)).collect()
}
