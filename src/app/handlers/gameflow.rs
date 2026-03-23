use serde_json::Value;
use tokio::task::JoinSet;

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
                        let mut my_prophet_results = vec![None; my_team.len()];
                        let mut their_prophet_results = vec![None; their_team.len()];
                        
                        let mut my_set = JoinSet::new();
                        for (idx, (puuid, name)) in my_team.iter().enumerate() {
                            let api_cc = api2.clone();
                            let puuid_cc = puuid.clone();
                            let name_cc = name.clone();
                            my_set.spawn(async move {
                                let mut res = format!("-- {} 评分:获取失败", name_cc);
                                if let Ok(history) = api_cc.get_match_history(&puuid_cc, 8).await {
                                    let games = history.get("games").and_then(|v| v.as_array())
                                        .or_else(|| history.get("games").and_then(|v| v.get("games")).and_then(|v| v.as_array()));
                                    if let Some(matches) = games {
                                        if let Some(rating) = prophet::calculate_player_rating(&puuid_cc, matches) {
                                            let grade = prophet::get_grade_name(rating.score);
                                            res = format!("{} {} 评分:{:.0} KDA:{:.1}", grade, name_cc, rating.score, rating.avg_kda);
                                        }
                                    }
                                }
                                (idx, res)
                            });
                        }

                        let mut their_set = JoinSet::new();
                        for (idx, (puuid, name)) in their_team.iter().enumerate() {
                            let api_cc = api2.clone();
                            let puuid_cc = puuid.clone();
                            let name_cc = name.clone();
                            their_set.spawn(async move {
                                let mut res = format!("-- {} 评分:获取失败", name_cc);
                                if let Ok(history) = api_cc.get_match_history(&puuid_cc, 8).await {
                                    let games = history.get("games").and_then(|v| v.as_array())
                                        .or_else(|| history.get("games").and_then(|v| v.get("games")).and_then(|v| v.as_array()));
                                    if let Some(matches) = games {
                                        if let Some(rating) = prophet::calculate_player_rating(&puuid_cc, matches) {
                                            let grade = prophet::get_grade_name(rating.score);
                                            res = format!("{} {} 评分:{:.0} KDA:{:.1}", grade, name_cc, rating.score, rating.avg_kda);
                                        }
                                    }
                                }
                                (idx, res)
                            });
                        }

                        // 动态更新循环
                        let tx2_c = tx2.clone();
                        let my_team_c = my_team.clone();
                        let their_team_c = their_team.clone();
                        tokio::spawn(async move {
                            loop {
                                tokio::select! {
                                    Some(Ok((idx, res))) = my_set.join_next() => {
                                        my_prophet_results[idx] = Some(res);
                                    }
                                    Some(Ok((idx, res))) = their_set.join_next() => {
                                        their_prophet_results[idx] = Some(res);
                                    }
                                    else => break,
                                }

                                let mut prophet_msg = String::new();
                                let my_lines: Vec<String> = my_team_c.iter().enumerate().map(|(i, (_, name))| {
                                    my_prophet_results[i].clone().unwrap_or_else(|| format!("-- {} 评分:加载中...", name))
                                }).collect();
                                prophet_msg.push_str(&format!("[我方评分]\n{}\n", my_lines.join("\n")));

                                let their_lines: Vec<String> = their_team_c.iter().enumerate().map(|(i, (_, name))| {
                                    their_prophet_results[i].clone().unwrap_or_else(|| format!("-- {} 评分:加载中...", name))
                                }).collect();
                                if !their_lines.is_empty() {
                                    prophet_msg.push_str(&format!("\n[对方评分]\n{}\n", their_lines.join("\n")));
                                }
                                let _ = tx2_c.send(OverlayCmd::UpdateProphet(prophet_msg)).await;
                            }
                        });

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
