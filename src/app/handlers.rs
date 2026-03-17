//! 游戏事件处理器
//!
//! 对应 Python 侧 `app/game_handlers.py` 的各函数：
//! - `handle_ready_check`
//! - `handle_gameflow`
//! - `handle_honor_ballot`
//! - `handle_champ_select`
//! - `handle_overlay_click`
//! - `window_fix_loop`
//!
//! 设计：
//! - 所有函数接受 `SharedState`（`Arc<Mutex<RuntimeState>>`）、`LcuClient`、
//!   以及 `OverlaySender`（跨线程通知 overlay 线程的通道）。
//! - Overlay UI 运行在 Win32 消息循环线程，通过 `mpsc` channel 接收指令。

use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::lcu::api::{gameflow, LcuClient};
use crate::win::overlay::OverlayCmd;
use crate::app::premade::{analyze_premade, extract_teams_from_session, format_premade_message};

use super::state::SharedState;

// ── 常量（对应 Python 侧同名变量）──────────────────────────────────

const READY_CHECK_ACCEPT_DELAY_SECS: u64 = 5;
const HONOR_SKIP_FALLBACK_COOLDOWN_SECS: u64 = 30;
const POSTGAME_CONTINUE_DELAY_SECS: f64 = 0.8;
const WINDOW_FIX_POLL_SECS: f64 = 1.5;

// ── 工具函数 ──────────────────────────────────────────────────────

/// 从事件 payload 中提取 `data` 对象。
fn event_data(event: &Value) -> Option<&Value> {
    let data = event.get("data")?;
    if data.is_object() { Some(data) } else { None }
}

// ── handle_ready_check ───────────────────────────────────────────

/// 处理 ReadyCheck WebSocket 事件。
///
/// 逻辑与 Python 完全一致：
/// 1. 若 state != InProgress 或 playerResponse 已设置，重置待接受标志；
/// 2. 延迟 5 秒后重新获取 ready-check 状态，再接受；
/// 3. 代次机制确保旧事件不会影响新局。
pub async fn handle_ready_check(
    api: LcuClient,
    state: SharedState,
    event: Value,
) {
    let data = match event_data(&event) {
        Some(d) => d.clone(),
        None => return,
    };

    let rc_state = data.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let player_response = data.get("playerResponse").and_then(|v| v.as_str());
    let event_id = data.get("id").and_then(|v| v.as_i64());

    let response_set = player_response.map(|r| r != "None").unwrap_or(false);

    if rc_state != "InProgress" || response_set {
        let mut s = state.lock();
        s.ready_check_pending_accept = false;
        s.ready_check_generation += 1;
        return;
    }

    let already_pending = {
        let s = state.lock();
        s.ready_check_pending_accept
    };
    if already_pending {
        return;
    }

    let generation = {
        let mut s = state.lock();
        s.ready_check_pending_accept = true;
        s.ready_check_generation
    };

    info!("检测到 Ready Check，{READY_CHECK_ACCEPT_DELAY_SECS} 秒后自动接受（如需拒绝请手动操作）...");
    sleep(Duration::from_secs(READY_CHECK_ACCEPT_DELAY_SECS)).await;

    // 检查状态是否已被取消
    {
        let s = state.lock();
        if !s.ready_check_pending_accept || s.ready_check_generation != generation {
            return;
        }
    }

    // 重新获取 ready-check 以确认仍处于 InProgress
    let current = match api.get_ready_check().await {
        Ok(v) => v,
        Err(e) => {
            warn!("获取 ready-check 状态失败: {e}");
            state.lock().ready_check_pending_accept = false;
            return;
        }
    };

    let cur_state = current.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let cur_response = current.get("playerResponse").and_then(|v| v.as_str());
    let cur_id = current.get("id").and_then(|v| v.as_i64());

    let cur_response_set = cur_response.map(|r| r != "None").unwrap_or(false);

    if cur_state != "InProgress" || cur_response_set {
        let mut s = state.lock();
        s.ready_check_pending_accept = false;
        s.ready_check_generation += 1;
        return;
    }

    // ID 不匹配说明已是新的 ready-check
    if let (Some(ev_id), Some(c_id)) = (event_id, cur_id) {
        if ev_id != c_id {
            let mut s = state.lock();
            s.ready_check_pending_accept = false;
            s.ready_check_generation += 1;
            return;
        }
    }

    let accepted = match api.accept_ready_check().await {
        Ok(_) => true,
        Err(e) => {
            warn!("接受 ready-check 失败: {e}");
            false
        }
    };

    {
        let mut s = state.lock();
        s.ready_check_pending_accept = false;
        s.ready_check_generation += 1;
    }

    if accepted {
        info!("Ready Check 已自动接受");
    }
}

