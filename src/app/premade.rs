//! 预组队（几黑）分析
//!
//! 算法移植自 LeagueAkari 的 `team-up-calc.ts`：
//! 1. 拉取双方所有玩家的历史战绩；
//! 2. 对每场战绩，遍历同队的两两玩家对，累计同队次数（加权图）；
//! 3. 枚举当前队伍玩家的所有非空子集，保留"同队次数 ≥ threshold"的组合；
//! 4. 利用并查集合并重叠子集，输出最终预组队分组。
//!
//! 入口函数：[`analyze_premade`]

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use tokio::task::JoinSet;
use tracing::{debug, warn};

use crate::lcu::api::{LcuApiError, LcuClient};

// ── 公共类型 ──────────────────────────────────────────────────────

/// 单支队伍的预组队分析结果。
#[derive(Debug, Clone)]
pub struct TeamPremade {
    /// 队伍名称（如 "我方" / "对方"）
    pub team_name: String,
    /// 推断出的预组队分组，每组包含玩家召唤师名称列表，以及共同出现次数
    pub groups: Vec<PremadeGroup>,
}

/// 一组预组队玩家。
#[derive(Debug, Clone)]
pub struct PremadeGroup {
    /// 召唤师名称列表（已去重）
    pub summoner_names: Vec<String>,
    /// 本组成员共同出现的局数（推断值，取组内最小公共次数）
    pub times: usize,
}

// ── 分析入口 ──────────────────────────────────────────────────────

