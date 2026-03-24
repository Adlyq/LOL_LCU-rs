//! 预组队（几黑）分析
//!
//! 算法逻辑完全参考 LeagueAkari (ongoing-game 模块)：
//! 1. 从当前对局（Gameflow Session 或 ChampSelect Session）中获取所有玩家的 PUUID。
//! 2. 批量拉取所有玩家最近的战绩（默认 20 场），提取每一场的 GameID 和 胜负结果(win)。
//! 3. 推断预组队：若两个玩家在历史战绩中多次（阈值默认为 3）出现在同一个 GameID 且胜负结果一致，则认为其是预组队。
//! 4. 使用并查集 (UnionFind) 将相互关联的玩家合并为最终的分组。

use std::collections::HashMap;

use serde_json::Value;
use tokio::task::JoinSet;
use tracing::{debug, warn};

use crate::lcu::api::{LcuApiError, LcuClient};

// ── 公共类型 ──────────────────────────────────────────────────────

/// 单支队伍的预组队分析结果。
#[derive(Debug, Clone)]
pub struct TeamPremade {
    pub team_name: String,
    pub groups: Vec<PremadeGroup>,
}

/// 一组预组队玩家。
#[derive(Debug, Clone)]
pub struct PremadeGroup {
    pub summoner_names: Vec<String>,
    pub times: usize,
}

type ChampSelectTeamData = (Vec<(String, String, i64)>, Vec<(String, String, i64)>, Option<u32>, Option<u32>);
type GameflowTeamData = (Vec<(String, String)>, Vec<(String, String)>, Option<u32>, Option<u32>);

// ── 分析入口 ──────────────────────────────────────────────────────

/// 分析双方队伍的预组队情况。
///
/// - `my_team` / `their_team`: `(puuid, display_name)` 列表。
/// - `threshold`: 认定为预组队的最低同场次数（LeagueAkari 默认为 3）。
/// - `history_count`: 每人拉取的战绩场数（LeagueAkari 默认为 20）。
pub async fn analyze_premade(
    api: &LcuClient,
    my_team: Vec<(String, String)>,
    their_team: Vec<(String, String)>,
    threshold: usize,
    history_count: usize,
) -> (TeamPremade, TeamPremade) {
    let all_players: Vec<(String, String)> = my_team.iter().chain(their_team.iter()).cloned().collect();

    // 1. 批量拉取所有玩家的历史战绩（GameID -> Win）
    let histories = fetch_all_player_game_histories(api, &all_players, history_count).await;

    // 2. 分别计算我方和对方的预组队
    let my_result = calc_inferred_premade("我方", &my_team, &histories, threshold);
    let their_result = calc_inferred_premade("对方", &their_team, &histories, threshold);

    (my_result, their_result)
}

// ── 战绩数据获取 ──────────────────────────────────────────────────

/// 并发拉取玩家战绩，返回 PUUID -> Map<GameID, Win>。
async fn fetch_all_player_game_histories(
    api: &LcuClient,
    players: &[(String, String)],
    count: usize,
) -> HashMap<String, HashMap<i64, bool>> {
    let mut set = JoinSet::new();

    for (puuid, _) in players {
        let api_c = api.clone();
        let puuid_c = puuid.clone();
        set.spawn(async move {
            let res = fetch_player_game_id_win_map(&api_c, &puuid_c, count).await;
            (puuid_c, res)
        });
    }

    let mut result = HashMap::new();
    while let Some(Ok((puuid, map_res))) = set.join_next().await {
        match map_res {
            Ok(map) => {
                debug!("PUUID={} 战绩拉取完成，共 {} 场", &puuid[..8.min(puuid.len())], map.len());
                result.insert(puuid, map);
            }
            Err(e) => {
                warn!("PUUID={} 战绩拉取失败: {e}", &puuid[..8.min(puuid.len())]);
                result.insert(puuid, HashMap::new());
            }
        }
    }
    result
}

