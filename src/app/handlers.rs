//! 游戏事件处理器

use std::time::{Duration, Instant};
use serde_json::Value;
use tokio::time::sleep;
use tracing::{info, error, debug};
use windows::Win32::Foundation::HWND;

use crate::lcu::api::{gameflow, LcuClient};
use crate::win::overlay::{OverlayCmd, OverlaySender};
use crate::app::premade::{analyze_premade, extract_teams_from_session, extract_teams_from_gameflow_session, format_premade_message};
use crate::app::config::SharedConfig;
use crate::app::prophet;
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

    // 1. 根据阶段设置可见性：只要有阶段就显示，除非阶段为 None 且无强制显示
    if phase == gameflow::NONE {
        if !is_overlay_forced() {
            let _ = overlay_tx.send(OverlayCmd::Hide).await;
        }
    } else {
        let _ = overlay_tx.send(OverlayCmd::Show).await;
    }

    // 2. 状态文字更新
    match phase {
        gameflow::CHAMP_SELECT | gameflow::IN_PROGRESS | gameflow::GAME_START => {}
        _ => {
            let api_c = api.clone();
            let tx_c = overlay_tx.clone();
            let phase_str = phase.to_owned();
            tokio::spawn(async move {
                let mut status_msg = format!("状态: {phase_str}");
                if phase_str == gameflow::MATCHMAKING || phase_str == gameflow::READY_CHECK {
                    if let Ok(session) = api_c.get_gameflow_session().await {
                        if let Some(queue) = session.get("gameData").and_then(|v| v.get("queue_info")).and_then(|v| v.get("description")).and_then(|v| v.as_str()) {
                            status_msg = format!("状态: {} ({})", phase_str, queue);
                        }
                    }
                }
                let _ = tx_c.send(OverlayCmd::UpdateHud(status_msg, String::new())).await;
            });

            {
                let mut s = state.lock();
                s.premade_analysis_done = false;
                s.premade_ingame_done = false;
            }
            let _ = overlay_tx.send(OverlayCmd::ClearHud).await;
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
                        // --- 组黑分析 ---
                        let (my_res, their_res) = analyze_premade(&api2, my_team.clone(), their_team.clone(), 2, 20).await;
                        let premade_msg = format_premade_message(&my_res, &their_res, my_side, their_side);
                        let _ = tx2.send(OverlayCmd::UpdateHud(String::new(), premade_msg)).await;
                        
                        // --- Prophet 评分分析 (进游戏后显示双方) ---
                        // 定义一个小闭包来分析整队
                        let fetch_ratings = |api_c: LcuClient, players: Vec<(String, String)>| async move {
                            let mut results = Vec::new();
                            for (puuid, name) in players {
                                if let Ok(history) = api_c.get_match_history(&puuid, 8).await {
                                    let games = history.get("games").and_then(|v| v.as_array())
                                        .or_else(|| history.get("games").and_then(|v| v.get("games")).and_then(|v| v.as_array()));
                                    if let Some(matches) = games {
                                        if let Some(rating) = prophet::calculate_player_rating(&puuid, matches) {
                                            let grade = prophet::get_grade_name(rating.score);
                                            results.push(format!("{} {} 评分:{:.0} KDA:{:.1}", grade, name, rating.score, rating.avg_kda));
                                        }
                                    }
                                }
                            }
                            results
                        };

                        let my_prophet = fetch_ratings(api2.clone(), my_team).await;
                        let their_prophet = fetch_ratings(api2.clone(), their_team).await;

                        let mut prophet_msg = String::new();
                        if !my_prophet.is_empty() {
                            prophet_msg.push_str(&format!("[我方评分]\n{}\n", my_prophet.join("\n")));
                        }
                        if !their_prophet.is_empty() {
                            if !prophet_msg.is_empty() { prophet_msg.push_str("\n"); }
                            prophet_msg.push_str(&format!("[对方评分]\n{}\n", their_prophet.join("\n")));
                        }
                        
                        if !prophet_msg.is_empty() {
                            let _ = tx2.send(OverlayCmd::UpdateProphet(prophet_msg)).await;
                        }

                        // --- 2 分钟自动隐藏逻辑 ---
                        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                        
                        // 隐藏窗口（不清空内容，以便 F1 唤起）
                        let _ = tx2.send(OverlayCmd::Hide).await;
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

    // 1. 始终显示 HUD 且开启背景 (尊重强制显示变量)
    let forced = is_overlay_forced();
    let is_aram = session.get("benchEnabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let _ = overlay_tx.send(OverlayCmd::Show).await;
    let _ = overlay_tx.send(OverlayCmd::ShowBench(forced || is_aram)).await;

    // 同步板凳席 ID 列表到状态中
    let bench_ids = LcuClient::extract_bench_champion_ids(&session);
    {
        state.lock().current_bench_ids = bench_ids;
    }

    // 2. 组队分析与战绩评分（仅分析一次）
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
            
            // --- 组黑分析 ---
            let my_team_p = my_raw.iter().map(|(p, n, _)| (p.clone(), n.clone())).collect();
            let their_team_p = their_raw.iter().map(|(p, n, _)| (p.clone(), n.clone())).collect();
            let (my_res, their_res) = analyze_premade(&api_c, my_team_p, their_team_p, 3, 20).await;
            let premade_msg = format_premade_message(&my_res, &their_res, my_side, their_side);
            let _ = tx_c.send(OverlayCmd::UpdateHud(String::new(), premade_msg)).await;

            // --- Prophet 评分分析 ---
            let mut my_prophet = Vec::new();
            for (puuid, name, _) in &my_raw {
                if let Ok(history) = api_c.get_match_history(puuid, 8).await {
                    let games = history.get("games").and_then(|v| v.as_array())
                        .or_else(|| history.get("games").and_then(|v| v.get("games")).and_then(|v| v.as_array()));
                    if let Some(matches) = games {
                        if let Some(rating) = prophet::calculate_player_rating(puuid, matches) {
                            let grade = prophet::get_grade_name(rating.score);
                            my_prophet.push(format!("{} {} 评分:{:.0} KDA:{:.1}", grade, name, rating.score, rating.avg_kda));
                        }
                    }
                }
            }
            
            let mut their_prophet = Vec::new();
            for (puuid, name, _) in &their_raw {
                // 如果 PUUID 看起来无效（如全 0），则跳过
                if puuid.is_empty() || puuid.starts_with('0') || name.contains("Summoner") { continue; }
                
                if let Ok(history) = api_c.get_match_history(puuid, 8).await {
                    let games = history.get("games").and_then(|v| v.as_array())
                        .or_else(|| history.get("games").and_then(|v| v.get("games")).and_then(|v| v.as_array()));
                    if let Some(matches) = games {
                        if let Some(rating) = prophet::calculate_player_rating(puuid, matches) {
                            let grade = prophet::get_grade_name(rating.score);
                            their_prophet.push(format!("{} {} 评分:{:.0} KDA:{:.1}", grade, name, rating.score, rating.avg_kda));
                        }
                    }
                }
            }

            let mut final_msg = String::new();
            if !my_prophet.is_empty() {
                final_msg.push_str(&format!("[我方评分]\n{}\n", my_prophet.join("\n")));
            }
            if !their_prophet.is_empty() {
                if !final_msg.is_empty() { final_msg.push_str("\n"); }
                final_msg.push_str(&format!("[对方评分]\n{}\n", their_prophet.join("\n")));
            }

            if !final_msg.is_empty() {
                let _ = tx_c.send(OverlayCmd::UpdateProphet(final_msg)).await;
            }
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

// ── handle_find_forgotten_loot ───────────────────────────────────

pub async fn handle_find_forgotten_loot(api: LcuClient) {
    let loot_list = match api.get_player_loot().await {
        Ok(v) => v,
        Err(e) => { error!("获取战利品失败: {e}"); return; }
    };

    let Some(loots) = loot_list.as_array() else { return; };
    let mut claimable = Vec::new();

    for loot in loots {
        let loot_id = loot.get("lootId").and_then(|v| v.as_str()).unwrap_or("");
        let count = loot.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
        let loot_name = loot.get("localizedName").and_then(|v| v.as_str())
            .or_else(|| loot.get("localizedDescription").and_then(|v| v.as_str()))
            .unwrap_or(loot_id);

        if count <= 0 { continue; }

        // 识别逻辑：匹配常见的可领取奖励前缀
        // 参考 Akari: CURRENCY_champion_faceoff, REWARD_..., CHEST_...
        let is_reward = loot_id.starts_with("REWARD_") 
            || loot_id.contains("champion_faceoff")
            || loot_id.starts_with("CHEST_")
            || loot_id.contains("_REWARD");

        if is_reward {
            // 尝试寻找配方：Akari 常用的是 CHEST_generic_OPEN 或 REWARD_claim
            let recipe = if loot_id.starts_with("CHEST_") { "CHEST_generic_OPEN" } else { "REWARD_claim" };
            claimable.push((loot_id.to_owned(), loot_name.to_owned(), recipe.to_owned(), count));
        }
    }

    if claimable.is_empty() {
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::*;
            use windows::core::PCWSTR;
            let text = crate::win::winapi::to_wide("没有发现可领取的遗忘资源。");
            let caption = crate::win::winapi::to_wide("战利品检查");
            MessageBoxW(HWND::default(), PCWSTR(text.as_ptr()), PCWSTR(caption.as_ptr()), MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND);
        }
        return;
    }

    let mut list_str = String::new();
    for (_, name, _, count) in &claimable {
        list_str.push_str(&format!(" - {} (数量: {})\n", name, count));
    }

    let msg = format!("发现以下可领取资源：\n\n{}\n是否立即找回？", list_str);
    
    let confirm = unsafe {
        use windows::Win32::UI::WindowsAndMessaging::*;
        use windows::core::PCWSTR;
        let text = crate::win::winapi::to_wide(&msg);
        let caption = crate::win::winapi::to_wide("找回遗忘的东西");
        let res = MessageBoxW(HWND::default(), PCWSTR(text.as_ptr()), PCWSTR(caption.as_ptr()), MB_OKCANCEL | MB_ICONQUESTION | MB_SETFOREGROUND);
        res == IDOK
    };

    if confirm {
        info!("正在开始找回战利品...");
        for (id, name, recipe, _) in claimable {
            debug!("正在领取: {} (ID: {}, 配方: {})", name, id, recipe);
            let _ = api.call_loot_recipe(&id, &recipe).await;
        }
        info!("找回任务执行完毕。");
    }
}

// ── handle_lobby ─────────────────────────────────────────────────

pub async fn handle_lobby(
    _api: LcuClient,
    _state: SharedState,
    _config: SharedConfig,
    _overlay_tx: OverlaySender,
    _event: Value,
) {
    // 房间成员显示逻辑已移除
}
