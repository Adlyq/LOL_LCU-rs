//! 用户可配置的运行时选项。
//!
//! 通过信息面板 UI 修改，各 handler 按需读取。
//! 配置文件存储在 %APPDATA%\lol-lcu\config.json。

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::info;

/// 可持久化的用户配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// 自动接受对局（ReadyCheck）
    pub auto_accept_enabled: bool,
    /// 自动接受延迟（秒，0–15）
    pub auto_accept_delay_secs: u64,
    /// 自动跳过点赞投票
    pub auto_honor_skip: bool,
    /// 选人阶段组黑分析
    pub premade_champ_select: bool,
    /// 游戏中组黑分析（含英雄名）
    pub premade_ingame: bool,
    /// 内存超限自动热重载
    pub memory_monitor: bool,
    /// 热重载内存阈值（MB，500–4000）
    pub memory_threshold_mb: u64,
    /// 窗口透明度（30–100%）
    pub opacity: u8,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            auto_accept_enabled: true,
            auto_accept_delay_secs: 5,
            auto_honor_skip: true,
            premade_champ_select: true,
            premade_ingame: true,
            memory_monitor: true,
            memory_threshold_mb: 1500,
            opacity: 95,
        }
    }
}

impl AppConfig {
    fn config_path() -> PathBuf {
        let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(appdata).join("lol-lcu").join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(data) => {
                info!("已加载配置: {}", path.display());
                serde_json::from_str(&data).unwrap_or_default()
            }
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, data);
        }
    }
}

/// 共享配置句柄（廉价 clone）。
pub type SharedConfig = Arc<Mutex<AppConfig>>;

pub fn new_shared_config() -> SharedConfig {
    Arc::new(Mutex::new(AppConfig::load()))
}
