use serde_json::{json, Value};
use tracing::{debug, warn};
use crate::lcu::api::{LcuClient, LcuApiError};

impl LcuClient {
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
        // 1. 尝试 LCU API
        let lcu_res = self.get_match_history_lcu(puuid, count).await;
        if let Ok(ref v) = lcu_res {
            if Self::is_match_history_valid(v) {
                return lcu_res;
            }
        }

        // 2. Fallback to SGP
        debug!(
            "LCU 战绩为空或失败，尝试通过 SGP 获取 (PUUID={})",
            &puuid[..8.min(puuid.len())]
        );
        match self.get_match_history_sgp(puuid, count).await {
            Ok(v) => Ok(v),
            Err(e) => {
                warn!("SGP 战绩获取也失败: {e}");
                lcu_res
            }
        }
    }

    async fn get_match_history_lcu(&self, puuid: &str, count: usize) -> Result<Value, LcuApiError> {
        let end = count.saturating_sub(1);
        let endpoint =
            format!("/lol-match-history/v1/products/lol/{puuid}/matches?begIndex=0&endIndex={end}");

        let mut retry_count = 0;
        let max_retries = 2;

        loop {
            let res = self.get_json(&endpoint).await;
            if let Ok(ref v) = res {
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

    async fn get_match_history_sgp(&self, puuid: &str, count: usize) -> Result<Value, LcuApiError> {
        let access_token = self.get_access_token().await?;
        let ent_token = self.get_entitlements_token().await?;

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

        if v.is_array() {
            v = json!({ "games": { "games": v } });
        } else if let Some(games) = v.get_mut("games") {
            if games.is_array() {
                v = json!({ "games": { "games": games.take() } });
            }
        } else if v.get("games").is_none() {
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
}
