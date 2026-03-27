use serde_json::Value;
use crate::lcu::api::{LcuClient, LcuApiError};

impl LcuClient {
    /// 退出结算界面，返回大厅。
    pub async fn play_again(&self) -> Result<(), LcuApiError> {
        self.post_json("/lol-lobby/v2/play-again", None).await?;
        Ok(())
    }

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
}
