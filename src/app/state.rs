//! 会议话状态管理

use std::sync::Arc;
use std::time::Instant;
use parking_lot::Mutex;

/// 全局共享会话状态
pub type SharedState = Arc<Mutex<RuntimeState>>;

/// 在内存中维护的实时状态
pub struct RuntimeState {
    /// Ready Check 自动接受代次（避免旧事件触发）
    pub ready_check_generation: u64,
    /// 是否正在等待 Ready Check 延迟接受
    pub ready_check_pending_accept: bool,

    /// 选人阶段组黑分析是否已完成（每局仅一次）
    pub premade_analysis_done: bool,
    /// 游戏中组黑分析是否已完成（每局仅一次）
    pub premade_ingame_done: bool,

    /// 上一次自动跳过点赞的游戏 ID
    pub last_skipped_honor_game_id: Option<i64>,
    /// 上一次自动跳过点赞的时间戳（用于 fallback）
    pub last_honor_skip_ts: Instant,
    /// 上一次点击点赞页面"继续"按钮的游戏 ID
    pub last_post_honor_continue_game_id: Option<i64>,
    /// 英雄 ID -> 名称 映射表缓存
    pub champion_id_name_map: std::collections::HashMap<i64, String>,
}

impl RuntimeState {
    pub fn new() -> Self {
        Self {
            ready_check_generation: 0,
            ready_check_pending_accept: false,
            premade_analysis_done: false,
            premade_ingame_done: false,
            last_skipped_honor_game_id: None,
            last_honor_skip_ts: Instant::now(),
            last_post_honor_continue_game_id: None,
            champion_id_name_map: std::collections::HashMap::new(),
        }
    }

    /// 重连时调用：重置会话级别的标记
    pub fn reset_session(&mut self) {
        self.premade_analysis_done = false;
        self.premade_ingame_done = false;
        // 点赞相关通常跟随进程，重连时不一定重置，但重置也安全
    }
}

pub fn new_shared_state() -> SharedState {
    Arc::new(Mutex::new(RuntimeState::new()))
}
