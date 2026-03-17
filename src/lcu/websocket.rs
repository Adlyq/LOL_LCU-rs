//! LCU WebSocket 事件循环与订阅分发
//!
//! 对应 Python 侧的 `willump_runtime.start_websocket_with_limits()`。
//!
//! 架构：
//! - `WsLoop` 持有一个 `tokio::sync::broadcast` channel。
//! - `spawn_ws_loop()` 在后台任务中读取 WebSocket 消息，过滤后广播 `LcuEvent`。
//! - 调用方通过 `subscribe()` 获得 `broadcast::Receiver`，自行过滤 URI。
//! - 大消息自动丢弃（对应 Python 侧 `ws_auto_drop_large_events`）。
//! - `/lol-champions/v1/inventories/*/champions` 全库存事件直接丢弃。

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tracing::{debug, info, warn};

use super::connection::LcuCredentials;

/// LCU WebSocket 事件（已解析为 JSON）。
#[derive(Debug, Clone)]
pub struct LcuEvent {
    /// 事件 URI，如 `/lol-gameflow/v1/gameflow-phase`
    pub uri: String,
    /// 完整 JSON payload（data[2]）
    pub payload: Value,
}

/// 广播通道容量：足够处理突发事件
const CHANNEL_CAP: usize = 256;

/// 大消息阈值（字节）：超过此值记录警告，默认 4 MiB
const LARGE_EVENT_THRESHOLD: usize = 4 * 1024 * 1024;

/// 最大消息尺寸（字节），默认 64 MiB
#[allow(dead_code)]
const MAX_MSG_SIZE: usize = 64 * 1024 * 1024;

/// WebSocket 循环句柄，通过 `subscribe()` 获取事件接收端。
#[derive(Clone)]
pub struct WsHandle {
    tx: broadcast::Sender<LcuEvent>,
    /// 自动丢弃的 URI 集合（线程安全）
    blocked_uris: Arc<Mutex<HashSet<String>>>,
}

impl WsHandle {
    /// 订阅所有 LCU 事件（调用方自行按 URI 过滤）。
    pub fn subscribe(&self) -> broadcast::Receiver<LcuEvent> {
        self.tx.subscribe()
    }
}

/// 启动 WebSocket 后台监听任务，返回句柄。
///
/// 该函数本身不阻塞——WebSocket 事件循环在 tokio 后台任务中运行。
pub async fn spawn_ws_loop(creds: &LcuCredentials) -> Result<WsHandle> {
    let url = format!("wss://127.0.0.1:{}", creds.port);
    let auth = creds.auth_header.clone();
    let port = creds.port;

    let request = Request::builder()
        .uri(&url)
        .header("Authorization", &auth)
        .header("Host", format!("127.0.0.1:{port}"))
        // Tungstenite 需要这些握手 header
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
        .body(())
        .context("构建 WS 请求失败")?;

    // 接受 LCU 自签名证书
    let connector = build_tls_connector();

    info!("正在连接 LCU WebSocket: {url}");
    let (ws_stream, _resp) =
        connect_async_tls_with_config(request, None, false, Some(connector))
            .await
            .context("WebSocket 握手失败")?;
    info!("LCU WebSocket 已连接");

    let (tx, _rx_dummy) = broadcast::channel::<LcuEvent>(CHANNEL_CAP);
    let handle = WsHandle {
        tx: tx.clone(),
        blocked_uris: Arc::new(Mutex::new(HashSet::new())),
    };

    let blocked_uris = handle.blocked_uris.clone();
    let (mut write, mut read) = ws_stream.split();

    // ── 发送 WAMP 订阅命令 ─────────────────────────────────────────
    // LCU WebSocket 使用 WAMP-like 协议，必须先发 [5, "OnJsonApiEvent"]
    // 服务端才会开始推送事件（对应 Python willump 的 wlp.subscribe("OnJsonApiEvent")）。
    let subscribe_msg = tungstenite::Message::Text(
        serde_json::json!([5, "OnJsonApiEvent"]).to_string().into()
    );
    write
        .send(subscribe_msg)
        .await
        .context("发送 OnJsonApiEvent 订阅命令失败")?;
    info!("已发送 WebSocket 订阅命令 [5, \"OnJsonApiEvent\"]");

    tokio::spawn(async move {
        // write 半端保持存活（不需要再次写入），防止连接被关闭
        let _write_keep = write;

        info!("WebSocket 事件监听循环已启动");

        while let Some(msg) = read.next().await {
            match msg {
                Ok(tungstenite::Message::Text(text)) => {
                    if text.is_empty() {
                        debug!("收到空 WS 消息，跳过");
                        continue;
                    }

                    let msg_size = text.len();
                    let parsed: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("WS JSON 解析失败: {e}");
                            continue;
                        }
                    };

                    // LCU 事件格式: [opcode, "OnJsonApiEvent", { uri, data, ... }]
                    let arr = match parsed.as_array() {
                        Some(a) if a.len() >= 3 => a,
                        _ => {
                            debug!("WS 消息不是预期数组格式，跳过");
                            continue;
                        }
                    };

                    if arr[1].as_str() != Some("OnJsonApiEvent") {
                        debug!("非 OnJsonApiEvent，跳过: {:?}", arr[1]);
                        continue;
                    }

                    let payload = arr[2].clone();
                    let uri = payload
                        .get("uri")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();

                    // 过滤全量英雄库存事件（非常大且无用）
                    if uri.starts_with("/lol-champions/v1/inventories/")
                        && uri.ends_with("/champions")
                    {
                        debug!("pre-drop 全量英雄库存事件: {uri}");
                        continue;
                    }

                    // 已被自动丢弃的 URI
                    {
                        let guard = blocked_uris.lock();
                        if guard.contains(&uri) {
                            continue;
                        }
                    }

                    // 大消息检测
                    if msg_size >= LARGE_EVENT_THRESHOLD {
                        warn!(
                            "大 WS 事件: uri={uri} size={msg_size} 字节，自动加入丢弃列表"
                        );
                        if !uri.is_empty() {
                            blocked_uris.lock().insert(uri.clone());
                            continue;
                        }
                    }

                    let event = LcuEvent { uri, payload };
                    // 忽略无接收者错误（订阅方可能还未就绪）
                    let _ = tx.send(event);
                }
                Ok(tungstenite::Message::Close(_)) => {
                    info!("WS 收到 Close，结束监听循环");
                    break;
                }
                Err(e) => {
                    warn!("WS 错误: {e}，结束监听循环");
                    break;
                }
                _ => {}
            }
        }
        info!("WebSocket 监听循环已退出");
    });

    Ok(handle)
}

/// 构建接受任意证书的 TLS 连接器（native-tls，与 willump 的 `ssl=False` 等价）。
fn build_tls_connector() -> tokio_tungstenite::Connector {
    // 使用 native-tls，禁用证书校验（本地 127.0.0.1，无 MITM 风险）
    // tokio_tungstenite::Connector::NativeTls 接受 native_tls::TlsConnector（非 tokio 包装版）
    let native = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .expect("构建 native-tls connector 失败");
    tokio_tungstenite::Connector::NativeTls(native)
}
