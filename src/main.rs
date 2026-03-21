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

use crate::app::config::new_shared_config;
use crate::app::handlers;
use crate::app::state::new_shared_state;
use crate::lcu::api::LcuClient;
use crate::lcu::connection::{build_client, wait_for_credentials};
use crate::lcu::websocket::spawn_ws_loop;
use crate::win::overlay::{spawn_overlay_thread, OverlayCmd, OverlaySender, TrayAction};

#[tokio::main]
async fn main() {
    // 开启 DPI 感知，防止多显示器缩放导致 HUD 错位
    unsafe {
        use windows::Win32::UI::HiDpi::*;
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    logging::init_logging(None);
    info!("LOL LCU 助手启动 (HUD + Tray 模式)");

    let config = new_shared_config();
    let (action_tx, mut action_rx) = mpsc::channel::<TrayAction>(32);
    let (click_tx, mut click_rx) = mpsc::channel::<usize>(32);
    let overlay_tx = spawn_overlay_thread(config.clone(), action_tx, click_tx);

    let state = new_shared_state();

    // 主重连循环
    run_with_reconnect(state, config, overlay_tx.clone(), &mut action_rx, &mut click_rx).await;

    let _ = overlay_tx.send(OverlayCmd::Quit).await;
}

async fn run_with_reconnect(
    state: app::state::SharedState,
    config: app::config::SharedConfig,
    overlay_tx: OverlaySender,
    action_rx: &mut mpsc::Receiver<TrayAction>,
    click_rx: &mut mpsc::Receiver<usize>,
) {
    loop {
        info!("正在连接 LCU...");
        let _ = overlay_tx.send(OverlayCmd::UpdateHud("等待连接...".to_owned(), String::new())).await;

        match run_once(state.clone(), config.clone(), overlay_tx.clone(), action_rx, click_rx).await {
            Ok(()) => info!("主循环正常结束"),
            Err(e) => error!("连接中断: {e:#}"),
        }

        state.lock().reset_session();
        sleep(Duration::from_secs(3)).await;
    }
}

async fn run_once(
    state: app::state::SharedState,
    config: app::config::SharedConfig,
    overlay_tx: OverlaySender,
    action_rx: &mut mpsc::Receiver<TrayAction>,
    click_rx: &mut mpsc::Receiver<usize>,
) -> anyhow::Result<()> {
    let creds = wait_for_credentials().await;
    let http_client = build_client(&creds)?;
    let api = LcuClient::new(&creds, http_client);
    let ws_handle = spawn_ws_loop(&creds).await?;

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
        
        let api_c2 = api.clone();
        let tx_c = overlay_tx.clone();
        tokio::spawn(async move {
            app::tasks::window_fix_loop(api_c2, tx_c).await;
        });
    }

    // 触发初始状态同步：确保应用启动时即使已在对局中也能正确衔接
    {
        let api_c = api.clone();
        let state_c = state.clone();
        let config_c = config.clone();
        let tx_c = overlay_tx.clone();
        tokio::spawn(async move {
            // 1. 同步 Gameflow Phase
            if let Ok(phase) = api_c.get_gameflow_phase().await {
                let payload = serde_json::json!({ "data": phase });
                handlers::handle_gameflow(api_c.clone(), state_c.clone(), config_c.clone(), tx_c.clone(), payload).await;
                
                // 2. 如果正在匹配准备中，尝试同步 ReadyCheck
                if phase == "ReadyCheck" {
                    if let Ok(rc) = api_c.get_ready_check().await {
                        let payload = serde_json::json!({ "data": rc });
                        handlers::handle_ready_check(api_c.clone(), state_c.clone(), config_c.clone(), payload).await;
                    }
                }
                
                // 3. 如果正在选人中，尝试同步 ChampSelect Session
                if phase == "ChampSelect" {
                    if let Ok(session) = api_c.get_champ_select_session().await {
                        let payload = serde_json::json!({ "data": session });
                        handlers::handle_champ_select(api_c.clone(), state_c.clone(), config_c.clone(), tx_c.clone(), payload).await;
                    }
                }
            }
            
            // 4. 尝试同步 Lobby 状态
            if let Ok(lobby) = api_c.get_lobby().await {
                let payload = serde_json::json!({ "data": lobby });
                handlers::handle_lobby(api_c, state_c, config_c, tx_c, payload).await;
            }
        });
    }

    let mut rx_gameflow = ws_handle.subscribe();
    let mut rx_ready_check = ws_handle.subscribe();
    let mut rx_honor = ws_handle.subscribe();
    let mut rx_champ_select = ws_handle.subscribe();
    let mut rx_lobby = ws_handle.subscribe();

    loop {
        tokio::select! {
            click = click_rx.recv() => {
                if let Some(idx) = click {
                    let api_c = api.clone();
                    tokio::spawn(async move {
                        if let Ok(session) = api_c.get_champ_select_session().await {
                            let ids = LcuClient::extract_bench_champion_ids(&session);
                            if let Some(&cid) = ids.get(idx) {
                                let _ = api_c.swap_bench_champion(cid).await;
                            }
                        }
                    });
                }
            }
            ev = rx_lobby.recv() => {
                if let Ok(event) = ev {
                    if event.uri == "/lol-lobby/v2/lobby" {
                        let api_c = api.clone();
                        let state_c = state.clone();
                        let config_c = config.clone();
                        let tx_c = overlay_tx.clone();
                        tokio::spawn(async move {
                            handlers::handle_lobby(api_c, state_c, config_c, tx_c, event.payload).await;
                        });
                    }
                }
            }
            ev = rx_gameflow.recv() => {
                if let Ok(event) = ev {
                    if event.uri == "/lol-gameflow/v1/gameflow-phase" {
                        let api_c = api.clone();
                        let state_c = state.clone();
                        let config_c = config.clone();
                        let tx_c = overlay_tx.clone();
                        tokio::spawn(async move {
                            handlers::handle_gameflow(api_c, state_c, config_c, tx_c, event.payload).await;
                        });
                    }
                }
            }
            ev = rx_ready_check.recv() => {
                if let Ok(event) = ev {
                    if event.uri == "/lol-matchmaking/v1/ready-check" {
                        let api_c = api.clone();
                        let state_c = state.clone();
                        let config_c = config.clone();
                        tokio::spawn(async move {
                            handlers::handle_ready_check(api_c, state_c, config_c, event.payload).await;
                        });
                    }
                }
            }
            ev = rx_honor.recv() => {
                if let Ok(event) = ev {
                    if event.uri == "/lol-honor-v2/v1/ballot" {
                        let api_c = api.clone();
                        let state_c = state.clone();
                        let config_c = config.clone();
                        let tx_c = overlay_tx.clone();
                        tokio::spawn(async move {
                            handlers::handle_honor_ballot(api_c, state_c, config_c, tx_c, event.payload).await;
                        });
                    }
                }
            }
            ev = rx_champ_select.recv() => {
                if let Ok(event) = ev {
                    if event.uri == "/lol-champ-select/v1/session" {
                        let api_c = api.clone();
                        let state_c = state.clone();
                        let config_c = config.clone();
                        let tx_c = overlay_tx.clone();
                        tokio::spawn(async move {
                            handlers::handle_champ_select(api_c, state_c, config_c, tx_c, event.payload).await;
                        });
                    }
                }
            }
            action = action_rx.recv() => {
                match action {
                    Some(TrayAction::ReloadUx) => {
                        let api_c = api.clone();
                        tokio::spawn(async move { let _ = api_c.reload_ux().await; });
                    }
                    Some(TrayAction::PlayAgain) => {
                        let api_c = api.clone();
                        tokio::spawn(async move { let _ = api_c.play_again().await; });
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}
