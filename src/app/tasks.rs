//! 后台任务集
//!
//! 包括：
//! - LCU 内存监控与自动重载

use std::time::Duration;
use tokio::time::sleep;
use tokio::sync::mpsc;
use tracing::{info, warn};
use tokio_util::sync::CancellationToken;

use crate::lcu::api::LcuClient;
use crate::app::config::SharedConfig;
use crate::app::event::AppEvent;

/// 窗口比例修复循环：定期检查 LCU 缩放并触发修复逻辑。
pub async fn window_fix_loop(api: LcuClient, event_tx: mpsc::Sender<AppEvent>, token: CancellationToken) {
    loop {
        if token.is_cancelled() { break; }

        if let Ok(zoom) = api.get_riotclient_zoom_scale().await {
            let _ = event_tx.send(AppEvent::RequestWindowFix { zoom, forced: false }).await;
        }
        
        tokio::select! {
            _ = sleep(Duration::from_millis(1500)) => {}
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
