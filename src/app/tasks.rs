//! 后台任务集
//!
//! 包括：
//! - LCU 内存监控与自动重载
//! - 客户端窗口比例自动修复 (独立运行)
//! - Overlay 坐标同步监控

use std::time::Duration;
use tokio::time::sleep;
use tokio::sync::mpsc;
use tracing::{info, warn, debug};
use tokio_util::sync::CancellationToken;

use crate::lcu::api::LcuClient;
use crate::app::config::SharedConfig;
use crate::app::event::AppEvent;
use crate::win::winapi;

/// 窗口比例修复循环：独立运行，直接调用 WinAPI 修复客户端异常，不干预助手逻辑。
pub async fn window_fix_loop(api: LcuClient, token: CancellationToken) {
    loop {
        if token.is_cancelled() { break; }

        // 获取缩放比例并执行静默修复
        if let Ok(zoom) = api.get_riotclient_zoom_scale().await {
            if let Some(target) = winapi::find_lcu_window() {
                // 直接修复，不发送事件
                winapi::fix_lcu_window_by_zoom(target, zoom, false);
            }
        }
        
        tokio::select! {
            _ = sleep(Duration::from_millis(1500)) => {}
            _ = token.cancelled() => break,
        }
    }
}

/// Overlay 坐标同步循环：专门为助手 UI 提供 LCU 窗口的实时位置快照。
pub async fn window_position_monitor_loop(event_tx: mpsc::Sender<AppEvent>, token: CancellationToken) {
    loop {
        if token.is_cancelled() { break; }

        if let Some(target) = winapi::find_lcu_window() {
            if let Some(r) = winapi::get_window_rect(target) {
                // 仅发送位置更新事件，驱动 ViewModel 变更
                let _ = event_tx.try_send(AppEvent::WindowRectUpdated {
                    x: r.left,
                    y: r.top,
                    width: r.right - r.left,
                    height: r.bottom - r.top,
                    zoom_scale: 1.0, // 坐标同步不关心缩放修复
                });
            }
        }

        tokio::select! {
            _ = sleep(Duration::from_millis(150)) => {}
            _ = token.cancelled() => break,
        }
    }
}

/// 内存监控循环：当 LeagueClientUx.exe 内存超限时自动触发热重载。
pub async fn memory_monitor_loop(api: LcuClient, config: SharedConfig, token: CancellationToken) {
    const CHECK_INTERVAL_SECS: u64 = 5 * 60;
    const COOLDOWN_SECS: u64 = 30 * 60;
    const SAFE_PHASES: &[&str] = &["None", "Lobby", "Matchmaking", "EndOfGame", ""];

    let mut last_reload: Option<std::time::Instant> = None;

    loop {
        if token.is_cancelled() { break; }

        tokio::select! {
            _ = sleep(Duration::from_secs(CHECK_INTERVAL_SECS)) => {}
            _ = token.cancelled() => break,
        }

        let (enabled, threshold_mb) = {
            let cfg = config.lock();
            (cfg.memory_monitor, cfg.memory_threshold_mb)
        };
        if !enabled { continue; }

        if let Some(t) = last_reload {
            if t.elapsed().as_secs() < COOLDOWN_SECS { continue; }
        }

        let phase = match api.get_gameflow_phase().await {
            Ok(p) => p,
            Err(_) => continue,
        };
        if !SAFE_PHASES.contains(&phase.as_str()) { continue; }

        let mem_mb = get_lcu_ux_memory_mb();
        if mem_mb == 0 || mem_mb < threshold_mb { continue; }

        warn!("LeagueClientUx 内存 {mem_mb} MB 超过阈值 {threshold_mb} MB，触发自动热重载...");
        if api.reload_ux().await.is_ok() {
            info!("内存超限热重载已触发");
            last_reload = Some(std::time::Instant::now());
        }
    }
}

fn get_lcu_ux_memory_mb() -> u64 {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        false,
        ProcessRefreshKind::new().with_memory(),
    );
    for process in sys.processes().values() {
        if process.name().to_string_lossy().to_lowercase().contains("leagueclientux") {
            return process.memory() / 1_048_576;
        }
    }
    0
}