/// 拉取单人战绩，提取 (GameID, Win)。
async fn fetch_player_game_id_win_map(
    api: &LcuClient,
    puuid: &str,
    count: usize,
) -> Result<HashMap<i64, bool>, LcuApiError> {
    let raw = api.get_match_history(puuid, count).await?;
    
    // 兼容多种 LCU 响应格式
    let games = raw.get("games")
        .and_then(|v| if v.is_array() { Some(v) } else { v.get("games") })
        .and_then(|v| v.as_array());

    let Some(games_arr) = games else {
        return Ok(HashMap::new());
    };

    let mut map = HashMap::new();
    for g in games_arr {
        if let Some(game_id) = g.get("gameId").and_then(|v| v.as_i64()) {
            // 战绩摘要中，participants[0] 通常就是该 PUUID 本人的数据
            let win = g.get("participants")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|p| p.get("stats"))
                .and_then(|s| s.get("win"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            map.insert(game_id, win);
        }
    }
    Ok(map)
}

// ── 推断算法 ──────────────────────────────────────────────────────

/// 基于战绩交叉推断预组队。
fn calc_inferred_premade(
    team_name: &str,
    team: &[(String, String)],
    histories: &HashMap<String, HashMap<i64, bool>>,
    threshold: usize,
) -> TeamPremade {
    if team.len() < 2 {
        return TeamPremade { team_name: team_name.to_owned(), groups: vec![] };
    }

    // 1. 计算两两玩家之间的共同对局次数（且胜负一致）
    let mut edges = Vec::new();
    for i in 0..team.len() {
        for j in (i + 1)..team.len() {
            let (puuid_a, _) = &team[i];
            let (puuid_b, _) = &team[j];

            let count = count_common_games(
                histories.get(puuid_a),
                histories.get(puuid_b)
            );

            if count >= threshold {
                edges.push((i, j, count));
            }
        }
    }

    if edges.is_empty() {
        return TeamPremade { team_name: team_name.to_owned(), groups: vec![] };
    }

    // 2. 使用并查集进行分组
    let mut parent: Vec<usize> = (0..team.len()).collect();
    fn find(p: &mut Vec<usize>, i: usize) -> usize {
        if p[i] == i { i } else {
            let root = find(p, p[i]);
            p[i] = root;
            root
        }
    }
    fn union(p: &mut Vec<usize>, i: usize, j: usize) {
        let root_i = find(p, i);
        let root_j = find(p, j);
        if root_i != root_j { p[root_i] = root_j; }
    }

    for (i, j, _) in &edges {
        union(&mut parent, *i, *j);
    }

    // 3. 聚合结果
    let mut groups_map: HashMap<usize, (Vec<String>, usize)> = HashMap::new();
    for (i, (_, name)) in team.iter().enumerate() {
        let root = find(&mut parent, i);
        let entry = groups_map.entry(root).or_insert((vec![], 0));
        entry.0.push(name.clone());
    }

    // 计算组内最小公共次数作为该组的 "times"
    let mut final_groups = Vec::new();
    for (root, (mut names, _)) in groups_map {
        if names.len() < 2 { continue; }
        
        // 查找属于该组的所有边，取最小值（由于并查集保证了连通性，这里简单处理）
        let mut min_times = 999;
        let mut found_edge = false;
        for (i, _j, c) in &edges {
            if find(&mut parent, *i) == root {
                min_times = min_times.min(*c);
                found_edge = true;
            }
        }

        names.sort();
        final_groups.push(PremadeGroup {
            summoner_names: names,
            times: if found_edge { min_times } else { threshold },
        });
    }

    final_groups.sort_by(|a, b| b.summoner_names.len().cmp(&a.summoner_names.len()));

    TeamPremade {
        team_name: team_name.to_owned(),
        groups: final_groups,
    }
}

/// 计算两个玩家共同出现在同一场对局且胜负一致的次数。
fn count_common_games(
    a: Option<&HashMap<i64, bool>>,
    b: Option<&HashMap<i64, bool>>,
) -> usize {
    let (Some(map_a), Some(map_b)) = (a, b) else { return 0; };
    let mut count = 0;
    for (game_id, win_a) in map_a {
        if let Some(win_b) = map_b.get(game_id) {
            if win_a == win_b {
                count += 1;
            }
        }
    }
    count
}

// ── Session 提取工具 ──────────────────────────────────────────────

pub fn extract_teams_from_session(
    session: &Value,
) -> ChampSelectTeamData {
    let extract = |key: &str| -> Vec<(String, String, i64)> {
        let players_val = session.get(key).and_then(|v| v.as_array());
        let mut result = Vec::new();
        
        if let Some(arr) = players_val {
            for p in arr {
                let puuid_raw = p.get("puuid").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
                    .map(|s| s.to_owned())
                    .or_else(|| p.get("summonerId").and_then(|v| v.as_i64()).map(|id| id.to_string()));
                
                let Some(puuid) = puuid_raw else {
                    tracing::warn!("选人阶段：跳过队伍 {} 中的一个玩家，因为找不到标识符", key);
                    continue;
                };

                let game_name = p.get("gameName").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
                let tag_line = p.get("tagLine").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
                let display_name = p.get("displayName").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
                let summoner_name = p.get("summonerName").and_then(|v| v.as_str()).filter(|s| !s.is_empty());

                // 优先级：GameName#Tag > DisplayName > SummonerName > "召唤师" (兜底)
                let name = if let Some(gn) = game_name {
                    if let Some(tl) = tag_line { format!("{}#{}", gn, tl) } else { gn.to_owned() }
                } else if let Some(dn) = display_name {
                    dn.to_owned()
                } else if let Some(sn) = summoner_name {
                    sn.to_owned()
                } else {
                    "召唤师".to_owned()
                };

                let champ_id = p.get("championId").and_then(|v| v.as_i64()).filter(|&id| id != 0)
                    .or_else(|| p.get("championPickIntent").and_then(|v| v.as_i64())).unwrap_or(0);
                
                result.push((puuid, name, champ_id));
            }
        }
        debug!("提取队伍 {}: {} 人", key, result.len());
        result
    };
    let my_team = extract("myTeam");
    let their_team = extract("theirTeam");
    let my_side = session.get("myTeam").and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|p| p.get("team")).and_then(|v| v.as_u64()).map(|v| v as u32);
    let their_side = session.get("theirTeam").and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|p| p.get("team")).and_then(|v| v.as_u64()).map(|v| v as u32);
    (my_team, their_team, my_side, their_side)
}

