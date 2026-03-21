//! LOL LCU 自动化工具 - Rust 实现

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod lcu;
mod logging;
mod win;

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{error, info};

use app::handlers;
use app::state::new_shared_state;
use app::config::new_shared_config;
use lcu::api::LcuClient;
use lcu::connection::{build_client, wait_for_credentials};
use lcu::websocket::spawn_ws_loop;
use win::overlay::{spawn_overlay_thread, OverlayCmd, OverlaySender, TrayAction};

/// 重连等待时间
const RECONNECT_DELAY_SECS: u64 = 5;

// ── 单实例守卫 ───────────────────────────────────────────────────

fn ensure_single_instance() -> windows::Win32::Foundation::HANDLE {
    use windows::Win32::Foundation::ERROR_ALREADY_EXISTS;
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;
    use std::os::windows::ffi::OsStrExt;
    use std::ffi::OsStr;

    let name: Vec<u16> = OsStr::new("Global\\LOL_LCU_SingleInstance")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe {
        CreateMutexW(None, true, PCWSTR(name.as_ptr()))
            .expect("CreateMutexW 失败")
    };

    if unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_ALREADY_EXISTS {
        #[cfg(not(debug_assertions))]
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OK};
            let text: Vec<u16> = OsStr::new("LOL_LCU 已在运行中。")
                .encode_wide().chain(std::iter::once(0)).collect();
            let caption: Vec<u16> = OsStr::new("LOL_LCU")
                .encode_wide().chain(std::iter::once(0)).collect();
            MessageBoxW(
                windows::Win32::Foundation::HWND::default(),
                PCWSTR(text.as_ptr()),
                PCWSTR(caption.as_ptr()),
                MB_OK | MB_ICONWARNING,
            );
        }
        std::process::exit(1);
    }
    handle
}

// ── 入口 ─────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let _single_instance_mutex = ensure_single_instance();

    // 开启 DPI 感知，防止多显示器缩放导致 HUD 错位
    unsafe {
        use windows::Win32::UI::HiDpi::*;
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    logging::init_logging(None);
    info!("LOL LCU 助手启动 (HUD + Tray 模式)");

    let config = new_shared_config();
    let (action_tx, mut action_rx) = mpsc::channel::<TrayAction>(32);
    let overlay_tx = spawn_overlay_thread(config.clone(), action_tx);

    let state = new_shared_state();

    // 主重连循环
    run_with_reconnect(state, config, overlay_tx.clone(), &mut action_rx).await;

    let _ = overlay_tx.send(OverlayCmd::Quit).await;
}

async fn run_with_reconnect(
    state: app::state::SharedState,
    config: app::config::SharedConfig,
    overlay_tx: OverlaySender,
    action_rx: &mut mpsc::Receiver<TrayAction>,
) {
    loop {
        info!("正在连接 LCU...");
        let _ = overlay_tx.send(OverlayCmd::UpdateHud("等待连接...".to_owned(), String::new())).await;

        match run_once(state.clone(), config.clone(), overlay_tx.clone(), action_rx).await {
            Ok(()) => info!("主循环正常结束"),
            Err(e) => error!("连接中断: {e:#}"),
        }

        state.lock().reset_session();
        
        while action_rx.try_recv().is_ok() {}

        sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
    }
}

async fn run_once(
    state: app::state::SharedState,
    config: app::config::SharedConfig,
    overlay_tx: OverlaySender,
    action_rx: &mut mpsc::Receiver<TrayAction>,
) -> anyhow::Result<()> {
    let creds = wait_for_credentials().await;
    let http_client = build_client(&creds)?;
    let api = LcuClient::new(&creds, http_client);
    let ws_handle = spawn_ws_loop(&creds).await?;

    let phase = api.get_gameflow_phase().await?;
    let summoner = api.get_current_summoner().await?;
    let display_name = summoner.get("displayName").and_then(|v| v.as_str()).unwrap_or("<未知>").to_owned();

    let _ = overlay_tx.send(OverlayCmd::UpdateHud(format!("已连接: {display_name}"), String::new())).await;

    // 启动后台任务
    {
        let api_c = api.clone();
        let config_c = config.clone();
        tokio::spawn(async move {
            app::tasks::memory_monitor_loop(api_c, config_c).await;
        });
    }

    // 触发初始阶段处理
    {
        let api_c = api.clone();
        let state_c = state.clone();
        let config_c = config.clone();
        let tx_c = overlay_tx.clone();
        let phase_c = phase.clone();
        tokio::spawn(async move {
            let initial_event = serde_json::json!({ "data": phase_c });
            handlers::handle_gameflow(api_c, state_c, config_c, tx_c, initial_event).await;
        });
    }

    let mut rx_gameflow = ws_handle.subscribe();
    let mut rx_ready_check = ws_handle.subscribe();
    let mut rx_honor = ws_handle.subscribe();
    let mut rx_champ_select = ws_handle.subscribe();

    loop {
        tokio::select! {
            ev = rx_gameflow.recv() => {
                if let Ok(event) = ev {
                    if event.uri == "/lol-gameflow/v1/gameflow-phase" {
                        handlers::handle_gameflow(api.clone(), state.clone(), config.clone(), overlay_tx.clone(), event.payload).await;
                    }
                }
            }
            ev = rx_ready_check.recv() => {
                if let Ok(event) = ev {
                    if event.uri == "/lol-matchmaking/v1/ready-check" {
                        handlers::handle_ready_check(api.clone(), state.clone(), config.clone(), event.payload).await;
                    }
                }
            }
            ev = rx_honor.recv() => {
                if let Ok(event) = ev {
                    if event.uri == "/lol-honor-v2/v1/ballot" {
                        handlers::handle_honor_ballot(api.clone(), state.clone(), config.clone(), overlay_tx.clone(), event.payload).await;
                    }
                }
            }
            ev = rx_champ_select.recv() => {
                if let Ok(event) = ev {
                    if event.uri == "/lol-champ-select/v1/session" {
                        handlers::handle_champ_select(api.clone(), state.clone(), config.clone(), overlay_tx.clone(), event.payload).await;
                    }
                }
            }
            action = action_rx.recv() => {
                match action {
                    Some(TrayAction::ReloadUx) => {
                        let _ = api.reload_ux().await;
                    }
                    Some(TrayAction::PlayAgain) => {
                        let _ = api.play_again().await;
                    }
                    Some(TrayAction::AutoLoot) => {
                        let api2 = api.clone();
                        tokio::spawn(async move { app::loot::run_auto_loot(&api2).await; });
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}