// ── handle_gameflow ──────────────────────────────────────────────

/// 处理 Gameflow 阶段变化事件，通知 overlay 显示/隐藏。
pub async fn handle_gameflow(
    _api: LcuClient,
    state: SharedState,
    overlay_tx: mpsc::Sender<OverlayCmd>,
    event: Value,
) {
    let phase = event
        .get("data")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());

    set_overlay_visibility_by_phase(&state, &overlay_tx, phase.as_deref()).await;
}

// ── set_overlay_visibility_by_phase ─────────────────────────────

pub async fn set_overlay_visibility_by_phase(
    state: &SharedState,
    overlay_tx: &mpsc::Sender<OverlayCmd>,
    phase: Option<&str>,
) {
    if phase == Some(gameflow::CHAMP_SELECT) {
        let _ = overlay_tx.send(OverlayCmd::Show).await;
    } else {
        // --show-overlay（仅 debug）：跳过 Hide，只清理状态
        #[cfg(debug_assertions)]
        if crate::DEBUG_SHOW_OVERLAY.load(std::sync::atomic::Ordering::Relaxed) {
            reset_champ_select_state_keep_visible(state, overlay_tx).await;
            return;
        }
        reset_champ_select_state(state, overlay_tx).await;
    }
}

/// 仅 debug 构建 / --show-overlay 时使用：清理状态但不隐藏 overlay。
#[cfg(debug_assertions)]
async fn reset_champ_select_state_keep_visible(
    state: &SharedState,
    overlay_tx: &mpsc::Sender<OverlayCmd>,
) {
    let had_bench = {
        let mut s = state.lock();
        let had = !s.current_bench_ids.is_empty();
        if had { s.current_bench_ids.clear(); }
        s.last_bench_key = None;
        reset_pick_state_locked(&mut s);
        had
    };
    if had_bench {
        let _ = overlay_tx.send(OverlayCmd::SetBenchIds(vec![])).await;
    }
}

async fn reset_champ_select_state(
    state: &SharedState,
    overlay_tx: &mpsc::Sender<OverlayCmd>,
) {
    let _ = overlay_tx.send(OverlayCmd::Hide).await;

    // 在 lock 作用域内取值，然后 drop guard，再做 await
    let had_bench = {
        let mut s = state.lock();
        let had = !s.current_bench_ids.is_empty();
        if had {
            s.current_bench_ids.clear();
        }
        s.last_bench_key = None;
        s.premade_analysis_done = false;
        reset_pick_state_locked(&mut s);
        had
    };

    if had_bench {
        let _ = overlay_tx.send(OverlayCmd::SetBenchIds(vec![])).await;
    }
}

fn reset_pick_state_locked(s: &mut super::state::RuntimeState) {
    s.pick_generation += 1;
    s.active_pick_slot = None;
    if let Some(task) = s.pick_task.take() {
        task.abort();
    }
}

// ── handle_honor_ballot ──────────────────────────────────────────

