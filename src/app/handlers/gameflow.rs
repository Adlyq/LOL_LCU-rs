use serde_json::Value;

use crate::lcu::api::{gameflow, LcuClient};
use crate::win::overlay::{OverlayCmd, OverlaySender};
use crate::app::premade::{analyze_premade, extract_teams_from_gameflow_session, format_premade_message};
use crate::app::config::SharedConfig;
use crate::app::prophet;
use crate::app::state::SharedState;

use super::utils::is_overlay_forced;

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
        gameflow::CHAMP_SELECT | gameflow::IN_PROGRESS | gameflow::GAME_START => {
            // 阶段切换瞬间主动触发一次窗口比例修复
            let api_c = api.clone();
            let tx_c = overlay_tx.clone();
            tokio::spawn(async move {
                if let Ok(zoom) = api_c.get_riotclient_zoom_scale().await {
                    let _ = tx_c.send(OverlayCmd::AutoFixWindow(zoom, false)).await;
                }
            });
        }
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
                state.lock().reset_premade_status();
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
