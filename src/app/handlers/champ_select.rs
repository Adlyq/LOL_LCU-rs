use serde_json::Value;
use tokio::task::JoinSet;

use crate::lcu::api::LcuClient;
use crate::win::overlay::{OverlayCmd, OverlaySender};
use crate::app::premade::{analyze_premade, extract_teams_from_session, format_premade_message};
use crate::app::config::SharedConfig;
use crate::app::prophet;
use crate::app::state::SharedState;

use super::utils::{event_data, is_overlay_forced};

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
            let (my_raw, _their_raw, my_side, their_side) = extract_teams_from_session(&session);
            
            // --- 组黑分析 (选人阶段仅分析我方) ---
            let my_team_p = my_raw.iter().map(|(p, n, _)| (p.clone(), n.clone())).collect();
            let (my_res, their_res) = analyze_premade(&api_c, my_team_p, Vec::new(), 3, 20).await;
            let premade_msg = format_premade_message(&my_res, &their_res, my_side, their_side);
            let _ = tx_c.send(OverlayCmd::UpdateHud(String::new(), premade_msg)).await;

            // --- Prophet 评分分析 (选人阶段仅分析我方) ---
            // 立即发送一次初始占位信息
            let mut init_msg = String::new();
            let my_init: Vec<String> = my_raw.iter().map(|(_, name, _)| format!("-- {} 评分:加载中...", name)).collect();
            let my_label = if my_side == Some(100) { "[蓝方评分]" } else { "[红方评分]" };
            init_msg.push_str(&format!("{}\n{}\n", my_label, my_init.join("\n")));
            let _ = tx_c.send(OverlayCmd::UpdateProphet(init_msg)).await;

            let mut my_prophet_results = vec![None; my_raw.len()];
            let mut my_set = JoinSet::new();
            // ... (省略 spawn 逻辑)
            for (idx, (puuid, name, _)) in my_raw.iter().enumerate() {
                let api_cc = api_c.clone();
                let puuid_cc = puuid.clone();
                let name_cc = name.clone();
                my_set.spawn(async move {
                    let mut res = format!("-- {} 评分:获取失败", name_cc);
                    tracing::info!("开始获取我方玩家战绩: {}", name_cc);
                    if let Ok(history) = api_cc.get_match_history(&puuid_cc, 8).await {
                        let games = history.get("games").and_then(|v| v.as_array())
                            .or_else(|| history.get("games").and_then(|v| v.get("games")).and_then(|v| v.as_array()));
                        if let Some(matches) = games {
                            tracing::info!("玩家 {} 战绩拉取成功，共 {} 场", name_cc, matches.len());
                            if let Some(rating) = prophet::calculate_player_rating(&puuid_cc, matches) {
                                let grade = prophet::get_grade_name(rating.score);
                                res = format!("{} {} 评分:{:.0} KDA:{:.1} 胜率:{:.0}%", grade, name_cc, rating.score, rating.avg_kda, rating.win_rate * 100.0);
                                tracing::info!("玩家 {} 评分计算完成: {:.0}", name_cc, rating.score);
                            } else {
                                tracing::warn!("玩家 {} 评分计算返回 None", name_cc);
                            }
                        } else {
                            tracing::warn!("玩家 {} 战绩数据解析失败 (缺少 games 字段)", name_cc);
                        }
                    } else {
                        tracing::error!("玩家 {} 战绩接口请求失败 (重试后)", name_cc);
                    }
                    (idx, res)
                });
            }

            // 动态更新，每完成一个玩家的抓取就刷新一次 UI
            loop {
                tokio::select! {
                    Some(Ok((idx, res))) = my_set.join_next() => {
                        my_prophet_results[idx] = Some(res);
                    }
                    else => break,
                }

                let mut final_msg = String::new();
                let my_lines: Vec<String> = my_raw.iter().enumerate().map(|(i, (_, name, _))| {
                    my_prophet_results[i].clone().unwrap_or_else(|| format!("-- {} 评分:加载中...", name))
                }).collect();
                final_msg.push_str(&format!("{}\n{}\n", my_label, my_lines.join("\n")));
                let _ = tx_c.send(OverlayCmd::UpdateProphet(final_msg)).await;
            }
        });
    }
}
