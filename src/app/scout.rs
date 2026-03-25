use tokio::task::JoinSet;
use tracing::{info, warn, debug};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::lcu::api::LcuClient;
use crate::app::event::AppEvent;
use crate::app::prophet;
use crate::app::premade::{analyze_premade, format_premade_message};

pub struct ScoutService {
    api: LcuClient,
    event_tx: mpsc::Sender<AppEvent>,
}

impl ScoutService {
    pub fn new(api: LcuClient, event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self { api, event_tx }
    }

    /// 执行完整的对局分析 (组黑 + 战绩)
    pub async fn execute_full_scout(
        &self,
        my_team: Vec<(String, String)>,
        their_team: Vec<(String, String)>,
        my_side: Option<u32>,
        their_side: Option<u32>,
        token: CancellationToken,
    ) {
        let api = self.api.clone();
        let event_tx = self.event_tx.clone();

        info!("[Scout] 启动全量分析: 我方 {} 人, 对方 {} 人", my_team.len(), their_team.len());

        tokio::spawn(async move {
            // 1. 组黑分析
            if token.is_cancelled() { return; }
            
            let (my_res, their_res) = analyze_premade(&api, my_team.clone(), their_team.clone(), 2, 20).await;
            let premade_msg = format_premade_message(&my_res, &their_res, my_side, their_side);
            
            if token.is_cancelled() { return; }
            let _ = event_tx.send(AppEvent::ScoutResult {
                puuid: "TEAM_PREMADE".to_string(),
                content: premade_msg,
                is_premade: true,
                is_enemy: false,
            }).await;

            // 2. Prophet 评分分析
            let mut set = JoinSet::new();
            
            for (puuid, name) in my_team {
                let api_c = api.clone();
                let t_c = token.clone();
                set.spawn(async move {
                    if t_c.is_cancelled() { return None; }
                    Some((puuid.clone(), name.clone(), Self::scout_player(&api_c, &puuid, &name).await, false))
                });
            }
            
            for (puuid, name) in their_team {
                let api_c = api.clone();
                let t_c = token.clone();
                set.spawn(async move {
                    if t_c.is_cancelled() { return None; }
                    Some((puuid.clone(), name.clone(), Self::scout_player(&api_c, &puuid, &name).await, true))
                });
            }

            while let Some(Ok(res)) = set.join_next().await {
                if token.is_cancelled() { break; }
                if let Some((puuid, _name, content, is_enemy)) = res {
                    let _ = event_tx.send(AppEvent::ScoutResult {
                        puuid,
                        content,
                        is_premade: false,
                        is_enemy,
                    }).await;
                }
            }
            info!("[Scout] 分析任务结束");
        });
    }

    async fn scout_player(api: &LcuClient, puuid: &str, name: &str) -> String {
        if let Ok(history) = api.get_match_history(puuid, 8).await {
            let games = history.get("games")
                .and_then(|v| if v.is_array() { Some(v) } else { v.get("games") })
                .and_then(|v| v.as_array());
                
            if let Some(matches) = games {
                if let Some(rating) = prophet::calculate_player_rating(puuid, matches) {
                    let grade = prophet::get_grade_name(rating.score);
                    return format!("{} {} 评分:{:.0} KDA:{:.1} 胜率:{:.0}%", grade, name, rating.score, rating.avg_kda, rating.win_rate * 100.0);
                }
            }
        }
        format!("-- {} 评分:获取失败", name)
    }
}
