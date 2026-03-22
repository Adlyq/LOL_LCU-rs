use serde_json::Value;

use crate::lcu::api::LcuClient;
use crate::win::overlay::OverlaySender;
use crate::app::config::SharedConfig;
use crate::app::state::SharedState;

pub async fn handle_lobby(
    _api: LcuClient,
    _state: SharedState,
    _config: SharedConfig,
    _overlay_tx: OverlaySender,
    _event: Value,
) {
    // 房间成员显示逻辑已移除
}