/// 获取英雄选择 session 中双方 PUUID 列表，以及各自的队伍颜色（100=蓝队，200=红队）。
///
/// 返回 `(my_team, their_team, my_side, their_side)`，
/// team 均为 `(puuid, summoner_name, champion_id)` 三元组；
/// `champion_id` 优先取 `championId`（已锁定），为 0 时回退到 `championPickIntent`（悬停意向）。
pub fn extract_teams_from_session(
    session: &Value,
) -> (Vec<(String, String, i64)>, Vec<(String, String, i64)>, Option<u32>, Option<u32>) {
    let extract = |key: &str| -> Vec<(String, String, i64)> {
        session
            .get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        let puuid = p.get("puuid")?.as_str()?;
                        if puuid.is_empty() || puuid == "00000000-0000-0000-0000-000000000000" {
                            return None;
                        }
                        // summonerName 可能在不同版本字段名不同
                        let name = p
                            .get("summonerName")
                            .or_else(|| p.get("displayName"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(puuid)
                            .to_owned();
                        // 优先锁定英雄，退回到悬停意向
                        let champ_id = p
                            .get("championId")
                            .and_then(|v| v.as_i64())
                            .filter(|&id| id != 0)
                            .or_else(|| {
                                p.get("championPickIntent")
                                    .and_then(|v| v.as_i64())
                                    .filter(|&id| id != 0)
                            })
                            .unwrap_or(0);
                        Some((puuid.to_owned(), name, champ_id))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    // 从队伍数组中读取第一个合法玩家的 `team` 字段（100 或 200）
    let read_side = |key: &str| -> Option<u32> {
        session
            .get(key)
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter().find_map(|p| {
                    let puuid = p.get("puuid")?.as_str()?;
                    if puuid.is_empty() || puuid == "00000000-0000-0000-0000-000000000000" {
                        return None;
                    }
                    p.get("team")?.as_u64().map(|v| v as u32)
                })
            })
    };

    let my_side = read_side("myTeam");
    let their_side = read_side("theirTeam");
    (extract("myTeam"), extract("theirTeam"), my_side, their_side)
}

/// 分析双方队伍的预组队情况，返回两个 `TeamPremade`。
///
/// - `my_team` / `their_team`：`(puuid, display_name)` 列表（display_name 已含英雄信息）
/// - `threshold`：认定为预组队的最低同队场次（建议 3）
/// - `history_count`：每人拉取的战绩场数（建议 20）
pub async fn analyze_premade(
    api: &LcuClient,
    my_team: Vec<(String, String)>,
    their_team: Vec<(String, String)>,
    threshold: usize,
    history_count: usize,
) -> (TeamPremade, TeamPremade) {
    // 收集所有玩家，并发拉取战绩
    let all_players: Vec<(String, String)> = my_team
        .iter()
        .chain(their_team.iter())
        .cloned()
        .collect();

    let histories = fetch_all_histories(api, &all_players, history_count).await;

    let my_result = calc_team_premade(
        "我方",
        &my_team,
        &histories,
        threshold,
    );
    let their_result = calc_team_premade(
        "对方",
        &their_team,
        &histories,
        threshold,
    );

    (my_result, their_result)
}

// ── 战绩拉取 ─────────────────────────────────────────────────────

/// 并发拉取所有玩家的最近 N 场战绩。
///
/// 返回 `HashMap<puuid, Vec<(game_id, Vec<puuid_in_same_team>)>>`。
async fn fetch_all_histories(
    api: &LcuClient,
    players: &[(String, String)],
    count: usize,
) -> HashMap<String, Vec<(i64, Vec<String>)>> {
    let mut set = JoinSet::new();

    for (puuid, _) in players {
        let api2 = api.clone();
        let puuid2 = puuid.clone();
        set.spawn(async move {
            let result = fetch_player_history(&api2, &puuid2, count).await;
            (puuid2, result)
        });
    }

    let mut result: HashMap<String, Vec<(i64, Vec<String>)>> = HashMap::new();
    while let Some(Ok((puuid, history))) = set.join_next().await {
        match history {
            Ok(h) => {
                debug!("战绩拉取完成 puuid={} games={}", &puuid[..8.min(puuid.len())], h.len());
                result.insert(puuid, h);
            }
            Err(e) => {
                warn!("战绩拉取失败 puuid={}: {e}", &puuid[..8.min(puuid.len())]);
                result.insert(puuid, vec![]);
            }
        }
    }
    result
}

/// 拉取单人战绩，返回 `Vec<(game_id, 同队玩家 PUUID 列表)>`。
///
/// 通过 participantIdentities 与 participants 关联得到 teamId → puuid 映射。
async fn fetch_player_history(
    api: &LcuClient,
    puuid: &str,
    count: usize,
) -> Result<Vec<(i64, Vec<String>)>, LcuApiError> {
    let raw = api.get_match_history(puuid, count).await?;

    let games = raw
        .get("games")
        .and_then(|v| v.get("games"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut result = Vec::new();

    for game in &games {
        let game_id = match game.get("gameId").and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => continue,
        };

        // participantId -> puuid 映射
        let id_to_puuid: HashMap<i64, String> = game
            .get("participantIdentities")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|pi| {
                        let pid = pi.get("participantId")?.as_i64()?;
                        let puuid = pi
                            .get("player")
                            .and_then(|p| p.get("puuid"))
                            .and_then(|v| v.as_str())?
                            .to_owned();
                        Some((pid, puuid))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 找到当前玩家的 teamId
        let my_participant_id = id_to_puuid
            .iter()
            .find(|(_, p)| p.as_str() == puuid)
            .map(|(id, _)| *id);

        let my_team_id = my_participant_id.and_then(|pid| {
            game.get("participants")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    arr.iter().find(|p| {
                        p.get("participantId").and_then(|v| v.as_i64()) == Some(pid)
                    })
                })
                .and_then(|p| p.get("teamId")?.as_i64())
        });

        let Some(my_team_id) = my_team_id else { continue };

        // 同队玩家（排除自己）
        let teammates: Vec<String> = game
            .get("participants")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        let tid = p.get("teamId")?.as_i64()?;
                        if tid != my_team_id {
                            return None;
                        }
                        let pid = p.get("participantId")?.as_i64()?;
                        let teammate_puuid = id_to_puuid.get(&pid)?.clone();
                        if teammate_puuid == puuid {
                            return None;
                        }
                        Some(teammate_puuid)
                    })
                    .collect()
            })
            .unwrap_or_default();

        result.push((game_id, teammates));
    }

    Ok(result)
}

