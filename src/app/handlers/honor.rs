use std::time::{Duration, Instant};
use serde_json::Value;
use tokio::time::sleep;
use tracing::info;

use crate::lcu::api::{gameflow, LcuClient};
use crate::win::overlay::OverlaySender;
use crate::app::config::SharedConfig;
use crate::app::state::SharedState;

use super::utils::event_data;

const POSTGAME_CONTINUE_DELAY_SECS: f64 = 0.8;

pub async fn handle_honor_ballot(
    api: LcuClient,
    state: SharedState,
    config: SharedConfig,
    _overlay_tx: OverlaySender,
    event: Value,
) {
    if !config.lock().auto_honor_skip { return; }

    let data = event_data(&event);
    let game_id = data.and_then(|d| d.get("gameId")).and_then(|v| v.as_i64());

    if let Some(gid) = game_id {
        if state.lock().last_skipped_honor_game_id == Some(gid) { return; }
    }

    if api.skip_honor_vote().await.unwrap_or(false) {
        let mut s = state.lock();
        s.last_skipped_honor_game_id = game_id;
        s.last_honor_skip_ts = Instant::now();
        info!("已自动跳过点赞");
        
        let api_c = api.clone();
        let state_c = state.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs_f64(POSTGAME_CONTINUE_DELAY_SECS)).await;
            if api_c.get_gameflow_phase().await.unwrap_or_default() == gameflow::END_OF_GAME
                && crate::win::winapi::click_postgame_continue(None) {
                    state_c.lock().last_post_honor_continue_game_id = game_id;
                }
        });
    }
}
