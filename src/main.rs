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
use app::config::new_shared_config;
use lcu::api::LcuClient;
use lcu::connection::{build_client, wait_for_credentials};
use lcu::websocket::{spawn_ws_loop, LcuEvent};
use win::overlay::{spawn_overlay_thread, OverlayCmd};
use win::info_panel::PanelAction;

/// 重连等待时间（对应 Python `RECONNECT_DELAY_SECONDS`）
const RECONNECT_DELAY_SECS: u64 = 5;

/// `--show-overlay`：强制 overlay 始终显示（仅 debug 构建有效）。
pub static DEBUG_SHOW_OVERLAY: AtomicBool = AtomicBool::new(false);

// ── 单实例守卫 ───────────────────────────────────────────────────

/// 通过 Windows 命名互斥量确保只有一个实例运行。
///
/// 返回互斥量句柄（必须保持存活至进程退出，否则互斥量会被释放）。
/// 若已有实例在运行，弹出提示后退出进程。
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

    // ERROR_ALREADY_EXISTS：互斥量已存在，说明已有实例在运行
    if unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_ALREADY_EXISTS {
        // Release 模式无控制台，弹一个 MessageBox 提示用户
        #[cfg(not(debug_assertions))]
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OK};
            use std::ffi::OsStr;
            let text: Vec<u16> = OsStr::new("LOL_LCU 已在运行中，不允许重复启动。")
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
        #[cfg(debug_assertions)]
        eprintln!("[LOL_LCU] 已有实例在运行，退出。");

        std::process::exit(1);
    }

    handle
}

// ── 入口 ─────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // 单实例检查（句柄必须存活至进程退出）
    let _single_instance_mutex = ensure_single_instance();

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
    let config = new_shared_config();
    let (action_tx, mut action_rx) = mpsc::channel::<PanelAction>(32);
    let overlay_tx = spawn_overlay_thread(click_tx, config.clone(), action_tx);

    // --show-overlay：线程刚启动，稍等窗口创建完成后立即 Show
    #[cfg(debug_assertions)]
    if DEBUG_SHOW_OVERLAY.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = overlay_tx.send(OverlayCmd::Show).await;
    }

    let state = new_shared_state();

    // 主重连循环
    run_with_reconnect(state.clone(), config, overlay_tx.clone(), &mut click_rx, &mut action_rx).await;

    // 退出前通知 overlay 线程退出
    let _ = overlay_tx.send(OverlayCmd::Quit).await;
    info!("程序退出");
}

// ── run_with_reconnect ───────────────────────────────────────────

async fn run_with_reconnect(
    state: app::state::SharedState,
    config: app::config::SharedConfig,
    overlay_tx: mpsc::Sender<OverlayCmd>,
    click_rx: &mut mpsc::Receiver<usize>,
    action_rx: &mut mpsc::Receiver<PanelAction>,
) {
    loop {
        info!("正在连接 LCU...");

        match run_once(state.clone(), config.clone(), overlay_tx.clone(), click_rx, action_rx).await {
            Ok(()) => {
                info!("主循环正常结束");
            }
            Err(e) => {
                error!("连接中断: {e:#}");
            }
        }

        // 重连前清理会话状态（对应 Python 每次重连都新建 RuntimeState()）
        state.lock().reset_session();
        // 面板显示等待状态
        let _ = overlay_tx.send(win::overlay::OverlayCmd::UpdatePanel(
            win::info_panel::PanelContent {
                connection: "等待连接...".to_owned(),
                ..Default::default()
            }
        )).await;
        // 重置 overlay 状态（--show-overlay 时跳过 Hide）
        #[cfg(debug_assertions)]
        if !crate::DEBUG_SHOW_OVERLAY.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = overlay_tx.send(OverlayCmd::Hide).await;
        }
        #[cfg(not(debug_assertions))]
        let _ = overlay_tx.send(OverlayCmd::Hide).await;
        let _ = overlay_tx.send(OverlayCmd::SetBenchIds(vec![])).await;

        // 清空断线期间积压的面板动作，避免重连后误触发
        while action_rx.try_recv().is_ok() {}

        info!("{RECONNECT_DELAY_SECS} 秒后尝试重连...");
        sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
    }
}

// ── run_once ─────────────────────────────────────────────────────

