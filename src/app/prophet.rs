//! 英雄先知 (Prophet) 评分系统
//! 移植自：https://github.com/real-web-world/hh-lol-prophet

use serde_json::Value;

// ── 常量配置 ─────────────────────────────────────────────────────

// (已移除旧的 hh-lol-prophet 常量)

pub fn get_grade_name(score: f64) -> &'static str {
    // 适配 AkariScore 标准 (通常总分在 15-45 之间)
    if score >= 35.0 {
        "通天代"
    } else if score >= 31.0 {
        "小代"
    } else if score >= 27.0 {
        "上等马"
    } else if score >= 23.0 {
        "中等马"
    } else if score >= 19.0 {
        "下等马"
    } else if score >= 15.0 {
        "纯牛马"
    } else {
        "没有马"
    }
}

// ── 数据模型 ─────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct PlayerPerformance {
    pub puuid: String,
    pub name: String,
    pub score: f64,
    pub avg_kda: f64,
    pub win_rate: f64,
    pub count: usize,
}

/// AkariScore 分项数据
#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct AkariScore {
    pub kda_score: f64,
    pub win_rate_score: f64,
    pub dmg_score: f64,
    pub dmg_taken_score: f64,
    pub cs_score: f64,
    pub gold_score: f64,
    pub participation_score: f64,
    pub total: f64,
}

// ── 核心逻辑 ─────────────────────────────────────────────────────

/// 计算基于 LeagueAkari 详细对局数据的评分。
///
/// 要求 `matches` 是详细的对局对象 (包含所有参与者的 stats)。
pub fn calculate_akari_score(self_puuid: &str, matches: &[Value]) -> Option<AkariScore> {
    if matches.is_empty() {
        return None;
    }

    let mut total_kda = 0.0;
    let mut wins = 0.0;
    let mut total_dmg_share_to_top = 0.0;
    let mut total_dmg_taken_share_to_top = 0.0;
    let mut total_cs_per_min = 0.0;
    let mut total_gold_share_to_top = 0.0;
    let mut total_participation_rate = 0.0;
    let mut valid_count = 0;

    for game in matches {
        let participants = game.get("participants").and_then(|v| v.as_array())?;
        let identities = game.get("participantIdentities").and_then(|v| v.as_array())?;
        let duration = game.get("gameDuration").and_then(|v| v.as_f64()).unwrap_or(1.0);

        // 1. 找到自己
        let pid = identities
            .iter()
            .find(|id| {
                id.get("player")
                    .and_then(|p| p.get("puuid"))
                    .and_then(|p| p.as_str())
                    == Some(self_puuid)
            })?
            .get("participantId")?
            .as_i64()?;

        let me = participants
            .iter()
            .find(|p| p.get("participantId").and_then(|v| v.as_i64()) == Some(pid))?;
        let my_stats = me.get("stats")?;
        let my_team_id = me.get("teamId")?;

        // 2. 统计我方数据
        let mut max_dmg: f64 = 0.0;
        let mut max_dmg_taken: f64 = 0.0;
        let mut max_gold: f64 = 0.0;
        let mut team_kills: f64 = 0.0;

        for p in participants {
            if p.get("teamId") != Some(my_team_id) {
                continue;
            }
            let s = p.get("stats")?;
            max_dmg = max_dmg.max(s.get("totalDamageDealtToChampions").and_then(|v| v.as_f64()).unwrap_or(0.0));
            max_dmg_taken = max_dmg_taken.max(s.get("totalDamageTaken").and_then(|v| v.as_f64()).unwrap_or(0.0));
            max_gold = max_gold.max(s.get("goldEarned").and_then(|v| v.as_f64()).unwrap_or(0.0));
            team_kills += s.get("kills").and_then(|v| v.as_f64()).unwrap_or(0.0);
        }

        // 3. 计算单场数据
        let kills = my_stats.get("kills").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let deaths = my_stats.get("deaths").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let assists = my_stats.get("assists").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let win = my_stats.get("win").and_then(|v| v.as_bool()).unwrap_or(false);
        let dmg = my_stats.get("totalDamageDealtToChampions").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let dmg_taken = my_stats.get("totalDamageTaken").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let gold = my_stats.get("goldEarned").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let cs = my_stats.get("totalMinionsKilled").and_then(|v| v.as_f64()).unwrap_or(0.0) 
                + my_stats.get("neutralMinionsKilled").and_then(|v| v.as_f64()).unwrap_or(0.0);

        total_kda += (kills + assists) / deaths.max(1.0);
        if win { wins += 1.0; }
        total_dmg_share_to_top += dmg / max_dmg.max(1.0);
        total_dmg_taken_share_to_top += dmg_taken / max_dmg_taken.max(1.0);
        total_gold_share_to_top += gold / max_gold.max(1.0);
        total_cs_per_min += cs / (duration / 60.0).max(1.0);
        total_participation_rate += (kills + assists) / team_kills.max(1.0);

        valid_count += 1;
    }

    if valid_count == 0 { return None; }

    let avg_count = valid_count as f64;
    let avg_kda = total_kda / avg_count;
    let win_rate = wins / avg_count;
    let avg_dmg_share = total_dmg_share_to_top / avg_count;
    let avg_dmg_taken_share = total_dmg_taken_share_to_top / avg_count;
    let avg_cs_per_min = total_cs_per_min / avg_count;
    let avg_gold_share = total_gold_share_to_top / avg_count;
    let avg_participation = total_participation_rate / avg_count;

    // LeagueAkari 公式核心
    let kda_score = avg_kda.sqrt() * 1.44;
    let win_rate_score = (win_rate - 0.5) * 4.0;
    let dmg_score = avg_dmg_share * 10.0;
    let dmg_taken_score = avg_dmg_taken_share * 8.0;
    let cs_score = avg_cs_per_min * (0.04 * avg_cs_per_min).clamp(0.1, 0.4);
    let gold_score = avg_gold_share * 4.0;
    let participation_score = avg_participation * 4.0;

    let total = kda_score + win_rate_score + dmg_score + dmg_taken_score + cs_score + gold_score + participation_score;

    Some(AkariScore {
        kda_score,
        win_rate_score,
        dmg_score,
        dmg_taken_score,
        cs_score,
        gold_score,
        participation_score,
        total,
    })
}

