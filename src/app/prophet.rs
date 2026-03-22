//! 英雄先知 (Prophet) 评分系统
//! 移植自：https://github.com/real-web-world/hh-lol-prophet

use serde_json::Value;

// ── 常量配置 ─────────────────────────────────────────────────────

const BASE_SCORE: f64 = 100.0;
const RECENT_WINDOW_MS: i64 = 5 * 60 * 60 * 1000; // 5 小时
const RECENT_WEIGHT: f64 = 0.8;
const OLD_WEIGHT: f64 = 0.2;

pub fn get_grade_name(score: f64) -> &'static str {
    if score >= 180.0 { "通天代" }
    else if score >= 150.0 { "小代" }
    else if score >= 125.0 { "上等马" }
    else if score >= 105.0 { "中等马" }
    else if score >= 95.0 { "下等马" }
    else if score >= 80.0 { "纯牛马" }
    else { "没有马" }
}

// ── 数据模型 ─────────────────────────────────────────────────────

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct PlayerPerformance {
    pub puuid: String,
    pub name: String,
    pub score: f64,
    pub avg_kda: f64,
    pub win_rate: f64,
    pub count: usize,
}

struct RawMatchStats {
    is_recent: bool,
    base_score: f64,
}

// ── 核心逻辑 ─────────────────────────────────────────────────────

pub fn calculate_player_rating(puuid: &str, matches: &[Value]) -> Option<PlayerPerformance> {
    if matches.is_empty() { return None; }

    let mut match_results = Vec::new();
    let mut total_kda = 0.0;
    let mut wins = 0.0;
    let mut valid_count = 0;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    for m in matches {
        // 提取统计数据
        let game_creation = m.get("gameCreation").and_then(|v| v.as_i64()).unwrap_or(0);
        let participants = m.get("participants").and_then(|v| v.as_array())?;
        let identities = m.get("participantIdentities").and_then(|v| v.as_array())?;

        // 找到目标玩家的 participantId
        let pid = identities.iter().find(|id| {
            id.get("player").and_then(|p| p.get("puuid")).and_then(|p| p.as_str()) == Some(puuid)
        })?.get("participantId")?.as_i64()?;

        // 找到目标玩家的 stats
        let me = participants.iter().find(|p| p.get("participantId").and_then(|v| v.as_i64()) == Some(pid))?;
        let stats = me.get("stats")?;
        
        let win = stats.get("win").and_then(|v| v.as_bool()).unwrap_or(false);
        let kills = stats.get("kills").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let deaths = stats.get("deaths").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let assists = stats.get("assists").and_then(|v| v.as_f64()).unwrap_or(0.0);
        
        // 计算该场基础分
        let mut score = BASE_SCORE;
        
        // KDA 计算
        let kda = (kills + assists) / deaths.max(1.0);
        total_kda += kda;
        if win { wins += 1.0; }
        valid_count += 1;

        // 简化的评分逻辑（对应 JS 的核心思想）
        score += kda * 5.0; // KDA 贡献
        if stats.get("firstBloodKill").and_then(|v| v.as_bool()).unwrap_or(false) { score += 10.0; }
        
        // 多杀奖励
        score += stats.get("tripleKills").and_then(|v| v.as_i64()).unwrap_or(0) as f64 * 5.0;
        score += stats.get("quadraKills").and_then(|v| v.as_i64()).unwrap_or(0) as f64 * 10.0;
        score += stats.get("pentaKills").and_then(|v| v.as_i64()).unwrap_or(0) as f64 * 20.0;

        match_results.push(RawMatchStats {
            is_recent: (now_ms - game_creation) < RECENT_WINDOW_MS,
            base_score: score,
        });
    }

    if valid_count == 0 { return None; }

    // 按权重合并分数
    let recent: Vec<_> = match_results.iter().filter(|m| m.is_recent).collect();
    let old: Vec<_> = match_results.iter().filter(|m| !m.is_recent).collect();

    let final_score = if recent.is_empty() {
        old.iter().map(|m| m.base_score).sum::<f64>() / old.len() as f64
    } else if old.is_empty() {
        recent.iter().map(|m| m.base_score).sum::<f64>() / recent.len() as f64
    } else {
        let avg_recent = recent.iter().map(|m| m.base_score).sum::<f64>() / recent.len() as f64;
        let avg_old = old.iter().map(|m| m.base_score).sum::<f64>() / old.len() as f64;
        avg_recent * RECENT_WEIGHT + avg_old * OLD_WEIGHT
    };

    Some(PlayerPerformance {
        puuid: puuid.to_owned(),
        name: String::new(), // 由外部填充
        score: final_score,
        avg_kda: total_kda / valid_count as f64,
        win_rate: wins / valid_count as f64,
        count: valid_count,
    })
}
