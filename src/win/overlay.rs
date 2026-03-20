//! Overlay 窗口与系统托盘管理
//! 
//! 修正：
//! 1. 采用 2 像素加厚描边。
//! 2. 优化 Alpha 通道修复算法，确保“近黑色”描边在分层窗口中清晰可见。
//! 3. 调整字体品质以适应描边算法。

use std::ffi::OsStr;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::thread;
use std::sync::mpsc as std_mpsc;

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
    hwnd: SendHwnd,
}

impl OverlaySender {
    pub async fn send(&self, cmd: OverlayCmd) -> Result<(), tokio::sync::mpsc::error::SendError<OverlayCmd>> {
        let tx = self.tx.clone();
        tx.send(cmd).await?;
        unsafe { let _ = PostMessageW(self.hwnd.0, WM_CMD_WAKEUP, WPARAM(0), LPARAM(0)); }
        Ok(())
    }
}

// ── 启动函数 ─────────────────────────────────────────────────────

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

    let hwnd = hwnd_rx.recv().expect("无法获取 Overlay 窗口句柄");
    OverlaySender { tx: cmd_tx, hwnd }
}

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
    let _ = GetClientRect(hwnd, &mut rect);
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
        ANTIALIASED_QUALITY.0 as u32, // 使用标准抗锯齿，配合描边效果更好
        (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32,
        windows::core::w!("Microsoft YaHei"),
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

    // ── Alpha 修复逻辑 ───────────────────────────────────────
    let pixels = std::slice::from_raw_parts_mut(bits_ptr as *mut u8, (win_w * win_h * 4) as usize);
    for chunk in pixels.chunks_exact_mut(4) {
        // 如果 RGB 任意分量 > 0，则视为文字/描边像素，强制不透明
        if chunk[0] > 0 || chunk[1] > 0 || chunk[2] > 0 {
            chunk[3] = 255;
        }
    }

    let pt_src = POINT { x: 0, y: 0 };
    let size_dst = SIZE { cx: win_w, cy: win_h };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let _ = UpdateLayeredWindow(
        hwnd, hdc_screen, None, Some(&size_dst),
        hdc_mem, Some(&pt_src), COLORREF(0), Some(&blend), ULW_ALPHA
    );

    SelectObject(hdc_mem, old_font);
    let _ = DeleteObject(hfont);
    SelectObject(hdc_mem, old_bm);
    let _ = DeleteObject(hbm);
    let _ = DeleteDC(hdc_mem);
    ReleaseDC(HWND::default(), hdc_screen);
}

/// 绘制描边文字
unsafe fn draw_stroked_text(hdc: HDC, text: &str, x: i32, y: i32, color: COLORREF) {
    let wide_text = to_wide(text);
    
    // 使用较深的灰色 (10,10,10) 作为描边，增加厚度至 2 像素
    SetTextColor(hdc, rgb(10, 10, 10)); 
    for dx in -2i32..=2i32 {
        for dy in -2i32..=2i32 {
            if dx == 0 && dy == 0 { continue; }
            // 略过四个极角，使描边更圆滑
            if dx.abs() == 2 && dy.abs() == 2 { continue; }
            let _ = TextOutW(hdc, x + dx, y + dy, &wide_text);
        }
    }

    // 主体文字绘制在最上层
    SetTextColor(hdc, color);
    let _ = TextOutW(hdc, x, y, &wide_text);
}

// ── 窗口过程 ─────────────────────────────────────────────────────

unsafe extern "system" fn overlay_wnd_proc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM
) -> LRESULT {
    match msg {
        WM_TRAY_ICON => {
            let event = (lparam.0 as u32) & 0xFFFF;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                show_tray_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_CMD_WAKEUP => LRESULT(0),
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WndState;
    if ptr.is_null() { return; }
    let state = &*ptr;

    let hmenu = CreatePopupMenu().unwrap();
    let _ = AppendMenuW(hmenu, MF_STRING, ID_PLAY_AGAIN, windows::core::w!("退出结算页面"));
    let _ = AppendMenuW(hmenu, MF_STRING, ID_RELOAD_UX, windows::core::w!("热重载客户端"));
    let _ = AppendMenuW(hmenu, MF_STRING, ID_AUTO_LOOT, windows::core::w!("领取任务与宝箱"));
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, None);

    {
        let config = state.config.lock();
        let add_item = |menu, id, text, checked| {
            let mut flags = MF_STRING;
            if checked { flags |= MF_CHECKED; }
            let _ = AppendMenuW(menu, flags, id, text);
        };

        add_item(hmenu, ID_AUTO_ACCEPT, windows::core::w!("自动接受对局"), config.auto_accept_enabled);
        add_item(hmenu, ID_AUTO_HONOR, windows::core::w!("自动点赞跳过"), config.auto_honor_skip);
        add_item(hmenu, ID_PREMADE_CHAMP, windows::core::w!("选人阶段组队分析"), config.premade_champ_select);
        add_item(hmenu, ID_MEMORY_MONITOR, windows::core::w!("内存监控自动重载"), config.memory_monitor);
    }
    
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(hmenu, MF_STRING, ID_QUIT, windows::core::w!("退出程序"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    
    let cmd = TrackPopupMenu(hmenu, TPM_RIGHTBUTTON | TPM_RETURNCMD, pt.x, pt.y, 0, hwnd, None);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(hmenu);

    if cmd.0 != 0 {
        handle_menu_command(hwnd, cmd.0 as usize);
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

// ── 消息循环 ─────────────────────────────────────────────────────

fn overlay_message_loop(
    mut cmd_rx: tokio::sync::mpsc::Receiver<OverlayCmd>,
    hwnd_tx: std_mpsc::Sender<SendHwnd>,
    action_tx: tokio::sync::mpsc::Sender<TrayAction>,
    config: SharedConfig,
) {
    let hinstance = unsafe { GetModuleHandleW(None).unwrap() };
    let wnd_class = to_wide("LOL_LCU_HUD");

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(overlay_wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: windows::core::PCWSTR(wnd_class.as_ptr()),
        hbrBackground: HBRUSH::default(),
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc); }

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            windows::core::PCWSTR(wnd_class.as_ptr()),
            windows::core::w!("LOL_LCU_HUD"),
            WS_POPUP,
            0, 0, 1200, 900,
            None, None, hinstance, None
        ).unwrap()
    };

    let state_ptr = Box::into_raw(Box::new(WndState {
        connection: "等待连接...".to_owned(),
        premade: String::new(),
        config,
        action_tx,
    }));
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize); }

    unsafe {
        paint_overlay(hwnd, &*state_ptr);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        add_tray_icon(hwnd);
    }

    let _ = hwnd_tx.send(SendHwnd(hwnd));

    loop {
        let mut msg = MSG::default();
        if unsafe { GetMessageW(&mut msg, None, 0, 0).as_bool() } {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        } else {
            return;
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
            unsafe { paint_overlay(hwnd, &*state_ptr); }
        }
    }
}

unsafe fn add_tray_icon(hwnd: HWND) {
    let hicon = LoadIconW(HINSTANCE::default(), IDI_APPLICATION).unwrap();
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
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(once(0)).collect()
}