async fn run_once(
    state: app::state::SharedState,
    config: app::config::SharedConfig,
    overlay_tx: mpsc::Sender<OverlayCmd>,
    click_rx: &mut mpsc::Receiver<usize>,
    action_rx: &mut mpsc::Receiver<PanelAction>,
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
        .unwrap_or("<未知>")
        .to_owned();
    info!("召唤师: {display_name}");

    // 初始化面板连接状态
    let _ = overlay_tx.send(win::overlay::OverlayCmd::UpdatePanel(
        win::info_panel::PanelContent {
            connection: format!("已连接 · {display_name}"),
            phase: phase.clone(),
            ..Default::default()
        }
    )).await;

    info!("已开始监听，Ctrl+C 退出");

    // 启动窗口修复后台任务
    let fix_api = api.clone();
    let fix_tx = overlay_tx.clone();
    let window_fix_task = tokio::spawn(async move {
        handlers::window_fix_loop(fix_api, fix_tx).await;
    });

    // 启动内存监控后台任务（功能 10：LeagueClientUx 内存超限自动热重载）
    let mem_api = api.clone();
    let mem_cfg = config.clone();
    let mem_monitor_task = tokio::spawn(async move {
        memory_monitor_loop(mem_api, mem_cfg).await;
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
        config,
        overlay_tx.clone(),
        click_rx,
        action_rx,
        &mut rx_gameflow,
        &mut rx_ready_check,
        &mut rx_honor,
        &mut rx_champ_select,
    )
    .await;

    window_fix_task.abort();
    mem_monitor_task.abort();

    result
}

// ── main_loop ─────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn main_loop(
    api: LcuClient,
    state: app::state::SharedState,
    config: app::config::SharedConfig,
    overlay_tx: mpsc::Sender<OverlayCmd>,
    click_rx: &mut mpsc::Receiver<usize>,
    action_rx: &mut mpsc::Receiver<PanelAction>,
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
                        let cfg2 = config.clone();
                        let tx2 = overlay_tx.clone();
                        tokio::spawn(async move {
                            handlers::handle_gameflow(api2, state2, cfg2, tx2, event.payload).await;
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
                        let cfg2 = config.clone();
                        tokio::spawn(async move {
                            handlers::handle_ready_check(api2, state2, cfg2, event.payload).await;
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
                        let cfg2 = config.clone();
                        let tx2 = overlay_tx.clone();
                        tokio::spawn(async move {
                            handlers::handle_honor_ballot(api2, state2, cfg2, tx2, event.payload).await;
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
                        let cfg2 = config.clone();
                        let tx2 = overlay_tx.clone();
                        tokio::spawn(async move {
                            handlers::handle_champ_select(api2, state2, cfg2, tx2, event.payload).await;
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

            // ── 面板动作 ─────────────────────────────────
            action = action_rx.recv() => {
                match action {
                    Some(PanelAction::ReloadUx) => {
                        info!("正在热重载 LCU 客户端...");
                        let api2 = api.clone();
                        tokio::spawn(async move {
                            match api2.reload_ux().await {
                                Ok(()) => info!("LCU 客户端热重载已触发"),
                                Err(e) => warn!("热重载失败: {e}"),
                            }
                        });
                    }
                    Some(PanelAction::PlayAgain) => {
                        info!("正在退出结算页面...");
                        let api2 = api.clone();
                        tokio::spawn(async move {
                            match api2.play_again().await {
                                Ok(()) => info!("退出结算页面成功"),
                                Err(e) => warn!("退出结算页面失败: {e}"),
                            }
                        });
                    }
                    Some(PanelAction::AutoLoot) => {
                        info!("手动触发领取任务与宝箱...");
                        let api2 = api.clone();
                        tokio::spawn(async move {
                            app::loot::run_auto_loot(&api2).await;
                        });
                    }
                    Some(PanelAction::Quit) => {
                        info!("用户请求退出");
                        std::process::exit(0);
                    }
                    None => {
                        warn!("action_rx 通道已关闭");
                    }
                }
            }
        }
    }
}

// ── 内存监控循环（功能 10）────────────────────────────────────────

/// 每 5 分钟检查一次 `LeagueClientUx.exe` 内存占用；
/// 超过阈值（1500 MB）且当前处于大厅/空闲阶段时，自动触发热重载。
/// 热重载后进入 30 分钟冷却期，避免频繁重载。
async fn memory_monitor_loop(api: lcu::api::LcuClient, config: app::config::SharedConfig) {
    const CHECK_INTERVAL_SECS: u64 = 5 * 60;
    const COOLDOWN_SECS: u64 = 30 * 60;

    // 安全阶段：只在这些阶段才重载，不打断游戏/选人
    const SAFE_PHASES: &[&str] = &["None", "Lobby", "Matchmaking", "EndOfGame", ""];

    let mut last_reload: Option<std::time::Instant> = None;

    loop {
        sleep(Duration::from_secs(CHECK_INTERVAL_SECS)).await;

        // 判断配置是否开启
        let (enabled, threshold_mb) = {
            let cfg = config.lock();
            (cfg.memory_monitor, cfg.memory_threshold_mb)
        };
        if !enabled {
            continue;
        }

        // 冷却期内跳过
        if let Some(t) = last_reload {
            if t.elapsed().as_secs() < COOLDOWN_SECS {
                continue;
            }
        }

        // 确认当前阶段安全
        let phase = match api.get_gameflow_phase().await {
            Ok(p) => p,
            Err(_) => continue,
        };
        if !SAFE_PHASES.contains(&phase.as_str()) {
            continue;
        }

        // 读取 LeagueClientUx.exe 内存
        let mem_mb = get_lcu_ux_memory_mb();
        if mem_mb == 0 {
            continue; // 进程未找到
        }

        if mem_mb < threshold_mb {
            tracing::debug!("LeagueClientUx 内存 {mem_mb} MB，正常");
            continue;
        }

        warn!(
            "LeagueClientUx 内存 {mem_mb} MB 超过阈値 {threshold_mb} MB，阶段={phase}，触发自动热重载..."
        );
        match api.reload_ux().await {
            Ok(()) => {
                info!("内存超限热重载已触发（{mem_mb} MB → 热重载）");
                last_reload = Some(std::time::Instant::now());
            }
            Err(e) => {
                warn!("内存超限热重载失败: {e}");
            }
        }
    }
}

/// 读取 `LeagueClientUx.exe` 的当前内存占用（RSS，单位 MB）。
/// 进程不存在时返回 0。
fn get_lcu_ux_memory_mb() -> u64 {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        false,
        ProcessRefreshKind::new().with_memory(),
    );
    for (_, process) in sys.processes() {
        if process.name().to_string_lossy().to_lowercase() == "leagueclientux.exe" {
            return process.memory() / 1_048_576;
        }
    }
    0
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