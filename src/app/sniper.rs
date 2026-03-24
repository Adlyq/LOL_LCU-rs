use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, debug, warn, error};

use crate::lcu::api::LcuClient;
use crate::app::event::AppEvent;

pub struct SniperService {
    api: LcuClient,
    event_tx: mpsc::Sender<AppEvent>,
}

impl SniperService {
    pub fn new(api: LcuClient, event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self { api, event_tx }
    }

    pub async fn start_sniping(&self, champion_id: i64, slot_index: usize) {
        let api = self.api.clone();
        let event_tx = self.event_tx.clone();
        
        info!("[Sniper] 启动抢英雄任务: ID={}, Slot={}", champion_id, slot_index);

        tokio::spawn(async move {
            let mut attempts = 0;
            loop {
                attempts += 1;
                // 尝试交换
                match api.swap_bench_champion(champion_id).await {
                    Ok(_) => {
                        debug!("[Sniper] 抢英雄请求已发送 (第 {} 次)", attempts);
                    }
                    Err(e) => {
                        debug!("[Sniper] 抢英雄请求失败 (可能已被抢或不在板凳): {} (第 {} 次)", e, attempts);
                    }
                }

                tokio::time::sleep(Duration::from_millis(300)).await;

                // 检查状态：是否已抢到或英雄已消失
                match api.get_champ_select_session().await {
                    Ok(session) => {
                        // 检查自己是否已持有该英雄
                        if let Some(me) = LcuClient::get_local_player(&session) {
                            if me.get("championId").and_then(|v| v.as_i64()) == Some(champion_id) {
                                info!("[Sniper] 成功抢到英雄: {}！", champion_id);
                                break;
                            }
                        }
                        
                        // 检查板凳席是否还有该英雄
                        let bench = LcuClient::extract_bench_champion_ids(&session);
                        if !bench.contains(&champion_id) {
                            warn!("[Sniper] 英雄已从板凳席消失，放弃任务");
                            break;
                        }
                    }
                    Err(e) => {
                        error!("[Sniper] 获取选人会话失败，停止抢人: {}", e);
                        break;
                    }
                }
            }

            info!("[Sniper] 抢英雄任务结束");
            // 通知主循环任务结束，清理 UI 高亮
            let _ = event_tx.send(AppEvent::BenchClick(slot_index)).await;
        });
    }
}
