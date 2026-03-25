use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::app::event::AppEvent;
use crate::lcu::api::LcuClient;

pub struct SniperService {
    api: LcuClient,
    event_tx: mpsc::Sender<AppEvent>,
}

impl SniperService {
    pub fn new(api: LcuClient, event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self { api, event_tx }
    }

    pub async fn start_sniping(
        &self,
        champion_id: i64,
        slot_index: usize,
        token: CancellationToken,
    ) {
        let api = self.api.clone();
        let event_tx = self.event_tx.clone();

        info!(
            "[Sniper] 启动抢英雄任务: ID={}, Slot={}",
            champion_id, slot_index
        );

        tokio::spawn(async move {
            loop {
                if token.is_cancelled() {
                    break;
                }

                // 1. 尝试交换
                let _ = api.swap_bench_champion(champion_id).await;

                // 2. 间隔
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(300)) => {}
                    _ = token.cancelled() => break,
                }

                // 3. 检查状态
                match api.get_champ_select_session().await {
                    Ok(session) => {
                        if let Some(me) = LcuClient::get_local_player(&session) {
                            if me.get("championId").and_then(|v| v.as_i64()) == Some(champion_id) {
                                info!("[Sniper] 成功抢到英雄！");
                                break;
                            }
                        }
                        let bench = LcuClient::extract_bench_champion_ids(&session);
                        if !bench.contains(&champion_id) {
                            warn!("[Sniper] 英雄已消失");
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }

            info!("[Sniper] 抢英雄任务结束");
            let _ = event_tx.send(AppEvent::SniperFinished(slot_index)).await;
        });
    }
}
