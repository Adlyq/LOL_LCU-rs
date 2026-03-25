//! 系统托盘与菜单管理

use tracing::info;
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::event::{AppEvent, TrayAction};
use crate::win::winapi::to_wide;

pub const WM_TRAY_ICON: u32 = WM_USER + 100;
pub const TRAY_UID: u32 = 1;

pub const ID_QUIT: usize = 1001;
pub const ID_RELOAD_UX: usize = 1002;
pub const ID_PLAY_AGAIN: usize = 1003;
pub const ID_FIND_LOOT: usize = 1004;
pub const ID_FIX_WINDOW: usize = 1005;

pub const ID_AUTO_ACCEPT: usize = 2001;
pub const ID_AUTO_HONOR: usize = 2002;
pub const ID_PREMADE_CHAMP: usize = 2003;
pub const ID_MEMORY_MONITOR: usize = 2004;

pub unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_TRAY_ICON {
        let event = (lparam.0 as u32) & 0xFFFF;
        if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
            show_tray_menu(hwnd);
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

pub unsafe fn show_tray_menu(tray_hwnd: HWND) {
    use crate::win::overlay::WndState;
    let ptr = GetWindowLongPtrW(tray_hwnd, GWLP_USERDATA) as *const WndState;
    if ptr.is_null() {
        return;
    }
    let state = &*ptr;

    let hmenu = CreatePopupMenu().expect("无法创建弹出菜单");

    let _ = AppendMenuW(
        hmenu,
        MF_STRING,
        ID_FIX_WINDOW,
        PCWSTR(to_wide("修复客户端窗口比例").as_ptr()),
    );
    let _ = AppendMenuW(
        hmenu,
        MF_STRING,
        ID_FIND_LOOT,
        PCWSTR(to_wide("找回一些遗忘的东西").as_ptr()),
    );
    let _ = AppendMenuW(
        hmenu,
        MF_STRING,
        ID_PLAY_AGAIN,
        PCWSTR(to_wide("退出结算页面").as_ptr()),
    );
    let _ = AppendMenuW(
        hmenu,
        MF_STRING,
        ID_RELOAD_UX,
        PCWSTR(to_wide("热重载客户端").as_ptr()),
    );
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());

    {
        let config = state.config.lock();
        let add_check_item = |id, text: &str, checked: bool| {
            let mut flags = MF_STRING;
            if checked {
                flags |= MF_CHECKED;
            }
            let title = to_wide(text);
            let _ = AppendMenuW(hmenu, flags, id, PCWSTR(title.as_ptr()));
        };
        add_check_item(ID_AUTO_ACCEPT, "自动接受对局", config.auto_accept_enabled);
        add_check_item(ID_AUTO_HONOR, "自动点赞跳过", config.auto_honor_skip);
        add_check_item(
            ID_PREMADE_CHAMP,
            "选人阶段组队分析",
            config.premade_champ_select,
        );
        add_check_item(ID_MEMORY_MONITOR, "内存监控自动重载", config.memory_monitor);
    }

    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
    let quit_title = to_wide("退出程序");
    let _ = AppendMenuW(hmenu, MF_STRING, ID_QUIT, PCWSTR(quit_title.as_ptr()));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(tray_hwnd);

    let cmd = TrackPopupMenu(
        hmenu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        tray_hwnd,
        None,
    );
    let _ = PostMessageW(tray_hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(hmenu);

    if cmd.0 != 0 {
        handle_menu_command(state, cmd.0 as usize);
    }
}

unsafe fn handle_menu_command(state: &crate::win::overlay::WndState, id: usize) {
    match id {
        ID_FIX_WINDOW => {
            let _ = state
                .event_tx
                .try_send(AppEvent::TrayAction(TrayAction::FixWindow));
        }
        ID_FIND_LOOT => {
            let _ = state
                .event_tx
                .try_send(AppEvent::TrayAction(TrayAction::FindForgottenLoot));
        }
        ID_PLAY_AGAIN => {
            let _ = state
                .event_tx
                .try_send(AppEvent::TrayAction(TrayAction::PlayAgain));
        }
        ID_RELOAD_UX => {
            let _ = state
                .event_tx
                .try_send(AppEvent::TrayAction(TrayAction::ReloadUx));
        }
        ID_AUTO_ACCEPT => {
            let _ = state
                .event_tx
                .try_send(AppEvent::TrayAction(TrayAction::ToggleAutoAccept));
        }
        ID_AUTO_HONOR => {
            let _ = state
                .event_tx
                .try_send(AppEvent::TrayAction(TrayAction::ToggleAutoHonor));
        }
        ID_PREMADE_CHAMP => {
            let _ = state
                .event_tx
                .try_send(AppEvent::TrayAction(TrayAction::TogglePremadeChamp));
        }
        ID_MEMORY_MONITOR => {
            let _ = state
                .event_tx
                .try_send(AppEvent::TrayAction(TrayAction::ToggleMemoryMonitor));
        }
        ID_QUIT => {
            let _ = state
                .event_tx
                .try_send(AppEvent::TrayAction(TrayAction::Exit));
        }
        _ => {}
    }
}

pub unsafe fn add_tray_icon(hwnd: HWND) {
    let hinstance: HINSTANCE = GetModuleHandleW(None).unwrap().into();
    let hicon = LoadImageW(
        hinstance,
        PCWSTR(1 as _),
        IMAGE_ICON,
        GetSystemMetrics(SM_CXSMICON),
        GetSystemMetrics(SM_CYSMICON),
        LR_DEFAULTCOLOR,
    )
    .map(|h| HICON(h.0))
    .unwrap_or_else(|_| LoadIconW(HINSTANCE::default(), IDI_APPLICATION).unwrap());

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
    info!("系统托盘图标已添加");
}
