//! 日志系统初始化
//!
//! 统一行为：仅输出到控制台，不生成日志文件。

use std::path::PathBuf;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// 初始化 tracing 日志。
///
/// - 仅控制台输出：根据 `RUST_LOG` 环境变量（默认 `info`）
pub fn init_logging(_log_dir: Option<PathBuf>) {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_ansi(true);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .init();
}
