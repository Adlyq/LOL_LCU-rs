//! 游戏事件处理器

use std::time::{Duration, Instant};
use serde_json::Value;
use tokio::time::sleep;
use tracing::info;

use crate::lcu::api::{gameflow, LcuClient};
use crate::win::overlay::{OverlayCmd, OverlaySender};
use crate::app::premade::{analyze_premade, extract_teams_from_session, extract_teams_from_gameflow_session, format_premade_message};
use crate::app::config::SharedConfig;
use super::state::SharedState;

const POSTGAME_CONTINUE_DELAY_SECS: f64 = 0.8;

fn event_data(event: &Value) -> Option<&Value> {
    let data = event.get("data")?;
    if data.is_object() || data.is_string() { Some(data) } else { None }
}

fn is_overlay_forced() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var("LOL_LCU_SHOW_OVERLAY").is_ok()
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

// ── handle_ready_check ───────────────────────────────────────────

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
        let mut s = state.lock();
        s.ready_check_pending_accept = false;
        s.ready_check_generation += 1;
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

    let generation = {
        let mut s = state.lock();
        s.ready_check_pending_accept = true;
        s.ready_check_generation
    };

    info!("检测到 Ready Check，{delay_secs} 秒后自动接受...");
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

    let _ = api.accept_ready_check().await;
    state.lock().ready_check_pending_accept = false;
}

// ── handle_gameflow ──────────────────────────────────────────────

pub async fn handle_gameflow(
    api: LcuClient,
    state: SharedState,
    config: SharedConfig,
    overlay_tx: OverlaySender,
    event: Value,
) {
    let phase = event.get("data").and_then(|v| v.as_str()).unwrap_or(gameflow::NONE);

    // 在非 release 模式下支持环境变量强制显示 Overlay
    if is_overlay_forced() {
        let _ = overlay_tx.send(OverlayCmd::ShowBench(true)).await;
    }

    match phase {
        gameflow::CHAMP_SELECT | gameflow::IN_PROGRESS | gameflow::GAME_START => {}
        _ => {
            let _ = overlay_tx.send(OverlayCmd::UpdateHud(format!("状态: {phase}"), String::new())).await;
            if !is_overlay_forced() {
                let _ = overlay_tx.send(OverlayCmd::ShowBench(false)).await;
            }
            let mut s = state.lock();
            s.premade_analysis_done = false;
            s.premade_ingame_done = false;
        }
    }

    if phase == gameflow::IN_PROGRESS || phase == gameflow::GAME_START {
        let should_analyze = {
            let s = state.lock();
            !s.premade_ingame_done
        };
        if should_analyze && config.lock().premade_ingame {
            state.lock().premade_ingame_done = true;
            let api2 = api.clone();
            let tx2 = overlay_tx.clone();
            tokio::spawn(async move {
                if let Ok(session) = api2.get_gameflow_session().await {
                    let me = api2.get_current_summoner().await.unwrap_or_default();
                    let my_puuid = me.get("puuid").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                    let id_name = api2.get_champion_id_name_map().await.unwrap_or_default();
                    let (my_team, their_team, my_side, their_side) = extract_teams_from_gameflow_session(&session, &my_puuid, &id_name);
                    
                    if !my_team.is_empty() || !their_team.is_empty() {
                        let (my_res, their_res) = analyze_premade(&api2, my_team, their_team, 2, 20).await;
                        let msg = format_premade_message(&my_res, &their_res, my_side, their_side);
                        let _ = tx2.send(OverlayCmd::UpdateHud(String::new(), msg.clone())).await;
                        let _ = api2.send_message_to_self(&msg).await;
                    }
                }
            });
        }
    }
}

// ── handle_honor_ballot ──────────────────────────────────────────

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

// ── handle_champ_select ──────────────────────────────────────────

