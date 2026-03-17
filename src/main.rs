//! LOL LCU 自动化工具 - Rust 实现
//!
//! 入口点，对应 Python `main.py`。

// Release 构建：不创建控制台黑窗（双击启动时无黑窗）。
// 从终端启动时 stderr 句柄仍由父进程继承，日志照常输出到终端。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//!
//! 架构：      
//! ```
//! main
//! ├── logging::init_logging()               初始化日志（控制台 + 文件）
//! ├── win::overlay::spawn_overlay_thread()  启动 overlay Win32 线程
//! └── tokio::main (async)
//!     └── run_with_reconnect()
//!         ├── lcu::connection::read_credentials()
//!         ├── lcu::connection::build_client()
//!         ├── lcu::websocket::spawn_ws_loop()
//!         └── main_loop()
//!             ├── 订阅 gameflow / ready-check / honor-ballot / champ-select
//!             └── window_fix_loop (后台 task)
//! ```

mod app;
mod lcu;
mod logging;
mod win;

use std::sync::atomic::AtomicBool;
#[cfg(debug_assertions)]
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{error, info, warn};

use app::handlers;
use app::state::new_shared_state;
use lcu::api::LcuClient;
use lcu::connection::{build_client, wait_for_credentials};
use lcu::websocket::{spawn_ws_loop, LcuEvent};
use win::overlay::{spawn_overlay_thread, OverlayCmd};

/// 重连等待时间（对应 Python `RECONNECT_DELAY_SECONDS`）
const RECONNECT_DELAY_SECS: u64 = 5;

/// `--show-overlay`：强制 overlay 始终显示（仅 debug 构建有效）。
pub static DEBUG_SHOW_OVERLAY: AtomicBool = AtomicBool::new(false);

// ── 入口 ─────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // Release 构建：尝试附加到父进程控制台（从终端启动时才有日志输出）
    #[cfg(not(debug_assertions))]
    try_attach_parent_console();

    // 解析启动参数（仅 debug 构建）
    #[cfg(debug_assertions)]
    {
        if std::env::args().any(|a| a == "--show-overlay") {
            DEBUG_SHOW_OVERLAY.store(true, Ordering::Relaxed);
        }
    }

    // 初始化日志（同时写入 lol_lcu.log 和控制台）
    logging::init_logging(None);
    info!("LOL LCU 自动化工具启动");
    #[cfg(debug_assertions)]
    if DEBUG_SHOW_OVERLAY.load(Ordering::Relaxed) {
        info!("[DEBUG] --show-overlay 已启用：overlay 将始终显示");
    }

    // 启动 overlay 线程（Win32 消息循环）
    // click_tx: overlay 线程 → tokio 的槽位点击事件
    let (click_tx, mut click_rx) = mpsc::channel::<usize>(32);
    let overlay_tx = spawn_overlay_thread(click_tx);

    // --show-overlay：线程刚启动，稍等窗口创建完成后立即 Show
    #[cfg(debug_assertions)]
    if DEBUG_SHOW_OVERLAY.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = overlay_tx.send(OverlayCmd::Show).await;
    }

    let state = new_shared_state();

    // 主重连循环
    run_with_reconnect(state.clone(), overlay_tx.clone(), &mut click_rx).await;

    // 退出前通知 overlay 线程退出
    let _ = overlay_tx.send(OverlayCmd::Quit).await;
    info!("程序退出");
}

// ── run_with_reconnect ───────────────────────────────────────────

async fn run_with_reconnect(
    state: app::state::SharedState,
    overlay_tx: mpsc::Sender<OverlayCmd>,
    click_rx: &mut mpsc::Receiver<usize>,
) {
    loop {
        info!("正在连接 LCU...");

        match run_once(state.clone(), overlay_tx.clone(), click_rx).await {
            Ok(()) => {
                info!("主循环正常结束");
            }
            Err(e) => {
                error!("连接中断: {e:#}");
            }
        }

        // 重连前清理会话状态（对应 Python 每次重连都新建 RuntimeState()）
        state.lock().reset_session();

        // 重置 overlay 状态（--show-overlay 时跳过 Hide）
        #[cfg(debug_assertions)]
        if !crate::DEBUG_SHOW_OVERLAY.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = overlay_tx.send(OverlayCmd::Hide).await;
        }
        #[cfg(not(debug_assertions))]
        let _ = overlay_tx.send(OverlayCmd::Hide).await;
        let _ = overlay_tx.send(OverlayCmd::SetBenchIds(vec![])).await;

        info!("{RECONNECT_DELAY_SECS} 秒后尝试重连...");
        sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
    }
}

// ── run_once ─────────────────────────────────────────────────────

async fn run_once(
    state: app::state::SharedState,
    overlay_tx: mpsc::Sender<OverlayCmd>,
    click_rx: &mut mpsc::Receiver<usize>,
) -> anyhow::Result<()> {
    // 扫描进程列表，等待 LCU 进程出现（对应 willump 的初始化循环）
    let creds = wait_for_credentials().await;
    info!("LCU 凭据获取成功，port={}", creds.port);

    // 构建 HTTP 客户端
    let http_client = build_client(&creds)?;
    let api = LcuClient::new(&creds, http_client);

    // 启动 WebSocket
    let ws_handle = spawn_ws_loop(&creds).await?;
    info!("WebSocket 已就绪");

    // 获取初始状态
    let phase = api.get_gameflow_phase().await?;
    info!("当前阶段: {phase}");
    handlers::set_overlay_visibility_by_phase(&state, &overlay_tx, Some(&phase)).await;

    let summoner = api.get_current_summoner().await?;
    let display_name = summoner
        .get("displayName")
        .and_then(|v| v.as_str())
        .unwrap_or("<未知>");
    info!("召唤师: {display_name}");

    info!("已开始监听，Ctrl+C 退出");

    // 启动窗口修复后台任务
    let fix_api = api.clone();
    let fix_tx = overlay_tx.clone();
    let window_fix_task = tokio::spawn(async move {
        handlers::window_fix_loop(fix_api, fix_tx).await;
    });

    // 订阅 WebSocket 各事件频道
    let mut rx_gameflow = ws_handle.subscribe();
    let mut rx_ready_check = ws_handle.subscribe();
    let mut rx_honor = ws_handle.subscribe();
    let mut rx_champ_select = ws_handle.subscribe();

    // 主事件分发循环
    let result = main_loop(
        api,
        state,
        overlay_tx.clone(),
        click_rx,
        &mut rx_gameflow,
        &mut rx_ready_check,
        &mut rx_honor,
        &mut rx_champ_select,
    )
    .await;

    window_fix_task.abort();

    result
}