/// 从游戏进行中 (Gameflow) Session 提取玩家。
pub fn extract_teams_from_gameflow_session(
    session: &Value,
    my_puuid: &str,
    id_name_map: &HashMap<i64, String>,
) -> GameflowTeamData {
    let game_data = session.get("gameData").unwrap_or(session);
    
    let extract_team = |key: &str| -> Vec<(String, String)> {
        let players_val = game_data.get(key).and_then(|v| v.as_array());
        let mut result = Vec::new();

        if let Some(arr) = players_val {
            for p in arr {
                let puuid_raw = p.get("puuid").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
                    .map(|s| s.to_owned())
                    .or_else(|| p.get("summonerId").and_then(|v| v.as_i64()).map(|id| id.to_string()));

                let Some(puuid) = puuid_raw else {
                    tracing::warn!("对局分析：跳过队伍 {} 中的一个玩家，因为找不到标识符", key);
                    continue;
                };

                let game_name = p.get("gameName").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
                let tag_line = p.get("tagLine").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
                let display_name = p.get("displayName").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
                let summoner_name = p.get("summonerName").and_then(|v| v.as_str()).filter(|s| !s.is_empty());

                let name = if let Some(gn) = game_name {
                    if let Some(tl) = tag_line { format!("{}#{}", gn, tl) } else { gn.to_owned() }
                } else if let Some(dn) = display_name {
                    dn.to_owned()
                } else if let Some(sn) = summoner_name {
                    sn.to_owned()
                } else {
                    "召唤师".to_owned()
                };

                let champ_id = p.get("championId").and_then(|v| v.as_i64()).unwrap_or(0);
                let label = if champ_id != 0 {
                    if let Some(cname) = id_name_map.get(&champ_id) { 
                        // 如果名字是“召唤师”，则只显示 "英雄名"，否则显示 "人名(英雄名)"
                        if name == "召唤师" {
                            cname.clone()
                        } else {
                            format!("{}({})", name, cname) 
                        }
                    } else { 
                        name 
                    }
                } else { 
                    name 
                };
                result.push((puuid, label));
            }
        }
        debug!("提取对局队伍 {}: {} 人", key, result.len());
        result
    };

    let t1 = extract_team("teamOne");
    let t2 = extract_team("teamTwo");
    let my_in_t1 = t1.iter().any(|(p, _)| p == my_puuid);
    if my_in_t1 { (t1, t2, Some(100), Some(200)) } else { (t2, t1, Some(200), Some(100)) }
}

// ── 格式化输出 ────────────────────────────────────────────────────

pub fn format_premade_message(
    my_team: &TeamPremade,
    their_team: &TeamPremade,
    my_side: Option<u32>,
    their_side: Option<u32>,
) -> String {
    let side_label = |s| match s { Some(100) => "[蓝方]", Some(200) => "[红方]", _ => "" };
    
    let fmt_t = |t: &TeamPremade, s| {
        if t.groups.is_empty() { return None; }
        // 这里的头不再包含蓝红队字样，因为已经提到总标题了
        let head = format!("{}", t.team_name);
        let g_strs: Vec<String> = t.groups.iter().map(|g| format!("  {}黑（{}局）：{}", g.summoner_names.len(), g.times, g.summoner_names.join(" / "))).collect();
        Some(format!("{}：\n{}", head, g_strs.join("\n")))
    };

    // 标题始终带上我方的阵营标识
    let title = format!("[对局组黑分析] {}", side_label(my_side));
    let mut parts = vec![title];
    
    if let Some(m) = fmt_t(my_team, my_side) { parts.push(m); }
    if let Some(t) = fmt_t(their_team, their_side) { parts.push(t); }

    if parts.len() == 1 {
        // 如果双方都没组黑
        return format!("{}\n纯路人局", parts[0]);
    }

    parts.join("\n")
}
