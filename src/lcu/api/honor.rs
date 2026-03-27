use serde_json::{json, Value};
use crate::lcu::api::{LcuClient, LcuApiError};

impl LcuClient {
    pub async fn get_honor_ballot(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-honor-v2/v1/ballot").await
    }

    /// 尝试跳过点赞投票，对应 Python 侧 `skip_honor_vote`。
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
}
