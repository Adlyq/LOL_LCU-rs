//! LCU WebSocket 事件循环与订阅分发
//!
//! 架构：
//! - `WsLoop` 持有一个 `tokio::sync::broadcast` channel。
//! - `spawn_ws_loop()` 在后台任务中读取 WebSocket 消息，过滤后广播 `LcuEvent`。

use std::sync::Arc;
use futures_util::{sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async_with_config, tungstenite::protocol::WebSocketConfig};
use tracing::{debug, error, info, trace, warn};

use super::connection::LcuCredentials;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcuEvent {
    pub opcode: String,
    pub event_type: String,
    pub uri: String,
    pub payload: Value,
}

pub struct WsHandle {
    tx: broadcast::Sender<LcuEvent>,
    _task: tokio::task::JoinHandle<()>,
}

impl WsHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<LcuEvent> {
        self.tx.subscribe()
    }
}

pub async fn spawn_ws_loop(creds: &LcuCredentials) -> anyhow::Result<WsHandle> {
    let (tx, _) = broadcast::channel(1024);
    let tx_c = tx.clone();
    let url = format!("wss://127.0.0.1:{}/", creds.port);
    let auth = format!("riot:{}", creds.token);
    let auth_base64 = base64::encode(auth);

    let mut request = reqwest::Request::new(reqwest::Method::GET, url.parse()?);
    request.headers_mut().insert("Authorization", format!("Basic {auth_base64}").parse()?);

    let config = WebSocketConfig {
        max_message_size: Some(64 * 1024 * 1024),
        max_frame_size: Some(64 * 1024 * 1024),
        ..Default::default()
    };

    info!("正在连接 LCU WebSocket: wss://127.0.0.1:{}...", creds.port);
    let (ws_stream, _) = connect_async_with_config(url, Some(config), true).await?;
    let (mut write, mut read) = ws_stream.split();

    // 订阅所有事件 (Json RPC [5, "OnJsonApiEvent"])
    write.send(tokio_tungstenite::tungstenite::Message::Text(json!([5, "OnJsonApiEvent"]).to_string())).await?;
    info!("已成功订阅 LCU OnJsonApiEvent");

    let task = tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                    trace!("WS 原始消息: {}", text);
                    if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&text) {
                        if arr.len() >= 3 && arr[0] == 8 && arr[1] == "OnJsonApiEvent" {
                            if let Ok(event) = serde_json::from_value::<LcuEvent>(arr[2].clone()) {
                                // 仅广播我们感兴趣的事件以减少总线压力
                                let uri = &event.uri;
                                if uri.contains("gameflow") || uri.contains("champ-select") || uri.contains("ready-check") || uri.contains("lobby") {
                                    debug!("WS 广播事件: {} ({})", uri, event.event_type);
                                    let _ = tx_c.send(event);
                                }
                            }
                        }
                    }
                }
                Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => {
                    warn!("LCU WebSocket 连接已关闭");
                    break;
                }
                Err(e) => {
                    error!("LCU WebSocket 读取错误: {}", e);
                    break;
                }
                _ => {}
            }
        }
        info!("WebSocket 任务已退出");
    });

    Ok(WsHandle { tx, _task: task })
}
