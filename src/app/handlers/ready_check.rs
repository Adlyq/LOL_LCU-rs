use std::time::Duration;
use serde_json::Value;
use tokio::time::sleep;
use tracing::{info, error};

use crate::lcu::api::LcuClient;
use crate::app::config::SharedConfig;
use crate::app::state::SharedState;

use super::utils::event_data;

pub async fn handle_ready_check(
    api: LcuClient,
    state: SharedState,
    config: SharedConfig,
    event: Value,
) {
    let data = match event_data(&event) {
        Some(d) => d.clone(),
        None => return,
    };

    let rc_state = data.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let player_response = data.get("playerResponse").and_then(|v| v.as_str());
    let event_id = data.get("id").and_then(|v| v.as_i64());

    if rc_state != "InProgress" || player_response.map(|r| r != "None").unwrap_or(false) {
        state.lock().cancel_ready_check();
        return;
    }

    let (enabled, delay_secs) = {
        let cfg = config.lock();
        (cfg.auto_accept_enabled, cfg.auto_accept_delay_secs)
    };
    if !enabled { return; }

    let already_pending = {
        let s = state.lock();
        s.ready_check_pending_accept
    };
    if already_pending { return; }

    let generation = state.lock().start_ready_check();

    info!("检测到 Ready Check，{} 秒后自动接受...", delay_secs);
    sleep(Duration::from_secs(delay_secs)).await;

    {
        let s = state.lock();
        if !s.ready_check_pending_accept || s.ready_check_generation != generation { return; }
    }

    let current = match api.get_ready_check().await {
        Ok(v) => v,
        Err(_) => { state.lock().ready_check_pending_accept = false; return; }
    };

    if current.get("state").and_then(|v| v.as_str()) != Some("InProgress") {
        state.lock().ready_check_pending_accept = false;
        return;
    }

    if let (Some(ev_id), Some(c_id)) = (event_id, current.get("id").and_then(|v| v.as_i64())) {
        if ev_id != c_id { state.lock().ready_check_pending_accept = false; return; }
    }

    if let Err(e) = api.accept_ready_check().await {
        error!("自动接受 Ready Check 失败: {e}");
    } else {
        info!("Ready Check 已自动接受");
    }
    state.lock().ready_check_pending_accept = false;
}
