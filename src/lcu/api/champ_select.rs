use serde_json::{json, Value};
use std::collections::HashMap;
use crate::lcu::api::{LcuClient, LcuApiError};

impl LcuClient {
    pub async fn get_champ_select_session(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-champ-select/v1/session").await
    }

    pub async fn get_pickable_champion_ids(&self) -> Result<Vec<i64>, LcuApiError> {
        let v = self
            .get_json("/lol-champ-select/v1/pickable-champion-ids")
            .await?;
        Ok(v.as_array()
            .map(|arr| arr.iter().filter_map(|x| x.as_i64()).collect())
            .unwrap_or_default())
    }

    pub async fn get_owned_champions_minimal(&self) -> Result<Vec<Value>, LcuApiError> {
        let summoner = self.get_current_summoner().await?;
        let summoner_id = summoner
            .get("summonerId")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| LcuApiError::Other("未找到 summonerId".into()))?;
        let v = self
            .get_json(&format!(
                "/lol-champions/v1/inventories/{summoner_id}/champions-minimal"
            ))
            .await?;
        Ok(v.as_array()
            .map(|arr| arr.iter().filter(|x| x.is_object()).cloned().collect())
            .unwrap_or_default())
    }

    /// 行动：hover 或 lock 指定英雄。
    pub async fn act_champion(
        &self,
        champion_id: i64,
        completed: bool,
        action_id: Option<i64>,
    ) -> Result<Value, LcuApiError> {
        let aid = match action_id {
            Some(id) => id,
            None => {
                let session = self.get_champ_select_session().await?;
                let action = Self::find_local_action_static(&session, "pick", true)
                    .ok_or_else(|| LcuApiError::Other("当前没有可用的 pick 行动".into()))?;
                action
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| LcuApiError::Other("action 缺少 id 字段".into()))?
            }
        };

        self.patch_json(
            &format!("/lol-champ-select/v1/session/actions/{aid}"),
            json!({"championId": champion_id, "completed": completed}),
        )
        .await
    }

    pub async fn hover_champion(
        &self,
        champion_id: i64,
        action_id: Option<i64>,
    ) -> Result<Value, LcuApiError> {
        self.act_champion(champion_id, false, action_id).await
    }

    pub async fn lock_champion(
        &self,
        champion_id: i64,
        action_id: Option<i64>,
    ) -> Result<Value, LcuApiError> {
        self.act_champion(champion_id, true, action_id).await
    }

    pub async fn reroll_aram(&self) -> Result<Value, LcuApiError> {
        self.post_json("/lol-champ-select/v1/session/my-selection/reroll", None)
            .await
    }

    pub async fn swap_bench_champion(&self, champion_id: i64) -> Result<Value, LcuApiError> {
        self.post_json(
            &format!("/lol-champ-select/v1/session/bench/swap/{champion_id}"),
            None,
        )
        .await
    }

    /// 构建英雄 ID -> 名称 的映射表。
    pub async fn get_champion_id_name_map(
        &self,
    ) -> Result<HashMap<i64, String>, LcuApiError> {
        let champions = self.get_owned_champions_minimal().await?;
        let mut map = HashMap::new();
        for champ in &champions {
            let id = match champ.get("id").and_then(|v| v.as_i64()) {
                Some(v) => v,
                None => continue,
            };
            let name = champ
                .get("name")
                .or_else(|| champ.get("alias"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned())
                .unwrap_or_else(|| format!("Champion-{id}"));
            map.insert(id, name);
        }
        Ok(map)
    }

    // ── Session 工具方法（纯计算，不需要异步）────────────────────────

    /// 提取 bench 中的英雄 ID 列表（兼容两种字段格式）。
    pub fn extract_bench_champion_ids(session: &Value) -> Vec<i64> {
        if let Some(ids) = session.get("benchChampionIds").and_then(|v| v.as_array()) {
            if !ids.is_empty() {
                return ids.iter().filter_map(|x| x.as_i64()).collect();
            }
        }
        if let Some(champions) = session.get("benchChampions").and_then(|v| v.as_array()) {
            return champions
                .iter()
                .filter_map(|c| c.get("championId")?.as_i64())
                .collect();
        }
        vec![]
    }

    /// 获取本地玩家信息。
    pub fn get_local_player(session: &Value) -> Option<&Value> {
        let local_cell_id = session.get("localPlayerCellId")?;
        session
            .get("myTeam")?
            .as_array()?
            .iter()
            .find(|p| p.get("cellId") == Some(local_cell_id))
    }

    /// 遍历 session 中所有行动（扁平化双层数组）。
    pub fn iter_actions(session: &Value) -> Vec<&Value> {
        let mut result = vec![];
        if let Some(actions) = session.get("actions").and_then(|v| v.as_array()) {
            for action_set in actions {
                if let Some(arr) = action_set.as_array() {
                    for action in arr {
                        if action.is_object() {
                            result.push(action);
                        }
                    }
                } else if action_set.is_object() {
                    result.push(action_set);
                }
            }
        }
        result
    }

    /// 查找本地玩家当前的行动（静态方法，可在无 self 时调用）。
    pub fn find_local_action_static<'a>(
        session: &'a Value,
        action_type: &str,
        only_unfinished: bool,
    ) -> Option<&'a Value> {
        let local_cell_id = session.get("localPlayerCellId")?;
        Self::iter_actions(session).into_iter().find(|action| {
            if action.get("actorCellId") != Some(local_cell_id) {
                return false;
            }
            if action.get("type").and_then(|v| v.as_str()) != Some(action_type) {
                return false;
            }
            if only_unfinished
                && action
                    .get("completed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            {
                return false;
            }
            true
        })
    }
}