// ── 图算法：计算同队次数 ──────────────────────────────────────────

/// 计算一支队伍内所有玩家对的"同队次数"，返回预组队分组。
fn calc_team_premade(
    team_name: &str,
    team: &[(String, String)],
    histories: &HashMap<String, Vec<(i64, Vec<String>)>>,
    threshold: usize,
) -> TeamPremade {
    if team.len() < 2 {
        return TeamPremade { team_name: team_name.to_owned(), groups: vec![] };
    }

    let puuids: Vec<&str> = team.iter().map(|(p, _)| p.as_str()).collect();
    let puuid_set: HashSet<&str> = puuids.iter().copied().collect();

    // 构建加权图：(puuid_a, puuid_b) -> 同队次数
    let mut edge_count: HashMap<(&str, &str), usize> = HashMap::new();

    for (puuid, _name) in team {
        let Some(history) = histories.get(puuid.as_str()) else { continue };
        for (_game_id, teammates) in history {
            // 找出 teammates 中属于当前队伍的玩家
            let in_team: Vec<&str> = teammates
                .iter()
                .filter_map(|t| {
                    let s = t.as_str();
                    if puuid_set.contains(s) && s != puuid.as_str() {
                        Some(s)
                    } else {
                        None
                    }
                })
                .collect();

            // 当前玩家与这些队友都同队过
            for teammate in &in_team {
                let key = if puuid.as_str() < *teammate {
                    (puuid.as_str(), *teammate)
                } else {
                    (*teammate, puuid.as_str())
                };
                *edge_count.entry(key).or_insert(0) += 1;
            }
        }
    }

    // 找出满足阈值的边
    let valid_edges: Vec<(&str, &str, usize)> = edge_count
        .iter()
        .filter(|(_, &count)| count >= threshold)
        .map(|((a, b), &count)| (*a, *b, count))
        .collect();

    if valid_edges.is_empty() {
        return TeamPremade { team_name: team_name.to_owned(), groups: vec![] };
    }

    // 并查集：把满足阈值的玩家对合并到同一组
    let mut parent: HashMap<&str, &str> = puuids.iter().map(|&p| (p, p)).collect();

    fn find<'a>(parent: &mut HashMap<&'a str, &'a str>, x: &'a str) -> &'a str {
        if parent[x] == x {
            return x;
        }
        let root = find(parent, parent[x]);
        parent.insert(x, root);
        root
    }

    fn union<'a>(parent: &mut HashMap<&'a str, &'a str>, x: &'a str, y: &'a str) {
        let rx = find(parent, x);
        let ry = find(parent, y);
        if rx != ry {
            parent.insert(rx, ry);
        }
    }

    for (a, b, _) in &valid_edges {
        union(&mut parent, a, b);
    }

    // 按根节点分组
    let mut groups_map: HashMap<String, Vec<&str>> = HashMap::new();
    let all_puuids: Vec<&str> = parent.keys().copied().collect();
    for puuid in all_puuids {
        let root = find(&mut parent, puuid);
        groups_map.entry(root.to_owned()).or_default().push(puuid);
    }

    // 构建输出：只保留组内有 ≥2 人且组内任意两人满足阈值的组
    let puuid_to_name: HashMap<&str, &str> = team
        .iter()
        .map(|(p, n)| (p.as_str(), n.as_str()))
        .collect();

    let mut groups: Vec<PremadeGroup> = groups_map
        .values()
        .filter(|g| g.len() >= 2)
        .map(|g| {
            // 计算组内最小同队次数（保守估计）
            let min_times = pairs(g)
                .iter()
                .filter_map(|(a, b)| {
                    let key = if a < b { (*a, *b) } else { (*b, *a) };
                    edge_count.get(&key).copied()
                })
                .min()
                .unwrap_or(0);

            let mut names: Vec<String> = g
                .iter()
                .map(|&p| puuid_to_name.get(p).unwrap_or(&p).to_string())
                .collect();
            names.sort();

            PremadeGroup {
                summoner_names: names,
                times: min_times,
            }
        })
        .filter(|g| g.times >= threshold)
        .collect();

    // 按组大小降序排列
    groups.sort_by(|a, b| b.summoner_names.len().cmp(&a.summoner_names.len()));

    TeamPremade { team_name: team_name.to_owned(), groups }
}