pub async fn handle_champ_select(
    api: LcuClient,
    state: SharedState,
    config: SharedConfig,
    overlay_tx: OverlaySender,
    event: Value,
) {
    let session = match event_data(&event) {
        Some(d) => d.clone(),
        None => return,
    };

    // 1. 核心显示逻辑（极速触发表率）
    let bench = session.get("benchChampions").and_then(|v| v.as_array());
    let forced = is_overlay_forced();

    if let Some(bench_arr) = bench {
        let count = bench_arr.len();
        // 立即发指令显示 HUD
        let _ = overlay_tx.send(OverlayCmd::ShowBench(forced || count > 0)).await;

        // 异步解析英雄名称，不阻塞显示
        let api_c = api.clone();
        let state_c = state.clone();
        let tx_c = overlay_tx.clone();
        let bench_arr_c = bench_arr.clone();

        tokio::spawn(async move {
            // 获取/更新缓存
            let map = {
                let cache = state_c.lock().champion_id_name_map.clone();
                if cache.is_empty() {
                    if let Ok(m) = api_c.get_champion_id_name_map().await {
                        state_c.lock().champion_id_name_map = m.clone();
                        m
                    } else { cache }
                } else { cache }
            };

            let mut names = Vec::new();
            for hero in bench_arr_c {
                if let Some(id) = hero.get("championId").and_then(|v| v.as_i64()) {
                    names.push(map.get(&id).cloned().unwrap_or_else(|| format!("Hero-{id}")));
                }
            }
            
            if !names.is_empty() {
                let msg = format!("板凳席: {}", names.join(" / "));
                let _ = tx_c.send(OverlayCmd::UpdateHud(String::new(), msg)).await;
            } else {
                let _ = tx_c.send(OverlayCmd::UpdateHud(String::new(), String::new())).await;
            }
        });
    } else {
        let _ = overlay_tx.send(OverlayCmd::ShowBench(forced)).await;
    }

    // 2. 组队分析（完全异步处理）
    let should_analyze = {
        let s = state.lock();
        !s.premade_analysis_done
    };
    if should_analyze && config.lock().premade_champ_select {
        state.lock().premade_analysis_done = true;
        let api_c = api.clone();
        let tx_c = overlay_tx.clone();
        tokio::spawn(async move {
            let (my_raw, their_raw, my_side, their_side) = extract_teams_from_session(&session);
            let my_team = my_raw.into_iter().map(|(p, n, _)| (p, n)).collect();
            let their_team = their_raw.into_iter().map(|(p, n, _)| (p, n)).collect();

            let (my_res, their_res) = analyze_premade(&api_c, my_team, their_team, 3, 20).await;
            let msg = format_premade_message(&my_res, &their_res, my_side, their_side);
            let _ = tx_c.send(OverlayCmd::UpdateHud(String::new(), msg.clone())).await;
            let _ = api_c.send_message_to_self(&msg).await;
        });
    }
}

// ── handle_lobby ─────────────────────────────────────────────────

pub async fn handle_lobby(
    _api: LcuClient,
    _state: SharedState,
    _config: SharedConfig,
    overlay_tx: OverlaySender,
    event: Value,
) {
    let data = match event_data(&event) {
        Some(d) => d,
        None => return,
    };

    if let Some(members) = data.get("members").and_then(|v| v.as_array()) {
        let mut names = Vec::new();
        for m in members {
            // 尝试获取各种可能的昵称字段
            let game_name = m.get("gameName").and_then(|v| v.as_str());
            let tag_line = m.get("tagLine").and_then(|v| v.as_str());
            let summoner_name = m.get("summonerName").and_then(|v| v.as_str());
            
            let name = if let (Some(gn), Some(tl)) = (game_name, tag_line) {
                if gn.is_empty() { summoner_name.unwrap_or("未知").to_owned() }
                else { format!("{}#{}", gn, tl) }
            } else {
                summoner_name.unwrap_or("未知").to_owned()
            };
            names.push(name);
        }
        
        if !names.is_empty() {
            let lobby_msg = format!("房间成员: {}", names.join(" / "));
            let _ = overlay_tx.send(OverlayCmd::UpdateHud(lobby_msg, String::new())).await;
        }
    }
}
