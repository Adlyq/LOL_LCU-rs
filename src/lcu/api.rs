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
use tracing::{debug, warn};

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
#[derive(Clone, Debug)]
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

    async fn raw_post(&self, endpoint: &str, body: Option<Value>) -> Result<Response, LcuApiError> {
        let req = self.client.post(self.url(endpoint));
        let req = match body {
            Some(v) => req.json(&v),
            None => req,
        };
        let resp = req.send().await?;
        Self::check_status(resp, "POST", endpoint).await
    }

    async fn raw_patch(&self, endpoint: &str, body: Value) -> Result<Response, LcuApiError> {
        let resp = self
            .client
            .patch(self.url(endpoint))
            .json(&body)
            .send()
            .await?;
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

    async fn post_json(&self, endpoint: &str, body: Option<Value>) -> Result<Value, LcuApiError> {
        debug!("POST {endpoint}");
        let resp = self.raw_post(endpoint, body).await?;
        Self::json_or_null(resp).await
    }

    async fn patch_json(&self, endpoint: &str, body: Value) -> Result<Value, LcuApiError> {
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
        v.as_f64()
            .ok_or_else(|| LcuApiError::Other(format!("无效的 zoom-scale 响应: {v:?}")))
    }

    /// 热重载 LCU 客户端界面（不会断开排队 / 游戏连接）。
    ///
    /// 调用 `POST /riotclient/kill-and-restart-ux`：RiotClient 终止并重启
    /// LeagueClientUx 进程，仅重载 UI 层，不影响游戏状态。
    pub async fn reload_ux(&self) -> Result<(), LcuApiError> {
        self.post_json("/riotclient/kill-and-restart-ux", None)
            .await?;
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

    pub async fn get_lobby(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-lobby/v2/lobby").await
    }

    pub async fn get_ready_check(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-matchmaking/v1/ready-check").await
    }

    pub async fn accept_ready_check(&self) -> Result<Value, LcuApiError> {
        self.post_json("/lol-matchmaking/v1/ready-check/accept", None)
            .await
    }

    pub async fn decline_ready_check(&self) -> Result<Value, LcuApiError> {
        self.post_json("/lol-matchmaking/v1/ready-check/decline", None)
            .await
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

    /// 获取单场游戏的详细统计数据。
    pub async fn get_game(&self, game_id: i64) -> Result<Value, LcuApiError> {
        self.get_json(&format!("/lol-match-history/v1/games/{game_id}"))
            .await
    }

    // ── 凭据与 Token ───────────────────────────────────────────────

    /// 获取 Entitlements Token (X-Riot-Entitlements-JWT)。
    pub async fn get_entitlements_token(&self) -> Result<String, LcuApiError> {
        let v = self.get_json("/lol-entitlements/v1/token").await?;
        v.get("accessToken")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned())
            .ok_or_else(|| LcuApiError::Other("未找到 entitlements token".into()))
    }

    /// 获取 RSO Access Token (Authorization: Bearer ...)。
    pub async fn get_access_token(&self) -> Result<String, LcuApiError> {
        let v = self
            .get_json("/lol-rso-auth/v1/authorization/access-token")
            .await?;
        v.get("token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned())
            .ok_or_else(|| LcuApiError::Other("未找到 access token".into()))
    }

    // ── 战绩 (LCU + SGP) ───────────────────────────────────────────

    /// 获取指定 PUUID 的最近场次战绩。
    /// 策略：优先 LCU 本地缓存，失败或数据不全则尝试 SGP (Riot 远程接口)。
    pub async fn get_match_history(&self, puuid: &str, count: usize) -> Result<Value, LcuApiError> {
        // 1. 尝试 LCU API (带重试逻辑，应对进入游戏初期 LCU 尚未加载完战绩的情况)
        let lcu_res = self.get_match_history_lcu(puuid, count).await;
        if let Ok(ref v) = lcu_res {
            if Self::is_match_history_valid(v) {
                return lcu_res;
            }
        }

        // 2. 如果 LCU 战绩为空或获取失败，尝试通过 SGP 获取 (Fallback)
        debug!(
            "LCU 战绩为空或失败，尝试通过 SGP 获取 (PUUID={})",
            &puuid[..8.min(puuid.len())]
        );
        match self.get_match_history_sgp(puuid, count).await {
            Ok(v) => Ok(v),
            Err(e) => {
                warn!("SGP 战绩获取也失败: {e}");
                // 如果 SGP 也失败了，返回之前 LCU 的结果
                lcu_res
            }
        }
    }

    /// 内部：仅通过 LCU 获取战绩。
    async fn get_match_history_lcu(&self, puuid: &str, count: usize) -> Result<Value, LcuApiError> {
        let end = count.saturating_sub(1);
        let endpoint =
            format!("/lol-match-history/v1/products/lol/{puuid}/matches?begIndex=0&endIndex={end}");

        let mut retry_count = 0;
        let max_retries = 2;

        loop {
            let res = self.get_json(&endpoint).await;
            if let Ok(ref v) = res {
                // 如果拿到了有效的列表（games 字段存在且有内容），则直接返回
                if Self::is_match_history_valid(v) {
                    return res;
                }
            }

            if retry_count >= max_retries {
                return res;
            }

            retry_count += 1;
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        }
    }

    /// 内部：通过 SGP 获取战绩。
    /// SGP 是 Riot 的 Service Gateway 接口，LCU 也是通过它同步战绩。
    async fn get_match_history_sgp(&self, puuid: &str, count: usize) -> Result<Value, LcuApiError> {
        let access_token = self.get_access_token().await?;
        let ent_token = self.get_entitlements_token().await?;

        // 自动探测 Region
        let region = self
            .get_json("/riotclient/region-locale")
            .await
            .map(|v| {
                v.get("region")
                    .and_then(|s| s.as_str())
                    .unwrap_or("hn1")
                    .to_lowercase()
            })
            .unwrap_or_else(|_| "hn1".to_string());

        // 国服环境使用通用的 BGP 代理，非国服使用 sgp.pvp.net
        let url = if region.contains("hn")
            || region.contains("tj")
            || region.contains("sh")
            || region.contains("gz")
        {
            format!("https://bgp.pallas.penta.qq.com/sgp/shno/v1/products/lol/player-history/v1/products/lol/{}/matches?begIndex=0&endIndex={}", 
                puuid, count.saturating_sub(1))
        } else {
            format!("https://sgp.pvp.net/match-history-query/v1/products/lol/player/{}/SUMMARY?count={}", 
                puuid, count)
        };

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("X-Riot-Entitlements-JWT", ent_token)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(LcuApiError::Http {
                status: resp.status().as_u16(),
                method: "GET (SGP)".into(),
                endpoint: url,
                body: resp.text().await.unwrap_or_default(),
            });
        }

        let mut v = resp.json::<Value>().await?;

        // 适配 LCU 格式：SGP 返回的是摘要列表，LCU 期望包裹在 { games: { games: [...] } } 中
        // 某些 SGP 接口返回 { games: [...] }，某些直接返回数组 [...]
        if v.is_array() {
            v = json!({ "games": { "games": v } });
        } else if let Some(games) = v.get_mut("games") {
            if games.is_array() {
                v = json!({ "games": { "games": games.take() } });
            }
        } else if v.get("games").is_none() {
            // 如果既不是数组也没有 games 字段，可能是单场数据
            v = json!({ "games": { "games": [v] } });
        }

        Ok(v)
    }

    fn is_match_history_valid(v: &Value) -> bool {
        let games = v
            .get("games")
            .and_then(|g| {
                if g.is_array() {
                    Some(g)
                } else {
                    g.get("games")
                }
            })
            .and_then(|arr| arr.as_array());

        match games {
            Some(arr) => !arr.is_empty(),
            None => false,
        }
    }

    // ── 战利品 / 奖励 ───────────────────────────────────────────────

    pub async fn get_player_loot(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-loot/v1/player-loot").await
    }

    pub async fn call_loot_recipe(
        &self,
        loot_id: &str,
        recipe_name: &str,
    ) -> Result<Value, LcuApiError> {
        self.post_json(
            &format!("/lol-loot/v1/recipes/{recipe_name}/craft?repeat=1"),
            Some(serde_json::json!([loot_id])),
        )
        .await
    }
}