// ── main_loop ─────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn main_loop(
    api: LcuClient,
    state: app::state::SharedState,
    overlay_tx: mpsc::Sender<OverlayCmd>,
    click_rx: &mut mpsc::Receiver<usize>,
    rx_gameflow: &mut tokio::sync::broadcast::Receiver<LcuEvent>,
    rx_ready_check: &mut tokio::sync::broadcast::Receiver<LcuEvent>,
    rx_honor: &mut tokio::sync::broadcast::Receiver<LcuEvent>,
    rx_champ_select: &mut tokio::sync::broadcast::Receiver<LcuEvent>,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            // ── Gameflow 阶段变化 ────────────────────────
            ev = rx_gameflow.recv() => {
                match ev {
                    Ok(event) if event.uri == "/lol-gameflow/v1/gameflow-phase" => {
                        let api2 = api.clone();
                        let state2 = state.clone();
                        let tx2 = overlay_tx.clone();
                        tokio::spawn(async move {
                            handlers::handle_gameflow(api2, state2, tx2, event.payload).await;
                        });
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("gameflow 事件丢失 {n} 条");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(anyhow::anyhow!("WebSocket 频道已关闭"));
                    }
                }
            }

            // ── ReadyCheck ───────────────────────────────
            ev = rx_ready_check.recv() => {
                match ev {
                    Ok(event) if event.uri == "/lol-matchmaking/v1/ready-check" => {
                        let api2 = api.clone();
                        let state2 = state.clone();
                        tokio::spawn(async move {
                            handlers::handle_ready_check(api2, state2, event.payload).await;
                        });
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("ready-check 事件丢失 {n} 条");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(anyhow::anyhow!("WebSocket 频道已关闭"));
                    }
                }
            }

            // ── 点赞投票 ─────────────────────────────────
            ev = rx_honor.recv() => {
                match ev {
                    Ok(event) if event.uri == "/lol-honor-v2/v1/ballot" => {
                        let api2 = api.clone();
                        let state2 = state.clone();
                        let tx2 = overlay_tx.clone();
                        tokio::spawn(async move {
                            handlers::handle_honor_ballot(api2, state2, tx2, event.payload).await;
                        });
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("honor-ballot 事件丢失 {n} 条");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(anyhow::anyhow!("WebSocket 频道已关闭"));
                    }
                }
            }

            // ── 英雄选择 ─────────────────────────────────
            ev = rx_champ_select.recv() => {
                match ev {
                    Ok(event) if event.uri == "/lol-champ-select/v1/session" => {
                        let api2 = api.clone();
                        let state2 = state.clone();
                        let tx2 = overlay_tx.clone();
                        tokio::spawn(async move {
                            handlers::handle_champ_select(api2, state2, tx2, event.payload).await;
                        });
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("champ-select 事件丢失 {n} 条");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(anyhow::anyhow!("WebSocket 频道已关闭"));
                    }
                }
            }

            // ── Overlay 点击回调 ─────────────────────────
            slot = click_rx.recv() => {
                match slot {
                    Some(slot_index) => {
                        let api2 = api.clone();
                        let state2 = state.clone();
                        let tx2 = overlay_tx.clone();
                        tokio::spawn(async move {
                            handlers::handle_overlay_click(api2, state2, tx2, slot_index).await;
                        });
                    }
                    None => {
                        warn!("click_rx 通道已关闭");
                    }
                }
            }
        }
    }
}
// ── Release 控制台附加 ─────────────────────────────────────────────

/// 尝试附加到父进程控制台（仅 release 构建有效）。
///
/// - 从终端（cmd/PowerShell）启动：`AttachConsole` 成功，重定向 stderr 到控制台，日志可见。
/// - 双击启动：无父控制台，`AttachConsole` 失败，保持静默，不弹黑窗。
#[cfg(not(debug_assertions))]
fn try_attach_parent_console() {
    use std::os::windows::io::IntoRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        AttachConsole, SetConsoleCP, SetConsoleOutputCP, SetStdHandle,
        ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    unsafe {
        // 附加到父进程控制台；失败则说明是双击启动，不需输出
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
        // 切换为 UTF-8 代码页，防止中文乱码
        let _ = SetConsoleOutputCP(65001);
        let _ = SetConsoleCP(65001);
        // 打开控制台输出设备，将 Windows 标准句柄指向它
        // （tracing-subscriber 首次调用 io::stderr() 时会懒初始化，此时已更新应能取到新句柄）
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
            let h = HANDLE(f.into_raw_handle().cast());
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, h);
            let _ = SetStdHandle(STD_ERROR_HANDLE, h);
            // into_raw_handle 已转移所有权，句柄生命周期随进程。
        }
    }
}