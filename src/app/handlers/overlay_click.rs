use std::time::Duration;

use crate::lcu::api::LcuClient;
use crate::win::overlay::{OverlayCmd, OverlaySender};
use crate::app::state::SharedState;

pub async fn handle_overlay_click(
    api: LcuClient,
    state: SharedState,
    overlay_tx: OverlaySender,
    slot_index: usize,
) {
    let action = {
        let mut s = state.lock();
        if slot_index >= s.current_bench_ids.len() {
            return;
        }

        if s.active_pick_slot == Some(slot_index) {
            // 再次点击同槽位：取消
            s.cancel_pick_task();
            false // 表示已取消
        } else {
            // 点击新槽位：启动抢人任务
            let champion_id = s.current_bench_ids[slot_index];
            s.cancel_pick_task();
            s.active_pick_slot = Some(slot_index);
            let gen = s.pick_generation;
            
            let api_c = api.clone();
            let state_c = state.clone();
            let tx_c = overlay_tx.clone();
            let handle = tokio::spawn(async move {
                loop_pick_until_refresh(api_c, state_c, tx_c, champion_id, gen, slot_index).await;
            });
            s.pick_task = Some(handle);
            true // 表示新开始
        }
    };

    if action {
        let _ = overlay_tx.send(OverlayCmd::SetSelectedSlot(slot_index)).await;
    } else {
        let _ = overlay_tx.send(OverlayCmd::ClearSelectedSlot).await;
    }
}

async fn loop_pick_until_refresh(
    api: LcuClient,
    state: SharedState,
    overlay_tx: OverlaySender,
    champion_id: i64,
    generation: u64,
    _slot_index: usize,
) {
    loop {
        // 1. 检查代次
        if state.lock().pick_generation != generation { return; }

        // 2. 尝试抢人
        let _ = api.swap_bench_champion(champion_id).await;

        tokio::time::sleep(Duration::from_millis(300)).await;

        // 3. 状态检查
        if let Ok(session) = api.get_champ_select_session().await {
            // 是否已在手？
            if let Some(me) = LcuClient::get_local_player(&session) {
                if me.get("championId").and_then(|v| v.as_i64()) == Some(champion_id) {
                    break;
                }
            }
            // 英雄是否还在板凳？
            let bench = LcuClient::extract_bench_champion_ids(&session);
            if !bench.contains(&champion_id) {
                break;
            }
        } else {
            break; // 获取 session 失败说明选人可能结束了
        }
    }

    // 清理状态
    let mut s = state.lock();
    if s.pick_generation == generation {
        s.active_pick_slot = None;
        let tx = overlay_tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(OverlayCmd::ClearSelectedSlot).await;
        });
    }
}
