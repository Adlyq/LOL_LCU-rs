//! 会议话状态管理

use parking_lot::Mutex;
use std::sync::Arc;

/// 全局共享会话状态
pub type SharedState = Arc<Mutex<RuntimeState>>;

/// 在内存中维护的实时状态
pub struct RuntimeState {
    /// 当前板凳席上的英雄 ID 列表 (用于抢人匹配)
    pub current_bench_ids: Vec<i64>,
    /// 选人阶段组队分析是否已完成 (每个会话仅分析一次)
    pub premade_analysis_done: bool,
    /// 游戏内组队分析是否已完成
    pub premade_ingame_done: bool,

    /// 当前正在尝试抢占的板凳席索引
    pub active_pick_slot: Option<usize>,
}

impl RuntimeState {
    pub fn new() -> Self {
        Self {
            current_bench_ids: Vec::new(),
            premade_analysis_done: false,
            premade_ingame_done: false,
            active_pick_slot: None,
        }
    }

    /// 重置预组队分析状态（通常在阶段切换到 None 时调用）
    pub fn reset_premade_status(&mut self) {
        self.premade_analysis_done = false;
        self.premade_ingame_done = false;
    }

    /// 取消当前的抢人任务
    pub fn cancel_pick_task(&mut self) {
        self.active_pick_slot = None;
    }
}

pub fn new_shared_state() -> SharedState {
    Arc::new(Mutex::new(RuntimeState::new()))
}
