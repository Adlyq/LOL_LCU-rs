use serde_json::Value;
use crate::lcu::api::{LcuClient, LcuApiError};

/// 游戏流程阶段字符串常量。
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

impl LcuClient {
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
    pub async fn reload_ux(&self) -> Result<(), LcuApiError> {
        self.post_json("/riotclient/kill-and-restart-ux", None)
            .await?;
        Ok(())
    }
}
