//! LCU HTTP API 封装入口
//!
//! 采用模块化设计，将不同领域的 API 拆分到子模块中。
//! 
//! 设计原则：
//! - 每个方法仅做单一 HTTP 调用 + 结果反序列化；
//! - 复杂业务逻辑（retry、状态判断）放在 `app` 层；
//! - 错误统一包装为 `LcuApiError`。

#![allow(dead_code)]

use anyhow::Result;
use reqwest::{Client, Response};
use serde_json::{Value};
use thiserror::Error;
use tracing::{debug};
use std::sync::Arc;
use tokio::sync::Semaphore;

use super::connection::LcuCredentials;

pub mod gameflow;
pub mod lobby;
pub mod summoner;
pub mod champ_select;
pub mod match_history;
pub mod honor;
pub mod chat;
pub mod loot;

/// LCU API 调用失败时的错误类型。
#[derive(Debug, Error)]
pub enum LcuApiError {
    #[error("HTTP {status} {method} {endpoint}: {body}")]
    Http {
        status: u16,
        method: String,
        endpoint: String,
        body: String,
    },
    #[error("网络错误: {0}")]
    Network(#[from] reqwest::Error),
    #[error("JSON 解析错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

/// LCU HTTP API 客户端。
///
/// `Clone` 开销极小（reqwest::Client 内部是 Arc）。
#[derive(Clone, Debug)]
pub struct LcuClient {
    pub(crate) client: Client,
    pub(crate) base_url: String,
    /// 资产请求并发限制器 (借鉴 LeagueAkari 的 PQueue 方案)
    pub(crate) asset_semaphore: Arc<Semaphore>,
}

impl LcuClient {
    /// 根据凭据构建客户端。
    pub fn new(creds: &LcuCredentials, http_client: Client) -> Self {
        Self {
            client: http_client,
            base_url: format!("https://127.0.0.1:{}", creds.port),
            // 限制资产并发请求为 8
            asset_semaphore: Arc::new(Semaphore::new(8)),
        }
    }

    pub(crate) fn url(&self, endpoint: &str) -> String {
        format!("{}{}", self.base_url, endpoint)
    }

    // ── 底层 HTTP 方法 ─────────────────────────────────────────────

    pub(crate) async fn raw_get(&self, endpoint: &str) -> Result<Response, LcuApiError> {
        let resp = self.client.get(self.url(endpoint)).send().await?;
        Self::check_status(resp, "GET", endpoint).await
    }

    pub(crate) async fn raw_post(&self, endpoint: &str, body: Option<Value>) -> Result<Response, LcuApiError> {
        let req = self.client.post(self.url(endpoint));
        let req = match body {
            Some(v) => req.json(&v),
            None => req,
        };
        let resp = req.send().await?;
        Self::check_status(resp, "POST", endpoint).await
    }

    pub(crate) async fn raw_patch(&self, endpoint: &str, body: Value) -> Result<Response, LcuApiError> {
        let resp = self
            .client
            .patch(self.url(endpoint))
            .json(&body)
            .send()
            .await?;
        Self::check_status(resp, "PATCH", endpoint).await
    }

    pub(crate) async fn raw_delete(&self, endpoint: &str) -> Result<Response, LcuApiError> {
        let resp = self.client.delete(self.url(endpoint)).send().await?;
        Self::check_status(resp, "DELETE", endpoint).await
    }

    pub(crate) async fn check_status(
        resp: Response,
        method: &str,
        endpoint: &str,
    ) -> Result<Response, LcuApiError> {
        if resp.status().as_u16() >= 400 {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LcuApiError::Http {
                status,
                method: method.to_owned(),
                endpoint: endpoint.to_owned(),
                body,
            });
        }
        Ok(resp)
    }

    /// 解析响应体为 JSON；空体返回 `Value::Null`。
    pub(crate) async fn json_or_null(resp: Response) -> Result<Value, LcuApiError> {
        let text = resp.text().await?;
        if text.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text)?)
    }

    pub async fn get_json(&self, endpoint: &str) -> Result<Value, LcuApiError> {
        debug!("GET {endpoint}");
        
        // 如果是资产请求，则应用限流 (lol-game-data/assets)
        if endpoint.contains("lol-game-data/assets") {
            let _permit = self.asset_semaphore.acquire().await.map_err(|e| LcuApiError::Other(e.to_string()))?;
            let resp = self.raw_get(endpoint).await?;
            Self::json_or_null(resp).await
        } else {
            let resp = self.raw_get(endpoint).await?;
            Self::json_or_null(resp).await
        }
    }

    pub async fn post_json(&self, endpoint: &str, body: Option<Value>) -> Result<Value, LcuApiError> {
        debug!("POST {endpoint}");
        let resp = self.raw_post(endpoint, body).await?;
        Self::json_or_null(resp).await
    }

    pub async fn patch_json(&self, endpoint: &str, body: Value) -> Result<Value, LcuApiError> {
        debug!("PATCH {endpoint}");
        let resp = self.raw_patch(endpoint, body).await?;
        Self::json_or_null(resp).await
    }

    pub async fn delete_json(&self, endpoint: &str) -> Result<Value, LcuApiError> {
        debug!("DELETE {endpoint}");
        let resp = self.raw_delete(endpoint).await?;
        Self::json_or_null(resp).await
    }
}
