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

    /// 当前板凳席英雄 ID 列表（用于点击索引匹配）
    pub current_bench_ids: Vec<i64>,
    /// 当前正在尝试抢英雄的槽位索引
    pub active_pick_slot: Option<usize>,
    /// 抢英雄任务代次（用于取消旧任务）
    pub pick_generation: u64,
    /// 抢英雄异步任务句柄
    pub pick_task: Option<tokio::task::JoinHandle<()>>,
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
            current_bench_ids: Vec::new(),
            active_pick_slot: None,
            pick_generation: 0,
            pick_task: None,
        }
    }

    /// 重连时调用：重置会话级别的标记
    pub fn reset_session(&mut self) {
        self.reset_premade_status();
        self.cancel_ready_check();
        self.current_bench_ids.clear();
        self.cancel_pick_task();
    }

    /// 重置 Ready Check 状态
    pub fn cancel_ready_check(&mut self) {
        self.ready_check_pending_accept = false;
        self.ready_check_generation += 1;
    }

    /// 开始 Ready Check（获取新代次）
    pub fn start_ready_check(&mut self) -> u64 {
        self.ready_check_pending_accept = true;
        self.ready_check_generation
    }

    /// 重置组黑分析状态（通常在阶段切换非游戏阶段时调用）
    pub fn reset_premade_status(&mut self) {
        self.premade_analysis_done = false;
        self.premade_ingame_done = false;
    }

    /// 取消当前抢人任务
    pub fn cancel_pick_task(&mut self) {
        self.active_pick_slot = None;
        self.pick_generation += 1;
        if let Some(task) = self.pick_task.take() {
            task.abort();
        }
    }
}

pub fn new_shared_state() -> SharedState {
    Arc::new(Mutex::new(RuntimeState::new()))
}
