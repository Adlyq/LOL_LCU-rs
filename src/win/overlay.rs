//! Overlay 窗口管理器 (协调者)

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, trace};

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::config::SharedConfig;
use crate::app::event::AppEvent;
use crate::app::viewmodel::ViewModel;
use crate::win::base::SendHwnd;
use crate::win::winapi::to_wide;

use crate::win::hud1::{hud_wnd_proc, paint_hud};
use crate::win::hud2::{bench_wnd_proc, get_bench_container_rect, paint_bench};
use crate::win::tray::{add_tray_icon, tray_wnd_proc};

pub const WM_VM_UPDATED: u32 = WM_USER + 101;

pub struct WndState {
    pub vm: ViewModel,
    pub config: SharedConfig,
    pub event_tx: tokio::sync::mpsc::Sender<AppEvent>,
    pub win_w: i32,
    pub win_h: i32,
}

#[derive(Clone)]
pub struct OverlaySender {
    pub _tx: tokio::sync::mpsc::Sender<AppEvent>,
    pub hud_hwnd: SendHwnd,
}

impl OverlaySender {
    /// 主动唤醒 UI 线程执行更新检查与重绘
    pub fn wake_up(&self) {
        unsafe {
            let _ = PostMessageW(self.hud_hwnd.0, WM_VM_UPDATED, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn spawn_overlay_thread(
    config: SharedConfig,
    event_tx: tokio::sync::mpsc::Sender<AppEvent>,
    vm_rx: tokio::sync::watch::Receiver<ViewModel>,
) -> OverlaySender {
    let (hwnd_tx, hwnd_rx) = std_mpsc::channel();
    let event_tx_c = event_tx.clone();

    info!("启动核心 UI 线程...");
    thread::spawn(move || {
        overlay_message_loop(config, event_tx_c, vm_rx, hwnd_tx);
    });

    let hud_hwnd = hwnd_rx.recv().expect("无法获取 Overlay HWND");
    OverlaySender {
        _tx: event_tx,
        hud_hwnd,
    }
}

static UI_HWND: AtomicIsize = AtomicIsize::new(0);
static KEYBOARD_HOOK: AtomicIsize = AtomicIsize::new(0);

fn overlay_message_loop(
    config: SharedConfig,
    event_tx: tokio::sync::mpsc::Sender<AppEvent>,
    mut vm_rx: tokio::sync::watch::Receiver<ViewModel>,
    hwnd_tx: std_mpsc::Sender<SendHwnd>,
) {
    let hinstance: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap().into() };

    // 注册所有窗口类
    unsafe {
        let hud_class = to_wide("LOL_HUD_CLASS");
        let mut wc = WNDCLASSW::default();
        wc.lpfnWndProc = Some(hud_wnd_proc);
        wc.hInstance = hinstance;
        wc.lpszClassName = PCWSTR(hud_class.as_ptr());
        wc.hCursor = LoadCursorW(None, IDC_ARROW).unwrap();
        RegisterClassW(&wc);

        let bench_class = to_wide("LOL_BENCH_CLASS");
        wc.lpfnWndProc = Some(bench_wnd_proc);
        wc.lpszClassName = PCWSTR(bench_class.as_ptr());
        RegisterClassW(&wc);

        let tray_class = to_wide("LOL_TRAY_CLASS");
        wc.lpfnWndProc = Some(tray_wnd_proc);
        wc.lpszClassName = PCWSTR(tray_class.as_ptr());
        RegisterClassW(&wc);
    }

    // 创建窗口
    let hud_hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            PCWSTR(to_wide("LOL_HUD_CLASS").as_ptr()),
            PCWSTR(to_wide("LOL_HUD").as_ptr()),
            WS_POPUP,
            0,
            0,
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN),
            None,
            None,
            hinstance,
            None,
        )
        .unwrap()
    };

