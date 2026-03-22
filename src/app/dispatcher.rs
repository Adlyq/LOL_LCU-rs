use crate::lcu::api::LcuClient;
use crate::lcu::websocket::LcuEvent;
use crate::app::state::SharedState;
use crate::app::config::SharedConfig;
use crate::win::overlay::OverlaySender;
use crate::app::handlers;

pub async fn dispatch_lcu_event(
    api: LcuClient,
    state: SharedState,
    config: SharedConfig,
    overlay_tx: OverlaySender,
    event: LcuEvent,
) {
    let uri = event.uri.as_str();
    let payload = event.payload;
    
    // 路由逻辑收敛于此
    if uri == "/lol-matchmaking/v1/ready-check" {
        handlers::handle_ready_check(api, state, config, payload).await;
    } else if uri == "/lol-gameflow/v1/gameflow-phase" {
        handlers::handle_gameflow(api, state, config, overlay_tx, payload).await;
    } else if uri == "/lol-honor-v2/v1/ballot" {
        handlers::handle_honor_ballot(api, state, config, overlay_tx, payload).await;
    } else if uri == "/lol-champ-select/v1/session" {
        handlers::handle_champ_select(api, state, config, overlay_tx, payload).await;
    } else if uri == "/lol-lobby/v2/lobby" {
        handlers::handle_lobby(api, state, config, overlay_tx, payload).await;
    }
}
