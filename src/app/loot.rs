//! 自动领取任务奖励与免费宝箱/胶囊
//!
//! 功能：
//! - [`auto_claim_missions`]：领取所有状态为 `COMPLETED` 的任务
//! - [`auto_open_free_loot`]：自动开启不需要额外材料（钥匙等）的宝箱/胶囊
//! - [`run_auto_loot`]：统一入口，游戏结束时调用

use serde_json::Value;
use tracing::{debug, info, warn};

use crate::lcu::api::LcuClient;

// ── 任务自动领取 ──────────────────────────────────────────────────

/// 领取所有已完成（`status == "COMPLETED"`）的任务奖励。
///
/// 对已领取或无法领取的任务，仅以 debug 级别记录错误、不中断流程。
/// 返回成功领取数量。
pub async fn auto_claim_missions(api: &LcuClient) -> usize {
    let missions = match api.get_missions().await {
        Ok(v) => v,
        Err(e) => {
            warn!("获取任务列表失败: {e}");
            return 0;
        }
    };

    let mut claimed = 0usize;
    for m in &missions {
        let status = m.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if status != "COMPLETED" {
            continue;
        }
        let id = match m.get("id").and_then(|v| v.as_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let title = m
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or(id.as_str())
            .to_owned();

        match api.claim_mission(&id).await {
            Ok(()) => {
                info!("已领取任务奖励：{title}");
                claimed += 1;
            }
            Err(e) => {
                debug!("领取任务 {id} 失败（可能已领取）: {e}");
            }
        }
    }
    claimed
}

// ── 免费宝箱自动开启 ──────────────────────────────────────────────

/// 自动开启"免费"宝箱/胶囊（配方中所有 slot 仅需要该物品本身，无需钥匙等额外材料）。
///
/// 判定逻辑：
/// 1. 筛选 `displayCategories` 含 `"CHEST"` 且 `count > 0` 的物品
/// 2. 查询配方列表，找到 `type == "open"` 且所有 slot 仅包含该物品本身的配方
/// 3. 批量合成（`repeat = count / items_per_craft`）
///
/// 返回成功开启的物品总数量。
pub async fn auto_open_free_loot(api: &LcuClient) -> usize {
    let loot = match api.get_player_loot().await {
        Ok(v) => v,
        Err(e) => {
            warn!("获取战利品列表失败: {e}");
            return 0;
        }
    };

    // 筛选出所有显示类别含 CHEST 且数量 > 0 的物品（按 lootId 去重取最大 count）
    let mut chest_map: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for item in &loot {
        let loot_id = match item.get("lootId").and_then(|v| v.as_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let count = item.get("count").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        if count == 0 {
            continue;
        }
        let cats = item
            .get("displayCategories")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if cats.to_uppercase().contains("CHEST") {
            let e = chest_map.entry(loot_id).or_insert(0);
            *e = (*e).max(count);
        }
    }

    let mut opened = 0usize;
    for (loot_id, count) in chest_map {
        let recipes = match api.get_loot_recipes(&loot_id).await {
            Ok(v) => v,
            Err(e) => {
                debug!("获取 {loot_id} 配方失败: {e}");
                continue;
            }
        };

        if let Some((ingredients, items_per_craft)) = find_free_open_recipe(&loot_id, &recipes) {
            let repeat = count / items_per_craft.max(1);
            if repeat == 0 {
                continue;
            }
            match api.craft_loot(&loot_id, ingredients, repeat).await {
                Ok(()) => {
                    let total = repeat * items_per_craft;
                    info!("自动开启 {loot_id} × {repeat}（共 {total} 个）");
                    opened += total as usize;
                }
                Err(e) => {
                    warn!("开启 {loot_id} 失败: {e}");
                }
            }
        }
    }
    opened
}

/// 查找"免费开启"配方：
/// - `type == "open"`
/// - 所有 slot 的 `lootIds` 只包含该物品本身（不引用钥匙等外部材料）
///
/// 返回 `(ingredients, items_per_craft)`，`ingredients` 为传给 craft 接口的材料数组。
fn find_free_open_recipe(loot_id: &str, recipes: &[Value]) -> Option<(Vec<String>, u32)> {
    for recipe in recipes {
        let rtype = recipe.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if rtype != "open" {
            continue;
        }
        let slots = match recipe.get("slots").and_then(|v| v.as_array()) {
            Some(s) => s,
            None => continue,
        };

        // 所有 slot 的 lootIds 必须只包含该物品本身
        let all_self = slots.iter().all(|slot| {
            let ids = slot
                .get("lootIds")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>())
                .unwrap_or_default();
            !ids.is_empty() && ids.iter().all(|id| *id == loot_id)
        });
        if !all_self {
            continue;
        }

        // 构建材料列表（slot.quantity 控制重复次数）
        let mut ingredients: Vec<String> = Vec::new();
        let mut total: u32 = 0;
        for slot in slots {
            let qty = slot
                .get("quantity")
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as u32;
            for _ in 0..qty {
                ingredients.push(loot_id.to_owned());
            }
            total += qty;
        }
        if total == 0 {
            total = 1;
            ingredients = vec![loot_id.to_owned()];
        }
        return Some((ingredients, total));
    }
    None
}

// ── 统一入口 ──────────────────────────────────────────────────────

/// 自动领取已完成任务 + 开启免费宝箱/胶囊（游戏结束后调用）。
pub async fn run_auto_loot(api: &LcuClient) {
    info!("开始自动领取任务与宝箱...");

    let missions = auto_claim_missions(api).await;
    if missions > 0 {
        info!("已自动领取 {missions} 个已完成任务");
    } else {
        debug!("没有待领取的已完成任务");
    }

    let opened = auto_open_free_loot(api).await;
    if opened > 0 {
        info!("已自动开启 {opened} 个免费宝箱/胶囊");
    } else {
        debug!("没有可免费开启的宝箱/胶囊");
    }
}
