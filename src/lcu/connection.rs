//! LCU 连接信息读取
//!
//! 通过扫描系统进程列表找到 `LeagueClient.exe`，
//! 从其启动参数中提取 `--app-port` 和 `--remoting-auth-token`。
//!
//! 对应 Python willump 中的 `find_LCU_process()` + `parse_cmdline_args()`。

use std::time::Duration;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use reqwest::{Client, ClientBuilder};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tokio::time::sleep;
use tracing::{info, warn};

/// 尝试匹配的 LCU 进程名（全部小写比较）
const LCU_PROCESS_NAMES: &[&str] = &[
    "leagueclient.exe",
    "leagueclientux.exe",
];

/// 连接凭据（端口号 + 认证信息）。
#[derive(Debug, Clone)]
pub struct LcuCredentials {
    pub port: u16,
    #[allow(dead_code)]
    pub auth_token: String,
    /// 预计算的 Basic Auth header 值 (`Basic <base64>`)
    pub auth_header: String,
}

impl LcuCredentials {
    /// 从进程命令行参数中解析凭据。
    ///
    /// 参数格式：`--app-port=12345` `--remoting-auth-token=abcdef`
    fn from_cmdline(args: &[impl AsRef<str>]) -> Option<Self> {
        let mut port: Option<u16> = None;
        let mut auth_token: Option<String> = None;

        for arg in args {
            let s = arg.as_ref();
            if let Some(v) = s.strip_prefix("--app-port=") {
                port = v.parse().ok();
            } else if let Some(v) = s.strip_prefix("--remoting-auth-token=") {
                auth_token = Some(v.to_owned());
            }
        }

        let port = port?;
        let auth_token = auth_token?;
        let raw = format!("riot:{auth_token}");
        let auth_header = format!("Basic {}", B64.encode(raw.as_bytes()));
        Some(Self { port, auth_token, auth_header })
    }
}

/// 扫描进程列表，尝试找到 LCU 进程并提取凭据。
///
/// 如果进程不存在或参数不完整，返回 `None`（由调用方负责重试）。
pub fn find_lcu_credentials() -> Option<LcuCredentials> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        false,
        ProcessRefreshKind::new().with_cmd(UpdateKind::Always),
    );

    for (_, process) in sys.processes() {
        let name = process.name().to_string_lossy().to_lowercase();
        if LCU_PROCESS_NAMES.iter().any(|&n| name == n) {
            let cmd: Vec<String> = process
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            if let Some(creds) = LcuCredentials::from_cmdline(&cmd) {
                info!(
                    "找到 LCU 进程: {} (port={})",
                    process.name().to_string_lossy(),
                    creds.port
                );
                return Some(creds);
            }
        }
    }
    None
}

/// 轮询等待 LCU 进程出现，每 500ms 重试一次。
///
/// 对应 Python willump 的初始化循环：
/// ```python
/// while not lcu_process:
///     lcu_process = find_LCU_process()
///     await asyncio.sleep(0.5)
/// ```
pub async fn wait_for_credentials() -> LcuCredentials {
    loop {
        match find_lcu_credentials() {
            Some(creds) => return creds,
            None => {
                warn!("未找到 LCU 进程，500ms 后重新扫描进程列表...");
                sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// 构建信任 LCU 自签名证书的 `reqwest::Client`。
///
/// 使用 native-tls（Windows SChannel）+ `danger_accept_invalid_certs`
/// 绕过证书校验（与 Python willump 行为一致；内网 127.0.0.1 无 MITM 风险）。
pub fn build_client(creds: &LcuCredentials) -> Result<Client> {
    let client = ClientBuilder::new()
        .use_native_tls()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .timeout(Duration::from_secs(10))
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                creds.auth_header.parse().context("auth header 解析失败")?,
            );
            headers.insert(
                reqwest::header::CONTENT_TYPE,
                "application/json".parse().unwrap(),
            );
            headers.insert(
                reqwest::header::ACCEPT,
                "application/json".parse().unwrap(),
            );
            headers
        })
        .build()
        .context("构建 reqwest client 失败")?;
    Ok(client)
}