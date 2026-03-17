//! 日志系统初始化
//!
//! Debug 构建：同时输出到控制台和滚动文件（lol_lcu.log）。
//! Release 构建：仅输出到控制台，不生成日志文件。

use std::path::PathBuf;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// 初始化 tracing 日志。
///
/// - 控制台：根据 `RUST_LOG` 环境变量（默认 `info`）
/// - 文件：Debug 模式下写入 `lol_lcu.log`（按日期滚动），Release 模式下禁用
pub fn init_logging(log_dir: Option<PathBuf>) {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_level(true);

    #[cfg(debug_assertions)]
    {
        let log_dir = log_dir.unwrap_or_else(|| PathBuf::from("."));
        let file_appender = tracing_appender::rolling::daily(log_dir, "lol_lcu.log");
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

        let file_layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_level(true)
            .with_ansi(false)
            .with_writer(non_blocking);

        tracing_subscriber::registry()
            .with(env_filter)
            .with(console_layer)
            .with(file_layer)
            .init();

        // _guard 必须保持存活直到进程退出
        Box::leak(Box::new(_guard));
    }

    #[cfg(not(debug_assertions))]
    {
        let _ = log_dir; // release 模式不使用 log_dir
        tracing_subscriber::registry()
            .with(env_filter)
            .with(console_layer)
            .init();
    }
}
