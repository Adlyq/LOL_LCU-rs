//! LCU HTTP API 封装
//!
//! 对应 Python 侧的 `api/api.py`（`LcuApi` 类）。
//!
//! 设计原则：
//! - 每个方法仅做单一 HTTP 调用 + 结果反序列化；
//! - 复杂业务逻辑（retry、状态判断）放在 `app` 层；
//! - 错误统一包装为 `LcuApiError`。

#![allow(dead_code)]

use anyhow::Result;
use reqwest::{Client, Response};
use serde_json::{json, Value};
use thiserror::Error;
use tracing::debug;

use super::connection::LcuCredentials;

/// LCU API 调用失败时的错误类型。
#[derive(Debug, Error)]
pub enum LcuApiError {
    #[error("HTTP {status} {method} {endpoint}: {body}")]
    Http {
        status: u16,
        method: String,
        endpoint: String,
        body: String,
    },
    #[error("网络错误: {0}")]
    Network(#[from] reqwest::Error),
    #[error("JSON 解析错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

/// 游戏流程阶段字符串常量。
#[allow(dead_code)]
pub mod gameflow {
    pub const NONE: &str = "None";
    pub const LOBBY: &str = "Lobby";
    pub const MATCHMAKING: &str = "Matchmaking";
    pub const READY_CHECK: &str = "ReadyCheck";
    pub const CHAMP_SELECT: &str = "ChampSelect";
    pub const GAME_START: &str = "GameStart";
    pub const IN_PROGRESS: &str = "InProgress";
    pub const RECONNECT: &str = "Reconnect";
    pub const WAITING_FOR_STATS: &str = "WaitingForStats";
    pub const PRE_END_OF_GAME: &str = "PreEndOfGame";
    pub const END_OF_GAME: &str = "EndOfGame";
    pub const TERMINATED_IN_ERROR: &str = "TerminatedInError";
}

/// LCU HTTP API 客户端。
///
/// `Clone` 开销极小（reqwest::Client 内部是 Arc）。
#[derive(Clone)]
pub struct LcuClient {
    client: Client,
    base_url: String,
}

impl LcuClient {
    /// 根据凭据构建客户端。
    pub fn new(creds: &LcuCredentials, http_client: Client) -> Self {
        Self {
            client: http_client,
            base_url: format!("https://127.0.0.1:{}", creds.port),
        }
    }

    fn url(&self, endpoint: &str) -> String {
        format!("{}{}", self.base_url, endpoint)
    }

    // ── 底层 HTTP 方法 ─────────────────────────────────────────────

    async fn raw_get(&self, endpoint: &str) -> Result<Response, LcuApiError> {
        let resp = self.client.get(self.url(endpoint)).send().await?;
        Self::check_status(resp, "GET", endpoint).await
    }

    async fn raw_post(
        &self,
        endpoint: &str,
        body: Option<Value>,
    ) -> Result<Response, LcuApiError> {
        let req = self.client.post(self.url(endpoint));
        let req = match body {
            Some(v) => req.json(&v),
            None => req,
        };
        let resp = req.send().await?;
        Self::check_status(resp, "POST", endpoint).await
    }

    async fn raw_patch(
        &self,
        endpoint: &str,
        body: Value,
    ) -> Result<Response, LcuApiError> {
        let resp = self.client.patch(self.url(endpoint)).json(&body).send().await?;
        Self::check_status(resp, "PATCH", endpoint).await
    }

    async fn raw_delete(&self, endpoint: &str) -> Result<Response, LcuApiError> {
        let resp = self.client.delete(self.url(endpoint)).send().await?;
        Self::check_status(resp, "DELETE", endpoint).await
    }

    async fn check_status(
        resp: Response,
        method: &str,
        endpoint: &str,
    ) -> Result<Response, LcuApiError> {
        if resp.status().as_u16() >= 400 {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LcuApiError::Http {
                status,
                method: method.to_owned(),
                endpoint: endpoint.to_owned(),
                body,
            });
        }
        Ok(resp)
    }

    /// 解析响应体为 JSON；空体返回 `Value::Null`。
    async fn json_or_null(resp: Response) -> Result<Value, LcuApiError> {
        let text = resp.text().await?;
        if text.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text)?)
    }

    async fn get_json(&self, endpoint: &str) -> Result<Value, LcuApiError> {
        debug!("GET {endpoint}");
        let resp = self.raw_get(endpoint).await?;
        Self::json_or_null(resp).await
    }

    async fn post_json(
        &self,
        endpoint: &str,
        body: Option<Value>,
    ) -> Result<Value, LcuApiError> {
        debug!("POST {endpoint}");
        let resp = self.raw_post(endpoint, body).await?;
        Self::json_or_null(resp).await
    }

    async fn patch_json(
        &self,
        endpoint: &str,
        body: Value,
    ) -> Result<Value, LcuApiError> {
        debug!("PATCH {endpoint}");
        let resp = self.raw_patch(endpoint, body).await?;
        Self::json_or_null(resp).await
    }

    async fn delete_json(&self, endpoint: &str) -> Result<Value, LcuApiError> {
        debug!("DELETE {endpoint}");
        let resp = self.raw_delete(endpoint).await?;
        Self::json_or_null(resp).await
    }

    // ── 游戏流程 ────────────────────────────────────────────────────

    /// 获取当前游戏流程阶段字符串（如 `"ChampSelect"`）。
    pub async fn get_gameflow_phase(&self) -> Result<String, LcuApiError> {
        let v = self.get_json("/lol-gameflow/v1/gameflow-phase").await?;
        Ok(v.as_str().unwrap_or("None").to_owned())
    }

    pub async fn get_gameflow_session(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-gameflow/v1/session").await
    }

    /// 获取 Riot 客户端缩放比例。
    pub async fn get_riotclient_zoom_scale(&self) -> Result<f64, LcuApiError> {
        let v = self.get_json("/riotclient/zoom-scale").await?;
        v.as_f64().ok_or_else(|| {
            LcuApiError::Other(format!("无效的 zoom-scale 响应: {v:?}"))
        })
    }

    /// 热重载 LCU 客户端界面（不会断开排队 / 游戏连接）。
    ///
    /// 调用 `POST /riotclient/kill-and-restart-ux`：RiotClient 终止并重启
    /// LeagueClientUx 进程，仅重载 UI 层，不影响游戏状态。
    pub async fn reload_ux(&self) -> Result<(), LcuApiError> {
        self.post_json("/riotclient/kill-and-restart-ux", None).await?;
        Ok(())
    }

    /// 退出结算界面，返回大厅。
    ///
    /// 调用 `POST /lol-lobby/v2/play-again`：适用于结算页面卡住无法手动退出的情况。
    pub async fn play_again(&self) -> Result<(), LcuApiError> {
        self.post_json("/lol-lobby/v2/play-again", None).await?;
        Ok(())
    }

    // ── 召唤师 ──────────────────────────────────────────────────────

    pub async fn get_current_summoner(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-summoner/v1/current-summoner").await
    }

    // ── 匹配 / 准备确认 ─────────────────────────────────────────────

    pub async fn get_ready_check(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-matchmaking/v1/ready-check").await
    }

    pub async fn accept_ready_check(&self) -> Result<Value, LcuApiError> {
        self.post_json("/lol-matchmaking/v1/ready-check/accept", None).await
    }

    pub async fn decline_ready_check(&self) -> Result<Value, LcuApiError> {
        self.post_json("/lol-matchmaking/v1/ready-check/decline", None).await
    }

    // ── 点赞投票 ────────────────────────────────────────────────────

    pub async fn get_honor_ballot(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-honor-v2/v1/ballot").await
    }

    pub async fn dismiss_end_of_game_stats(&self) -> Result<bool, LcuApiError> {
        match self
            .post_json("/lol-end-of-game/v1/state/dismiss-stats", None)
            .await
        {
            Ok(_) => Ok(true),
            Err(LcuApiError::Http { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// 尝试跳过点赞投票，对应 Python 侧 `skip_honor_vote`。
    ///
    /// 策略（与 Python 完全一致）：
    /// 1. 获取 ballot，提取 gameId；
    /// 2. 依次尝试三种 POST payload 的 `/lol-honor-v2/v1/honor-player`；
    /// 3. 若均失败，尝试 `/lol-honor-v2/v1/ballot/skip` 和 `/lol-honor-v2/v1/skip`。
    pub async fn skip_honor_vote(&self) -> Result<bool, LcuApiError> {
        let ballot = match self.get_honor_ballot().await {
            Ok(v) => v,
            Err(_) => return Ok(false),
        };

        let game_id = ballot.get("gameId").and_then(|v| v.as_i64());

        if let Some(gid) = game_id {
            let payloads = vec![
                json!({"gameId": gid, "honorCategory": "OPT_OUT", "summonerId": 0}),
                json!({"gameId": gid, "honorCategory": "NONE", "summonerId": 0}),
                json!({"gameId": gid, "honorType": "OPT_OUT", "summonerId": 0}),
            ];
            for payload in payloads {
                match self
                    .post_json("/lol-honor-v2/v1/honor-player", Some(payload))
                    .await
                {
                    Ok(_) => return Ok(true),
                    Err(LcuApiError::Http { .. }) => continue,
                    Err(e) => return Err(e),
                }
            }
        }

        for endpoint in &["/lol-honor-v2/v1/ballot/skip", "/lol-honor-v2/v1/skip"] {
            match self.post_json(endpoint, None).await {
                Ok(_) => return Ok(true),
                Err(LcuApiError::Http { .. }) => continue,
                Err(e) => return Err(e),
            }
        }

        Ok(false)
    }

    // ── 英雄选择 ────────────────────────────────────────────────────

    pub async fn get_champ_select_session(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-champ-select/v1/session").await
    }

    pub async fn get_pickable_champion_ids(&self) -> Result<Vec<i64>, LcuApiError> {
        let v = self.get_json("/lol-champ-select/v1/pickable-champion-ids").await?;
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
                    .ok_or_else(|| {
                        LcuApiError::Other("当前没有可用的 pick 行动".into())
                    })?;
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
        self.post_json(
            "/lol-champ-select/v1/session/my-selection/reroll",
            None,
        )
        .await
    }

    pub async fn swap_bench_champion(&self, champion_id: i64) -> Result<Value, LcuApiError> {
        self.post_json(
            &format!("/lol-champ-select/v1/session/bench/swap/{champion_id}"),
            None,
        )
        .await
    }

    // ── Session 工具方法（纯计算，不需要异步）────────────────────────

    /// 提取 bench 中的英雄 ID 列表（兼容两种字段格式）。
    pub fn extract_bench_champion_ids(session: &Value) -> Vec<i64> {
        if let Some(ids) = session.get("benchChampionIds").and_then(|v| v.as_array()) {
            if !ids.is_empty() {
                return ids.iter().filter_map(|x| x.as_i64()).collect();
            }
        }
        // 旧版格式：benchChampions: [{ championId: ... }]
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

    /// 构建英雄 ID -> 名称 的映射表。
    pub async fn get_champion_id_name_map(
        &self,
    ) -> Result<std::collections::HashMap<i64, String>, LcuApiError> {
        let champions = self.get_owned_champions_minimal().await?;
        let mut map = std::collections::HashMap::new();
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

    // ── 聊天 ────────────────────────────────────────────────────────

    /// 获取自己的聊天信息（jid / pid 等）。
    ///
    /// `GET /lol-chat/v1/me`
    pub async fn get_chat_me(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-chat/v1/me").await
    }

    /// 创建或复用与指定 pid（XMPP name）的单聊会话，返回 conversation id。
    ///
    /// `POST /lol-chat/v1/conversations`  body: `{"pid": pid, "type": "chat"}`
    pub async fn open_conversation(&self, pid: &str) -> Result<String, LcuApiError> {
        let v = self
            .post_json(
                "/lol-chat/v1/conversations",
                Some(json!({"pid": pid, "type": "chat"})),
            )
            .await?;
        v.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned())
            .ok_or_else(|| LcuApiError::Other("open_conversation：响应缺少 id 字段".into()))
    }

    /// 向指定会话发送消息。
    ///
    /// `POST /lol-chat/v1/conversations/{id}/messages`
    pub async fn send_chat_message(
        &self,
        conversation_id: &str,
        body: &str,
    ) -> Result<(), LcuApiError> {
        self.post_json(
            &format!("/lol-chat/v1/conversations/{conversation_id}/messages"),
            Some(json!({"body": body, "type": "chat"})),
        )
        .await?;
        Ok(())
    }

    /// 向自己发送私信（仅自己可见）。
    ///
    /// 流程：GET /lol-chat/v1/me → 拿 pid → POST 创建会话 → POST 发消息。
    pub async fn send_message_to_self(&self, body: &str) -> Result<(), LcuApiError> {
        let me = self.get_chat_me().await?;
        let pid = me
            .get("pid")
            .and_then(|v| v.as_str())
            .ok_or_else(|| LcuApiError::Other("get_chat_me：响应缺少 pid 字段".into()))?
            .to_owned();
        let conv_id = self.open_conversation(&pid).await?;
        self.send_chat_message(&conv_id, body).await
    }

    // ── 战绩 ────────────────────────────────────────────────────────

    /// 获取指定 PUUID 的最近场次战绩（简要列表）。
    ///
    /// `GET /lol-match-history/v1/products/lol/{puuid}/matches?begIndex=0&endIndex={end}`
    pub async fn get_match_history(
        &self,
        puuid: &str,
        count: usize,
    ) -> Result<Value, LcuApiError> {
        let end = count.saturating_sub(1);
        self.get_json(&format!(
            "/lol-match-history/v1/products/lol/{puuid}/matches?begIndex=0&endIndex={end}"
        ))
        .await
    }

    // ── 任务 ────────────────────────────────────────────────────────

    /// 获取当前账号所有任务（含进行中、已完成等）。
    ///
    /// `GET /lol-missions/v1/missions`
    pub async fn get_missions(&self) -> Result<Vec<Value>, LcuApiError> {
        let v = self.get_json("/lol-missions/v1/missions").await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    /// 领取指定任务的奖励。
    ///
    /// `POST /lol-missions/v1/missions/{id}/claim`
    pub async fn claim_mission(&self, mission_id: &str) -> Result<(), LcuApiError> {
        self.post_json(
            &format!("/lol-missions/v1/missions/{mission_id}/claim"),
            None,
        )
        .await?;
        Ok(())
    }

    // ── 战利品 ──────────────────────────────────────────────────────

    /// 获取当前账号全部战利品。
    ///
    /// `GET /lol-loot/v1/player-loot`
    pub async fn get_player_loot(&self) -> Result<Vec<Value>, LcuApiError> {
        let v = self.get_json("/lol-loot/v1/player-loot").await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    /// 获取指定战利品的所有可用配方。
    ///
    /// `GET /lol-loot/v1/recipes/initial-item/{lootId}`
    pub async fn get_loot_recipes(&self, loot_id: &str) -> Result<Vec<Value>, LcuApiError> {
        let v = self
            .get_json(&format!("/lol-loot/v1/recipes/initial-item/{loot_id}"))
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    /// 合成/开启指定战利品。
    ///
    /// `POST /lol-loot/v1/recipes/initial-item/{lootId}/craft?repeat={n}`
    ///
    /// - `ingredients`：配方所有 slot 的材料 lootId（按 slot 顺序排列）
    /// - `repeat`：批量合成次数
    pub async fn craft_loot(
        &self,
        loot_id: &str,
        ingredients: Vec<String>,
        repeat: u32,
    ) -> Result<(), LcuApiError> {
        self.post_json(
            &format!("/lol-loot/v1/recipes/initial-item/{loot_id}/craft?repeat={repeat}"),
            Some(json!(ingredients)),
        )
        .await?;
        Ok(())
    }
}
