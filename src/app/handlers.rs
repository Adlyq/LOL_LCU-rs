//! 游戏事件处理器

use std::time::{Duration, Instant};
use serde_json::Value;
use tokio::time::sleep;
use tracing::{info, warn, error, debug};

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

    if let Err(e) = api.accept_ready_check().await {
        error!("自动接受 Ready Check 失败: {e}");
    } else {
        info!("Ready Check 已自动接受");
    }
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

    // 1. 根据阶段设置可见性
    if phase == gameflow::CHAMP_SELECT {
        let _ = overlay_tx.send(OverlayCmd::Show).await;
    } else if !is_overlay_forced() {
        match phase {
            gameflow::IN_PROGRESS | gameflow::GAME_START => {}, // 游戏中保持当前状态（可能是显示中）
            _ => {
                let _ = overlay_tx.send(OverlayCmd::Hide).await;
            }
        }
    }

    // 2. 状态文字更新
    match phase {
        gameflow::CHAMP_SELECT | gameflow::IN_PROGRESS | gameflow::GAME_START => {}
        _ => {
            let _ = overlay_tx.send(OverlayCmd::UpdateHud(format!("状态: {phase}"), String::new())).await;
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

                        // 显示 2 分钟后自动隐藏
                        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                        // 完全清空 HUD 文字
                        let _ = tx2.send(OverlayCmd::UpdateHud(String::new(), String::new())).await;
                        // 如果不是强制显示，则关闭背景容器，达到“不显示任何东西”的效果
                        if !is_overlay_forced() {
                            let _ = tx2.send(OverlayCmd::ShowBench(false)).await;
                        }
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

    // 1. 始终显示 HUD 背景 (尊重强制显示变量)
    let forced = is_overlay_forced();
    let is_aram = session.get("benchEnabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let _ = overlay_tx.send(OverlayCmd::ShowBench(forced || is_aram)).await;

    // 同步板凳席 ID 列表到状态中
    let bench_ids = LcuClient::extract_bench_champion_ids(&session);
    {
        state.lock().current_bench_ids = bench_ids;
    }

    // 2. 组队分析并显示结果（不再显示板凳英雄列表）
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
            // 这里 my_raw 已经包含 (puuid, nickname, champion_id)
            let my_team = my_raw.iter().map(|(p, n, _)| (p.clone(), n.clone())).collect();
            let their_team = their_raw.iter().map(|(p, n, _)| (p.clone(), n.clone())).collect();

            let (my_res, their_res) = analyze_premade(&api_c, my_team, their_team, 3, 20).await;
            let msg = format_premade_message(&my_res, &their_res, my_side, their_side);
            
            // 选人界面的主要内容是组黑分析结果（包含昵称）
            let _ = tx_c.send(OverlayCmd::UpdateHud(String::new(), msg.clone())).await;
            let _ = api_c.send_message_to_self(&msg).await;
        });
    }
}

// ── handle_overlay_click ─────────────────────────────────────────

pub async fn handle_overlay_click(
    api: LcuClient,
    state: SharedState,
    overlay_tx: OverlaySender,
    slot_index: usize,
) {
    let action = {
        let mut s = state.lock();
        if slot_index >= s.current_bench_ids.len() {
            return;
        }

        if s.active_pick_slot == Some(slot_index) {
            // 再次点击同槽位：取消
            if let Some(task) = s.pick_task.take() {
                task.abort();
            }
            s.pick_generation += 1;
            s.active_pick_slot = None;
            false // 表示已取消
        } else {
            // 点击新槽位：启动抢人任务
            let champion_id = s.current_bench_ids[slot_index];
            if let Some(task) = s.pick_task.take() {
                task.abort();
            }
            s.pick_generation += 1;
            s.active_pick_slot = Some(slot_index);
            let gen = s.pick_generation;
            
            let api_c = api.clone();
            let state_c = state.clone();
            let tx_c = overlay_tx.clone();
            let handle = tokio::spawn(async move {
                loop_pick_until_refresh(api_c, state_c, tx_c, champion_id, gen, slot_index).await;
            });
            s.pick_task = Some(handle);
            true // 表示新开始
        }
    };

    if action {
        let _ = overlay_tx.send(OverlayCmd::SetSelectedSlot(slot_index)).await;
    } else {
        let _ = overlay_tx.send(OverlayCmd::ClearSelectedSlot).await;
    }
}

async fn loop_pick_until_refresh(
    api: LcuClient,
    state: SharedState,
    overlay_tx: OverlaySender,
    champion_id: i64,
    generation: u64,
    _slot_index: usize,
) {
    loop {
        // 1. 检查代次
        if state.lock().pick_generation != generation { return; }

        // 2. 尝试抢人
        let _ = api.swap_bench_champion(champion_id).await;

        tokio::time::sleep(Duration::from_millis(300)).await;

        // 3. 状态检查
        if let Ok(session) = api.get_champ_select_session().await {
            // 是否已在手？
            if let Some(me) = LcuClient::get_local_player(&session) {
                if me.get("championId").and_then(|v| v.as_i64()) == Some(champion_id) {
                    break;
                }
            }
            // 英雄是否还在板凳？
            let bench = LcuClient::extract_bench_champion_ids(&session);
            if !bench.contains(&champion_id) {
                break;
            }
        } else {
            break; // 获取 session 失败说明选人可能结束了
        }
    }

    // 清理状态
    let mut s = state.lock();
    if s.pick_generation == generation {
        s.active_pick_slot = None;
        let tx = overlay_tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(OverlayCmd::ClearSelectedSlot).await;
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