/// 处理点赞投票事件，自动跳过，对应 Python `handle_honor_ballot`。
pub async fn handle_honor_ballot(
    api: LcuClient,
    state: SharedState,
    overlay_tx: mpsc::Sender<OverlayCmd>,
    event: Value,
) {
    let data = event_data(&event);
    let game_id: Option<i64> = data
        .and_then(|d| d.get("gameId"))
        .and_then(|v| v.as_i64());
    let eligible_raw = data.and_then(|d| d.get("eligiblePlayers")).cloned();

    debug!(
        "[honor] 收到事件 game_id={:?} eligiblePlayers={}",
        game_id,
        match &eligible_raw {
            None => "null".to_owned(),
            Some(v) if v.is_null() => "null".to_owned(),
            Some(v) => v.to_string(),
        }
    );

    // 同局已跳过，不再重复
    if let Some(gid) = game_id {
        let s = state.lock();
        if s.last_skipped_honor_game_id == Some(gid) {
            debug!("[honor] 跳过：game_id={gid} 本局已处理过，不重复");
            return;
        }
    }

    // 无 game_id 时的冷却检测
    let now = Instant::now();
    if game_id.is_none() {
        let s = state.lock();
        let elapsed = now.duration_since(s.last_honor_skip_ts);
        if elapsed < Duration::from_secs(HONOR_SKIP_FALLBACK_COOLDOWN_SECS) {
            debug!(
                "[honor] 跳过：无 game_id，冷却中（已过 {:.1}s / {}s）",
                elapsed.as_secs_f64(),
                HONOR_SKIP_FALLBACK_COOLDOWN_SECS
            );
            return;
        }
    }

    // 检查事件是否有效（对应 Python auto_skip_honor_if_needed 的前置判断）
    // Python: game_id is None and eligible_players in (None, [])
    if let Some(data) = data {
        let raw_game_id = data.get("gameId");
        let eligible = data.get("eligiblePlayers");
        // eligible_players in (None, [])：字段不存在 或 是空数组
        let eligible_none_or_empty = eligible.is_none()
            || eligible
                .and_then(|v| v.as_array())
                .map(|a| a.is_empty())
                .unwrap_or(false); // 字段存在但不是数组 → 视为有值，不跳过
        if raw_game_id.is_none() && eligible_none_or_empty {
            debug!("[honor] 跳过：gameId=null 且 eligiblePlayers 为空/null，事件无效（游戏未开始）");
            return;
        }
        debug!(
            "[honor] 通过前置校验：gameId={:?} eligiblePlayers_none_or_empty={}",
            raw_game_id, eligible_none_or_empty
        );
    }

    debug!("[honor] 触发 skip_honor_vote");
    let skipped = match api.skip_honor_vote().await {
        Ok(v) => v,
        Err(e) => {
            warn!("skip_honor_vote 失败: {e}");
            false
        }
    };

    if skipped {
        {
            let mut s = state.lock();
            if let Some(gid) = game_id {
                s.last_skipped_honor_game_id = Some(gid);
            }
            s.last_honor_skip_ts = now;
        }
        info!("已自动跳过点赞");
        try_advance_post_honor_screen(api, state, overlay_tx, game_id).await;
    } else {
        debug!("[honor] skip_honor_vote 返回 false（接口未成功，当前可能不在点赞页面）");
    }
}

/// 点赞跳过后尝试点击"继续"按钮（对应 Python `_try_advance_post_honor_screen`）。
async fn try_advance_post_honor_screen(
    api: LcuClient,
    state: SharedState,
    _overlay_tx: mpsc::Sender<OverlayCmd>,
    game_id: Option<i64>,
) {
    if let Some(gid) = game_id {
        let already = state.lock().last_post_honor_continue_game_id == Some(gid);
        if already {
            return;
        }
    }

    sleep(Duration::from_secs_f64(POSTGAME_CONTINUE_DELAY_SECS)).await;

    let phase = match api.get_gameflow_phase().await {
        Ok(p) => p,
        Err(e) => {
            warn!("获取 gameflow phase 失败: {e}");
            return;
        }
    };

    if phase != gameflow::END_OF_GAME {
        return;
    }

    let clicked = crate::win::winapi::click_postgame_continue(None);
    if !clicked {
        return;
    }

    if let Some(gid) = game_id {
        state.lock().last_post_honor_continue_game_id = Some(gid);
    }
    info!("已尝试点击点赞后的第一层页面继续按钮");
}

// ── handle_champ_select ──────────────────────────────────────────