/// 兼容接口：计算评分。如果输入是详细数据，则使用 AkariScore。
pub fn calculate_player_rating(puuid: &str, matches: &[Value]) -> Option<PlayerPerformance> {
    if matches.is_empty() { return None; }

    // 检查是否包含详细数据 (是否有 participantIdentities 且 participantId 能对应到 puuid)
    let is_detailed = matches[0].get("participantIdentities").is_some();

    if is_detailed {
        if let Some(akari) = calculate_akari_score(puuid, matches) {
            let mut total_kda = 0.0;
            let mut wins = 0.0;
            for m in matches {
                let idents = m.get("participantIdentities")?.as_array()?;
                let pid = idents.iter().find(|id| id.get("player").and_then(|p| p.get("puuid")).and_then(|p| p.as_str()) == Some(puuid))?
                    .get("participantId")?.as_i64()?;
                let p = m.get("participants")?.as_array()?.iter().find(|p| p.get("participantId").and_then(|v| v.as_i64()) == Some(pid))?;
                let s = p.get("stats")?;
                total_kda += (s.get("kills").and_then(|v| v.as_f64()).unwrap_or(0.0) + s.get("assists").and_then(|v| v.as_f64()).unwrap_or(0.0)) / s.get("deaths").and_then(|v| v.as_f64()).unwrap_or(1.0).max(1.0);
                if s.get("win").and_then(|v| v.as_bool()).unwrap_or(false) { wins += 1.0; }
            }

            return Some(PlayerPerformance {
                puuid: puuid.to_owned(),
                name: String::new(),
                score: akari.total,
                avg_kda: total_kda / matches.len() as f64,
                win_rate: wins / matches.len() as f64,
                count: matches.len(),
            });
        }
    }

    // 基础评分兜底 (旧逻辑)
    let mut total_kda = 0.0;
    let mut wins = 0.0;
    let mut valid_count = 0;

    for m in matches {
        let participants = m.get("participants").and_then(|v| v.as_array())?;
        let me = participants.first()?; // 摘要战绩中，自己通常是第一个
        let stats = me.get("stats")?;

        let win = stats.get("win").and_then(|v| v.as_bool()).unwrap_or(false);
        let kills = stats.get("kills").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let deaths = stats.get("deaths").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let assists = stats.get("assists").and_then(|v| v.as_f64()).unwrap_or(0.0);

        total_kda += (kills + assists) / deaths.max(1.0);
        if win { wins += 1.0; }
        valid_count += 1;
    }

    if valid_count == 0 { return None; }

    let avg_kda = total_kda / valid_count as f64;
    let win_rate = wins / valid_count as f64;
    
    // 降级分计算：仅 KDA + 胜率。KDA 分值大约占总分的 40%
    let kda_score = avg_kda.sqrt() * 1.44;
    let win_rate_score = (win_rate - 0.5) * 4.0;
    // 为缺失字段补齐一个合理的平均值分 (假设中等马)
    let final_score = kda_score + win_rate_score + 15.0;

    Some(PlayerPerformance {
        puuid: puuid.to_owned(),
        name: String::new(),
        score: final_score,
        avg_kda,
        win_rate,
        count: valid_count,
    })
}
