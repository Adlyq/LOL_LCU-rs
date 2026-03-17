//! 运行时状态（对应 Python `RuntimeState` dataclass）
//!
//! 以 `Arc<Mutex<RuntimeState>>` 的形式在各异步任务间共享。

use std::sync::Arc;
use parking_lot::Mutex;
use tokio::task::JoinHandle;

/// 运行时共享状态。
#[derive(Debug)]
pub struct RuntimeState {
    // ── ReadyCheck ──────────────────────────────────────
    /// 是否已触发自动接受等待
    pub ready_check_pending_accept: bool,
    /// 用于检测 ReadyCheck 是否已过期的代次标记
    pub ready_check_generation: u64,

    // ── 英雄选择 ────────────────────────────────────────
    /// 当前 bench 英雄 ID 列表（按显示顺序）
    pub current_bench_ids: Vec<i64>,
    /// 上次 bench 快照（用于去重日志输出）
    pub last_bench_key: Option<Vec<i64>>,
    /// 当前被点击（正在循环 swap）的槽位索引
    pub active_pick_slot: Option<usize>,
    /// pick 任务代次（用于取消旧任务）
    pub pick_generation: u64,
    /// 当前 pick 循环任务的句柄（需通过 Option 持有）
    pub pick_task: Option<JoinHandle<()>>,

    // ── 点赞 ────────────────────────────────────────────
    /// 上次跳过点赞的 game_id（避免重复跳过）
    pub last_skipped_honor_game_id: Option<i64>,
    /// 上次跳过点赞的单调时间（用于无 game_id 的冷却逻辑）
    pub last_honor_skip_ts: std::time::Instant,
    /// 上次已尝试点击"继续"按钮的 game_id
    pub last_post_honor_continue_game_id: Option<i64>,

    // ── 组黑分析 ─────────────────────────────────────────
    /// 本局是否已发送过组黑分析（去重）
    pub premade_analysis_done: bool,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeState {
    pub fn new() -> Self {
        Self {
            last_bench_key: None,
            ready_check_pending_accept: false,
            ready_check_generation: 0,
            current_bench_ids: Vec::new(),
            active_pick_slot: None,
            pick_generation: 0,
            pick_task: None,
            last_skipped_honor_game_id: None,
            last_honor_skip_ts: std::time::Instant::now(),
            last_post_honor_continue_game_id: None,
            premade_analysis_done: false,
        }
    }

    /// 重置断线/重连时需要清理的字段。
    ///
    /// 对应 Python `main()` 中每次重连都会新建 `RuntimeState()`，
    /// 以及 `finally` 块中手动清理 bench/pick 状态的逻辑。
    ///
    /// 保留跨连接有效的字段（点赞冷却、game_id 去重），
    /// 清除与当前会话绑定的字段。
    pub fn reset_session(&mut self) {
        // 中止旧 pick 任务
        if let Some(task) = self.pick_task.take() {
            task.abort();
        }
        self.current_bench_ids.clear();
        self.last_bench_key = None;
        self.active_pick_slot = None;
        self.pick_generation += 1;
        self.ready_check_pending_accept = false;
        self.ready_check_generation += 1;
        self.premade_analysis_done = false;
        // last_skipped_honor_game_id / last_honor_skip_ts / last_post_honor_continue_game_id
        // 刻意保留，避免重连后对同一局重复执行
    }
}

/// 共享状态句柄（廉价 clone）。
pub type SharedState = Arc<Mutex<RuntimeState>>;

pub fn new_shared_state() -> SharedState {
    Arc::new(Mutex::new(RuntimeState::new()))
}