/// 处理英雄选择事件，更新 bench 列表并通知 overlay。
pub async fn handle_champ_select(
    api: LcuClient,
    state: SharedState,
    overlay_tx: mpsc::Sender<OverlayCmd>,
    event: Value,
) {
    if event_data(&event).is_none() {
        // eventType = Delete：选人结束，重置状态
        reset_champ_select_state(&state, &overlay_tx).await;
        return;
    }

    // WS payload 的 data 字段即为完整 session，无需额外 HTTP GET
    let session = event_data(&event).unwrap().clone();

    // ── 组黑分析（每局只触发一次）──────────────────────────────────
    let should_analyze = {
        let s = state.lock();
        !s.premade_analysis_done
    };
    if should_analyze {
        state.lock().premade_analysis_done = true;
        let api2 = api.clone();
        let session2 = session.clone();
        tokio::spawn(async move {
            let (my_team, their_team, my_side, their_side) = extract_teams_from_session(&session2);
            if my_team.is_empty() && their_team.is_empty() {
                return;
            }
            info!("开始组黑分析（我方{}人 / 对方{}人）...", my_team.len(), their_team.len());
            let (my_result, their_result) = analyze_premade(&api2, my_team, their_team, 3, 20).await;
            let msg = format_premade_message(&my_result, &their_result, my_side, their_side);
            info!("{msg}");
            match api2.send_message_to_self(&msg).await {
                Ok(()) => info!("组黑分析已私信发送给自己"),
                Err(e) => warn!("组黑分析私信发送失败: {e}"),
            }
        });
    }

    // 非大乱斗模式（benchEnabled = false），重置状态
    if !session
        .get("benchEnabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        reset_champ_select_state(&state, &overlay_tx).await;
        return;
    }

    let bench_ids = LcuClient::extract_bench_champion_ids(&session);

    // 更新 bench 状态
    {
        let mut s = state.lock();
        s.current_bench_ids = bench_ids.clone();
    }
    let _ = overlay_tx.send(OverlayCmd::SetBenchIds(bench_ids.clone())).await;

    // 去重：bench 未变化时不重复打印
    let bench_key = bench_ids.clone();
    {
        let mut s = state.lock();
        if s.last_bench_key.as_ref() == Some(&bench_key) {
            return;
        }
        s.last_bench_key = Some(bench_key);
    }

    // 获取英雄名称并打印
    let id_name_map = api.get_champion_id_name_map().await.unwrap_or_default();
    info!("\n=== 大乱斗可换英雄列表 ===");
    info!("上方可换英雄({}):", bench_ids.len());
    for cid in &bench_ids {
        let name = id_name_map
            .get(cid)
            .cloned()
            .unwrap_or_else(|| format!("Champion-{cid}"));
        info!(" - {cid} - {name}");
    }
    info!("====================");
}

// ── handle_overlay_click ─────────────────────────────────────────

/// 处理 overlay 槽位点击事件。
///
/// 逻辑与 Python `_handle_overlay_click` 完全一致：
/// - 再次点击同槽位 → 取消当前 swap 任务；
/// - 点击新槽位 → 取消旧任务，启动新 swap 循环。
pub async fn handle_overlay_click(
    api: LcuClient,
    state: SharedState,
    overlay_tx: mpsc::Sender<OverlayCmd>,
    slot_index: usize,
) {
    // ── 判断并更新状态（在 lock 内完成，不跨 await）──────────────────
    enum Action {
        CancelSameSlot,
        StartNew { champion_id: i64, generation: u64 },
        OutOfRange,
    }

    let action = {
        let mut s = state.lock();

        if slot_index >= s.current_bench_ids.len() {
            Action::OutOfRange
        } else if s.active_pick_slot == Some(slot_index) {
            // 再次点击同槽位 → 取消
            if let Some(task) = s.pick_task.take() {
                if !task.is_finished() {
                    task.abort();
                }
            }
            s.pick_generation += 1;
            s.active_pick_slot = None;
            Action::CancelSameSlot
        } else {
            let champion_id = s.current_bench_ids[slot_index];
            s.pick_generation += 1;
            if let Some(task) = s.pick_task.take() {
                task.abort();
            }
            s.active_pick_slot = Some(slot_index);
            let gen = s.pick_generation;
            Action::StartNew { champion_id, generation: gen }
        }
        // guard 在此作用域结束时自动 drop，不跨 await
    };

    match action {
        Action::OutOfRange => {}
        Action::CancelSameSlot => {
            let _ = overlay_tx.send(OverlayCmd::ClearSelectedSlot).await;
        }
        Action::StartNew { champion_id, generation } => {
            // 先高亮选中槽位（对应 Python mousePressEvent 里 self.selected_slot = i）
            let _ = overlay_tx.send(OverlayCmd::SetSelectedSlot(slot_index)).await;

            let api2 = api.clone();
            let state2 = state.clone();
            let overlay_tx2 = overlay_tx.clone();

            let handle = tokio::spawn(async move {
                loop_pick_until_refresh(api2, state2, overlay_tx2, champion_id, generation, slot_index)
                    .await;
            });

            state.lock().pick_task = Some(handle);
        }
    }
}