/// 生成列表的所有两两对。
fn pairs<'a>(items: &[&'a str]) -> Vec<(&'a str, &'a str)> {
    let mut result = Vec::new();
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            result.push((items[i], items[j]));
        }
    }
    result
}

/// 从 gameflow session 中提取双方队伍信息（游戏进行中使用），显示名含英雄名。
///
/// - `my_puuid`：本地玩家 PUUID，用于区分我方 / 对方
/// - `id_name_map`：英雄 ID → 名称映射
///
/// teamOne = 蓝队（100），teamTwo = 红队（200）。
pub fn extract_teams_from_gameflow_session(
    session: &Value,
    my_puuid: &str,
    id_name_map: &std::collections::HashMap<i64, String>,
) -> (Vec<(String, String)>, Vec<(String, String)>, Option<u32>, Option<u32>) {
    let game_data = match session.get("gameData") {
        Some(v) => v,
        None => return (vec![], vec![], None, None),
    };

    let extract_team = |key: &str| -> Vec<(String, String)> {
        game_data
            .get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        let puuid = p.get("puuid")?.as_str()?;
                        if puuid.is_empty() || puuid == "00000000-0000-0000-0000-000000000000" {
                            return None;
                        }
                        let name = p
                            .get("summonerName")
                            .or_else(|| p.get("gameName"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(puuid)
                            .to_owned();
                        let champ_id = p
                            .get("championId")
                            .and_then(|v| v.as_i64())
                            .filter(|&id| id != 0)
                            .unwrap_or(0);
                        let label = if champ_id != 0 {
                            if let Some(cname) = id_name_map.get(&champ_id) {
                                format!("{name}({cname})")
                            } else {
                                name
                            }
                        } else {
                            name
                        };
                        Some((puuid.to_owned(), label))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let team_one = extract_team("teamOne"); // 蓝队 100
    let team_two = extract_team("teamTwo"); // 红队 200

    // 根据本地玩家所在队伍确定我方 / 对方
    let my_in_one = team_one.iter().any(|(p, _)| p == my_puuid);
    if my_in_one {
        (team_one, team_two, Some(100), Some(200))
    } else {
        (team_two, team_one, Some(200), Some(100))
    }
}

// ── 格式化输出 ────────────────────────────────────────────────────

/// 将两队分析结果格式化为发送到聊天的字符串。
///
/// `my_side` / `their_side`：队伍颜色，`100` = 蓝队，`200` = 红队，`None` = 未知。
pub fn format_premade_message(
    my_team: &TeamPremade,
    their_team: &TeamPremade,
    my_side: Option<u32>,
    their_side: Option<u32>,
) -> String {
    fn side_label(side: Option<u32>) -> &'static str {
        match side {
            Some(100) => "🔵蓝队",
            Some(200) => "🔴红队",
            _ => "",
        }
    }

    let format_team = |team: &TeamPremade, side: Option<u32>| -> String {
        let side_str = side_label(side);
        let header = if side_str.is_empty() {
            team.team_name.clone()
        } else {
            format!("{} {}", team.team_name, side_str)
        };
        if team.groups.is_empty() {
            return format!("{header}：未检测到预组队");
        }
        let group_strs: Vec<String> = team
            .groups
            .iter()
            .map(|g| {
                format!(
                    "  {}黑（{}局）：{}",
                    g.summoner_names.len(),
                    g.times,
                    g.summoner_names.join(" / ")
                )
            })
            .collect();
        format!("{header}：\n{}", group_strs.join("\n"))
    };

    format!(
        "[对局组黑分析]\n{}\n{}",
        format_team(my_team, my_side),
        format_team(their_team, their_side)
    )
}
