use serde_json::Value;

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