    let bench_hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            PCWSTR(to_wide("LOL_BENCH_CLASS").as_ptr()),
            PCWSTR(to_wide("LOL_BENCH").as_ptr()),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            hinstance,
            None,
        )
        .unwrap()
    };

    let tray_hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE,
            PCWSTR(to_wide("LOL_TRAY_CLASS").as_ptr()),
            PCWSTR(to_wide("LOL_TRAY").as_ptr()),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            hinstance,
            None,
        )
        .unwrap()
    };

    let state = Box::into_raw(Box::new(WndState {
        vm: ViewModel::default(),
        config: config.clone(),
        event_tx: event_tx.clone(),
        win_w: 1,
        win_h: 1,
    }));

    unsafe {
        SetWindowLongPtrW(hud_hwnd, GWLP_USERDATA, state as isize);
        SetWindowLongPtrW(bench_hwnd, GWLP_USERDATA, state as isize);
        SetWindowLongPtrW(tray_hwnd, GWLP_USERDATA, state as isize);

        add_tray_icon(tray_hwnd);
        UI_HWND.store(hud_hwnd.0 as isize, Ordering::SeqCst);

        let event_tx_hook = event_tx.clone();
        thread::Builder::new()
            .name("keyboard-hook".to_owned())
            .spawn(move || {
                keyboard_hook_loop(event_tx_hook);
            })
            .expect("启动监控线程失败");
    }
    let _ = hwnd_tx.send(SendHwnd(hud_hwnd));

    let mut last_sync = Instant::now();
    let mut force_sync = true;
    let mut needs_paint_hud = false;
    let mut needs_paint_bench = false;

    // 移除 30ms 定时器驱动，改为消息驱动
    // unsafe { SetTimer(hud_hwnd, 1, 30, None); }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // 无论收到什么消息，都尝试检查 ViewModel 更新，确保响应速度
        if vm_rx.has_changed().unwrap_or(false) {
            let new_vm = vm_rx.borrow_and_update().clone();
            let s = unsafe { &mut *state };

            #[cfg(debug_assertions)]
            let start = Instant::now();

            // 可见性联动
            if s.vm.hud1_visible != new_vm.hud1_visible {
                unsafe {
                    let _ = ShowWindow(
                        hud_hwnd,
                        if new_vm.hud1_visible {
                            SW_SHOWNOACTIVATE
                        } else {
                            SW_HIDE
                        },
                    );
                }
            }
            if s.vm.hud2_visible != new_vm.hud2_visible {
                unsafe {
                    let _ = ShowWindow(
                        bench_hwnd,
                        if new_vm.hud2_visible {
                            SW_SHOWNOACTIVATE
                        } else {
                            SW_HIDE
                        },
                    );
                }
            }

            // 内容变更检查
            if s.vm.hud1_lines != new_vm.hud1_lines || s.vm.hud1_title != new_vm.hud1_title {
                needs_paint_hud = true;
            }
            if s.vm.hud2_selected_slot != new_vm.hud2_selected_slot {
                needs_paint_bench = true;
                #[cfg(debug_assertions)]
                tracing::info!("Overlay: 检测到 HUD2 槽位变更, 标记重绘");
            }

            s.vm = new_vm;
            force_sync = true;

            #[cfg(debug_assertions)]
            trace!("Overlay: VM 更新处理耗时: {:?}", start.elapsed());
        }

        // 窗口对齐逻辑 (频率限制，但 force_sync 时立即执行)
        if force_sync || Instant::now().duration_since(last_sync) >= Duration::from_millis(150)
        {
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

                    let r = windows::Win32::Foundation::RECT {
                        left: s.vm.lcu_rect.x,
                        top: s.vm.lcu_rect.y,
                        right: s.vm.lcu_rect.x + nw,
                        bottom: s.vm.lcu_rect.y + nh,
                    };
                    if s.vm.hud2_visible {
                        let bench_r = get_bench_container_rect(nw, nh);
                        let bx = r.left + bench_r.x.round() as i32;
                        let by = r.top + bench_r.y.round() as i32;
                        let bw = bench_r.w.round() as i32;
                        let bh = bench_r.h.round() as i32;
                        unsafe {
                            let _ = SetWindowPos(
                                bench_hwnd,
                                HWND_TOPMOST,
                                bx,
                                by,
                                bw,
                                bh,
                                SWP_NOACTIVATE,
                            );
                        }
                    }
                }
            }
            force_sync = false;
        }

        // 执行重绘 (在此处执行可确保在一次消息处理循环内完成从“收到变更”到“呈现”的全过程)
        if needs_paint_hud && unsafe { (*state).vm.hud1_visible } {
            unsafe {
                paint_hud(hud_hwnd, &*state);
            }
            needs_paint_hud = false;
        }
        if needs_paint_bench && unsafe { (*state).vm.hud2_visible } {
            #[cfg(debug_assertions)]
            let paint_start = Instant::now();

            unsafe {
                paint_bench(bench_hwnd, &*state);
            }
            needs_paint_bench = false;

            #[cfg(debug_assertions)]
            tracing::info!("Overlay: HUD2 重绘完成，耗时: {:?}", paint_start.elapsed());
        }
    }
}

static mut HOOK_TX: Option<tokio::sync::mpsc::Sender<AppEvent>> = None;

fn keyboard_hook_loop(event_tx: tokio::sync::mpsc::Sender<AppEvent>) {
    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(None).unwrap().into();
        let hook =
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), hinstance, 0).unwrap();
        KEYBOARD_HOOK.store(hook.0 as isize, Ordering::SeqCst);
        HOOK_TX = Some(event_tx);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
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