/// Swap 循环（对应 Python `_loop_pick_until_refresh`）。
async fn loop_pick_until_refresh(
    api: LcuClient,
    state: SharedState,
    overlay_tx: mpsc::Sender<OverlayCmd>,
    champion_id: i64,
    generation: u64,
    _slot_index: usize,
) {
    // 记录初始 pickable 列表，用于检测刷新
    let initial_pickable: Vec<i64> = api
        .get_pickable_champion_ids()
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();
    let mut initial_pickable_sorted = initial_pickable.clone();
    initial_pickable_sorted.sort_unstable();

    loop {
        // 检查代次，若已被取消则退出
        if state.lock().pick_generation != generation {
            return;
        }

        // 尝试 swap
        if let Err(e) = api.swap_bench_champion(champion_id).await {
            // swap 失败不中断，继续重试
            let _ = e;
        }

        sleep(Duration::from_millis(300)).await;

        // 检查是否已在本地玩家手上
        let session = api.get_champ_select_session().await.unwrap_or(Value::Null);
        let local_player = LcuClient::get_local_player(&session);
        if let Some(player) = local_player {
            let local_champ = player.get("championId").and_then(|v| v.as_i64());
            if local_champ == Some(champion_id) {
                let _ = overlay_tx.send(OverlayCmd::ClearSelectedSlot).await;
                let mut s = state.lock();
                if s.pick_generation == generation {
                    s.active_pick_slot = None;
                }
                return;
            }
        }

        // 检查英雄是否还在 bench
        let bench = LcuClient::extract_bench_champion_ids(&session);
        if !bench.contains(&champion_id) {
            let _ = overlay_tx.send(OverlayCmd::ClearSelectedSlot).await;
            let mut s = state.lock();
            if s.pick_generation == generation {
                s.active_pick_slot = None;
            }
            return;
        }

        // 检查 pickable 列表是否已刷新
        let mut current_pickable: Vec<i64> = api
            .get_pickable_champion_ids()
            .await
            .unwrap_or_default();
        current_pickable.sort_unstable();
        if current_pickable != initial_pickable_sorted {
            let _ = overlay_tx.send(OverlayCmd::ClearSelectedSlot).await;
            let mut s = state.lock();
            if s.pick_generation == generation {
                s.active_pick_slot = None;
            }
            return;
        }
    }
}

// ── window_fix_loop ──────────────────────────────────────────────

/// 周期性自动修复 LCU 窗口比例（对应 Python `_window_fix_loop`）。
pub async fn window_fix_loop(api: LcuClient, overlay_tx: mpsc::Sender<OverlayCmd>) {
    loop {
        match api.get_riotclient_zoom_scale().await {
            Ok(zoom) => {
                let _ = overlay_tx.send(OverlayCmd::AutoFixWindow(zoom)).await;
            }
            Err(_) => {}
        }
        sleep(Duration::from_secs_f64(WINDOW_FIX_POLL_SECS)).await;
    }
}
