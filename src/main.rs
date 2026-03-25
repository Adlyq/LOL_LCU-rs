//! LOL LCU 自动化工具 - Rust 实现

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod lcu;
mod logging;
mod win;

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::app::config::new_shared_config;
use crate::app::state::new_shared_state;
use crate::lcu::api::LcuClient;
use crate::lcu::connection::{build_client, wait_for_credentials};
use crate::lcu::websocket::spawn_ws_loop;

// ── 单实例守卫 ───────────────────────────────────────────────────

fn ensure_single_instance() -> windows::Win32::Foundation::HANDLE {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_ALREADY_EXISTS;
    use windows::Win32::System::Threading::CreateMutexW;

    let name = to_wide("Global\\LOL_LCU_SingleInstance");
    let handle =
        unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())).expect("CreateMutexW 失败") };

    if unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_ALREADY_EXISTS {
        #[cfg(not(debug_assertions))]
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OK};
            let text = to_wide("LOL_LCU 已在运行中，不允许重复启动。");
            let caption = to_wide("LOL_LCU");
            MessageBoxW(
                HWND::default(),
                PCWSTR(text.as_ptr()),
                PCWSTR(caption.as_ptr()),
                MB_OK | MB_ICONWARNING,
            );
        }
        #[cfg(debug_assertions)]
        eprintln!("[LOL_LCU] 已有实例在运行，退出。");
        std::process::exit(1);
    }
    handle
}

#[cfg(not(debug_assertions))]
fn try_attach_parent_console() {
    use std::os::windows::io::IntoRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::*;
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
        let _ = SetConsoleOutputCP(65001);
        let _ = SetConsoleCP(65001);
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
            let h = HANDLE(f.into_raw_handle().cast());
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, h);
            let _ = SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
}

use crate::win::winapi::to_wide;

#[tokio::main]
async fn main() {
    let _single_instance = ensure_single_instance();

    #[cfg(not(debug_assertions))]
    try_attach_parent_console();

    // 开启 DPI 感知
    unsafe {
        use windows::Win32::UI::HiDpi::*;
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    logging::init_logging(None);
    info!("LOL LCU 助手启动 (HUD + Tray 模式)");

    let config = crate::app::config::new_shared_config();
    let state = crate::app::state::new_shared_state();

    // ── 初始化核心事件总线 ──────────────────────────────────────────
    let (event_tx, event_rx) = mpsc::channel::<crate::app::event::AppEvent>(1024);
    let (vm_tx, vm_rx) = tokio::sync::watch::channel(crate::app::viewmodel::ViewModel::default());

    // 启动 UI 线程 (传入 vm_rx 和 event_tx)
    let overlay_tx =
        crate::win::overlay::spawn_overlay_thread(config.clone(), event_tx.clone(), vm_rx);

    // 启动主逻辑循环
    let mut main_loop = crate::app::main_loop::MainLoop::new(
        event_tx.clone(),
        event_rx,
        vm_tx,
        overlay_tx.clone(),
        state.clone(),
        config.clone(),
    );

    tokio::spawn(async move {
        main_loop.run().await;
    });

    // 启动 Tick 服务
    {
        let event_tx_c = event_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if event_tx_c
                    .send(crate::app::event::AppEvent::Tick)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    // 主连接监控循环
    connection_monitor_loop(event_tx).await;
}

async fn connection_monitor_loop(event_tx: mpsc::Sender<crate::app::event::AppEvent>) {
    loop {
        debug!("开始探测 LCU 进程...");
        let creds = wait_for_credentials().await;
        info!(
            "发现 LCU 进程: Port={}, Token=***{}",
            creds.port,
            &creds.token[creds.token.len() - 4..]
        );

        let http_client = match build_client(&creds) {
            Ok(c) => c,
            Err(e) => {
                error!("构建 HTTP 客户端失败: {e}");
                sleep(Duration::from_secs(3)).await;
                continue;
            }
        };

        let api = LcuClient::new(&creds, http_client);
        let ws_handle = match spawn_ws_loop(&creds).await {
            Ok(h) => h,
            Err(e) => {
                error!("WebSocket 连接失败: {e}");
                sleep(Duration::from_secs(3)).await;
                continue;
            }
        };

        // 通知 MainLoop LCU 已连接
        info!("发送 LcuConnected 事件至主循环");
        let _ = event_tx
            .send(crate::app::event::AppEvent::LcuConnected(api.clone()))
            .await;

        // WebSocket 事件转发任务
        let mut rx_ws = ws_handle.subscribe();
        let event_tx_ws = event_tx.clone();
        let ws_task = tokio::spawn(async move {
            debug!("WebSocket 转发任务已启动");
            while let Ok(event) = rx_ws.recv().await {
                if event_tx_ws
                    .send(crate::app::event::AppEvent::LcuEvent(event))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            debug!("WebSocket 转发任务已退出");
        });

        // 初始状态同步
        if let Ok(phase) = api.get_gameflow_phase().await {
            info!("同步初始游戏阶段: {}", phase);
            let _ = event_tx
                .send(crate::app::event::AppEvent::LcuPhaseChanged(phase))
                .await;
        }

        // 等待连接断开
        let _ = ws_task.await;

        warn!("LCU 连接已断开，准备重连...");
        let _ = event_tx
            .send(crate::app::event::AppEvent::LcuDisconnected)
            .await;

        sleep(Duration::from_secs(3)).await;
    }
}
