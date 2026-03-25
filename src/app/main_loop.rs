use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::app::config::SharedConfig;
use crate::app::event::{AppEvent, TrayAction};
use crate::app::premade::{extract_teams_from_gameflow_session, extract_teams_from_session};
use crate::app::scout::ScoutService;
use crate::app::sniper::SniperService;
use crate::app::state::RuntimeState;
use crate::app::viewmodel::ViewModel;
use crate::lcu::api::{gameflow, LcuClient};
use crate::win::overlay::OverlaySender;
use crate::win::winapi;

pub struct MainLoop {
    event_tx: mpsc::Sender<AppEvent>,
    event_rx: mpsc::Receiver<AppEvent>,
    vm_tx: watch::Sender<ViewModel>,
    overlay_tx: OverlaySender,
    state: Arc<Mutex<RuntimeState>>,
    config: SharedConfig,
    api: Option<LcuClient>,

    // 隐藏倒计时
    hide_timer: Option<u32>,
    // 自动接受倒计时
    accept_timer: Option<u32>,

    // 缓存分析结果以便重新排序渲染
    premade_msg: String,
    my_scores: HashMap<String, String>,
    their_scores: HashMap<String, String>,

    // 任务取消令牌
    scout_token: Option<CancellationToken>,
    sniper_token: Option<CancellationToken>,
    lcu_token: Option<CancellationToken>,
}

impl MainLoop {
    pub fn new(
        event_tx: mpsc::Sender<AppEvent>,
        event_rx: mpsc::Receiver<AppEvent>,
        vm_tx: watch::Sender<ViewModel>,
        overlay_tx: OverlaySender,
        state: Arc<Mutex<RuntimeState>>,
        config: SharedConfig,
    ) -> Self {
        Self {
            event_tx,
            event_rx,
            vm_tx,
            overlay_tx,
            state,
            config,
            api: None,
            hide_timer: None,
            accept_timer: None,
            premade_msg: String::new(),
            my_scores: HashMap::new(),
            their_scores: HashMap::new(),
            scout_token: None,
            sniper_token: None,
            lcu_token: None,
        }
    }

    fn send_vm(&self, vm: ViewModel) {
        if self.vm_tx.send(vm).is_ok() {
            self.overlay_tx.wake_up();
        }
    }

    pub async fn run(&mut self) {
        info!("主逻辑循环已启动");

        while let Some(event) = self.event_rx.recv().await {
            match event {
                AppEvent::LcuConnected(api) => {
                    info!("事件处理: LcuConnected");
                    self.api = Some(api.clone());
                    let mut vm = self.vm_tx.borrow().clone();
                    vm.is_connected = true;
                    vm.hud1_visible = true;
                    vm.hud1_title = "已连接 LCU".to_string();
                    self.send_vm(vm);

                    // 1. 创建新的 LCU 令牌并启动后台任务
                    if let Some(token) = self.lcu_token.take() {
                        token.cancel();
                    }
                    let lcu_token = CancellationToken::new();
                    self.lcu_token = Some(lcu_token.clone());

                    let api_c = api.clone();
                    let config_c = self.config.clone();
                    let t1 = lcu_token.clone();
                    tokio::spawn(async move {
                        crate::app::tasks::memory_monitor_loop(api_c, config_c, t1).await;
                    });

                    let api_c2 = api.clone();
                    let event_tx_c = self.event_tx.clone();
                    let t2 = lcu_token.clone();
                    tokio::spawn(async move {
                        crate::app::tasks::window_fix_loop(api_c2, event_tx_c, t2).await;
                    });
                }
                AppEvent::LcuDisconnected => {
                    warn!("事件处理: LcuDisconnected, 执行清理");
                    self.api = None;
                    self.reset_scout_results();
                    if let Some(token) = self.lcu_token.take() {
                        token.cancel();
                    }
                    if let Some(token) = self.scout_token.take() {
                        token.cancel();
                    }
                    if let Some(token) = self.sniper_token.take() {
                        token.cancel();
                    }

                    let mut vm = self.vm_tx.borrow().clone();
                    vm.is_connected = false;
                    vm.current_phase = "Disconnected".to_string();
                    vm.hud1_title = "等待 LCU 连接...".to_string();
                    vm.hud1_lines.clear();
                    self.send_vm(vm);
                }
                AppEvent::LcuEvent(lcu_event) => {
                    self.handle_lcu_event(lcu_event).await;
                }
                AppEvent::LcuPhaseChanged(phase) => {
                    self.handle_phase_changed(phase).await;
                }
                AppEvent::LcuSessionUpdated(session) => {
                    self.handle_session_updated(session).await;
                }
                AppEvent::TrayAction(action) => {
                    self.handle_tray_action(action).await;
                }
                AppEvent::BenchClick(index) => {
                    self.handle_bench_click(index).await;
                }
                AppEvent::SniperFinished(index) => {
                    let mut s = self.state.lock();
                    if s.active_pick_slot == Some(index) {
                        s.active_pick_slot = None;
                        let mut vm = self.vm_tx.borrow().clone();
                        vm.hud2_selected_slot = None;
                        self.send_vm(vm);
                    }
                }
                AppEvent::HotKeyF1 => {
                    self.toggle_hud1().await;
                }
                AppEvent::Tick => {
                    self.handle_tick().await;
                }
                AppEvent::ScoutResult {
                    puuid,
                    content,
                    is_premade,
                    is_enemy,
                } => {
                    self.handle_scout_result(puuid, content, is_premade, is_enemy)
                        .await;
                }
                AppEvent::RequestWindowFix { zoom, forced } => {
                    if let Some(target) = winapi::find_lcu_window() {
                        winapi::fix_lcu_window_by_zoom(target, zoom, forced);
                        if let Some(r) = winapi::get_window_rect(target) {
                            let _ = self.event_tx.try_send(AppEvent::WindowRectUpdated {
                                x: r.left,
                                y: r.top,
                                width: r.right - r.left,
                                height: r.bottom - r.top,
                                zoom_scale: zoom,
                            });
                        }
                    }
                }
                AppEvent::WindowRectUpdated {
                    x,
                    y,
                    width,
                    height,
                    zoom_scale,
                } => {
                    self.update_window_rect(x, y, width, height, zoom_scale)
                        .await;
                }
                AppEvent::Quit => {
                    info!("收到退出信号，终止主循环");
                    break;
                }
                _ => {}
            }
        }
    }

    fn reset_scout_results(&mut self) {
        self.premade_msg.clear();
        self.my_scores.clear();
        self.their_scores.clear();
    }

    async fn handle_lcu_event(&mut self, event: crate::lcu::websocket::LcuEvent) {
        match event.uri.as_str() {
            "/lol-gameflow/v1/gameflow-phase" => {
                if let Some(phase) = event.payload.as_str() {
                    self.handle_phase_changed(phase.to_string()).await;
                }
            }
            "/lol-champ-select/v1/session" => {
                self.handle_session_updated(event.payload).await;
            }
            "/lol-matchmaking/v1/ready-check" => {
                self.handle_ready_check(event.payload).await;
            }
            _ => {}
        }
    }

    async fn handle_phase_changed(&mut self, phase: String) {
        info!(
            "阶段状态转移: {} -> {}",
            self.vm_tx.borrow().current_phase,
            phase
        );
        let mut vm = self.vm_tx.borrow().clone();
        vm.current_phase = phase.clone();

        vm.hud2_visible = phase == gameflow::CHAMP_SELECT;
        vm.hud1_visible = true;

        // 1. 任务生命周期管理
        if phase != gameflow::CHAMP_SELECT {
            if let Some(token) = self.sniper_token.take() {
                debug!("正在取消抢人任务 (离开选人阶段)");
                token.cancel();
            }
        }

        let is_in_game_process = phase == gameflow::CHAMP_SELECT
            || phase == gameflow::IN_PROGRESS
            || phase == gameflow::GAME_START
            || phase == gameflow::RECONNECT;
        if !is_in_game_process {
            if let Some(token) = self.scout_token.take() {
                debug!("正在取消战绩分析任务 (非游戏进程)");
                token.cancel();
            }
            vm.hud1_title = if phase == gameflow::NONE {
                "已连接".to_string()
            } else {
                format!("当前阶段: {}", phase)
            };
            vm.hud1_lines.clear();
            self.reset_scout_results();
            self.hide_timer = None;
        } else {
            vm.hud1_title = format!("当前阶段: {}", phase);
        }

        {
            let mut state = self.state.lock();
            if phase != gameflow::CHAMP_SELECT && phase != gameflow::IN_PROGRESS {
                state.reset_premade_status();
                state.cancel_pick_task();
                vm.hud2_selected_slot = None;
            }
        }

        if phase == gameflow::IN_PROGRESS || phase == gameflow::GAME_START {
            let should_analyze = {
                let mut s = self.state.lock();
                if !s.premade_ingame_done {
                    s.premade_ingame_done = true;
                    true
                } else if self.my_scores.is_empty() && self.their_scores.is_empty() {
                    true
                } else {
                    false
                }
            };
            if should_analyze {
                self.start_ingame_scout().await;
            }
            self.hide_timer = Some(120);
        } else if phase == gameflow::CHAMP_SELECT {
            self.hide_timer = None;
        } else if phase == gameflow::END_OF_GAME {
            self.handle_end_of_game().await;
        }

        self.send_vm(vm);
    }

    async fn handle_ready_check(&mut self, payload: Value) {
        if let Some(state) = payload.get("state").and_then(|v| v.as_str()) {
            if state == "InProgress" && self.config.lock().auto_accept_enabled {
                if self.accept_timer.is_none() {
                    self.accept_timer = Some(2);
                }
            } else {
                self.accept_timer = None;
            }
        }
    }

    async fn handle_session_updated(&mut self, session: Value) {
        let bench = LcuClient::extract_bench_champion_ids(&session);
        {
            let mut s = self.state.lock();
            if s.current_bench_ids != bench {
                s.current_bench_ids = bench;
            }
        }

        let should_analyze = {
            let mut s = self.state.lock();
            if !s.premade_analysis_done {
                s.premade_analysis_done = true;
                true
            } else {
                false
            }
        };

        if should_analyze && self.config.lock().premade_champ_select {
            self.start_champ_select_scout(session).await;
        }
    }

    async fn handle_tray_action(&mut self, action: TrayAction) {
        if let Some(api) = &self.api {
            match action {
                TrayAction::FixWindow => {
                    if let Ok(zoom) = api.get_riotclient_zoom_scale().await {
                        if let Some(target) = winapi::find_lcu_window() {
                            let _ = winapi::fix_lcu_window_by_zoom(target, zoom, true);
                        }
                    }
                }
                TrayAction::ReloadUx => {
                    let _ = api.reload_ux().await;
                }
                TrayAction::PlayAgain => {
                    let _ = api.play_again().await;
                }
                TrayAction::FindForgottenLoot => {
                    let api_c = api.clone();
                    tokio::spawn(async move {
                        crate::app::handlers::handle_find_forgotten_loot(api_c).await;
                    });
                }
                TrayAction::ToggleAutoAccept => {
                    let mut c = self.config.lock();
                    c.auto_accept_enabled = !c.auto_accept_enabled;
                    c.save();
                }
                TrayAction::ToggleAutoHonor => {
                    let mut c = self.config.lock();
                    c.auto_honor_skip = !c.auto_honor_skip;
                    c.save();
                }
                TrayAction::TogglePremadeChamp => {
                    let mut c = self.config.lock();
                    c.premade_champ_select = !c.premade_champ_select;
                    c.save();
                }
                TrayAction::ToggleMemoryMonitor => {
                    let mut c = self.config.lock();
                    c.memory_monitor = !c.memory_monitor;
                    c.save();
                }
                TrayAction::Exit => {
                    let _ = self.event_tx.send(AppEvent::Quit).await;
                }
                _ => {}
            }
        }
    }

    async fn handle_bench_click(&mut self, index: usize) {
        let (champ_id, already_sniping) = {
            let mut s = self.state.lock();
            if index >= s.current_bench_ids.len() {
                return;
            }
            let id = s.current_bench_ids[index];
            let already = s.active_pick_slot == Some(index);
            if already {
                s.cancel_pick_task();
                if let Some(token) = self.sniper_token.take() {
                    token.cancel();
                }
            } else {
                s.active_pick_slot = Some(index);
                if let Some(token) = self.sniper_token.take() {
                    token.cancel();
                }
                self.sniper_token = Some(CancellationToken::new());
            }
            (id, already)
        };

        let mut vm = self.vm_tx.borrow().clone();
        if already_sniping {
            vm.hud2_selected_slot = None;
        } else {
            vm.hud2_selected_slot = Some(index);
            if let (Some(api), Some(token)) = (&self.api, &self.sniper_token) {
                let sniper = SniperService::new(api.clone(), self.event_tx.clone());
                sniper.start_sniping(champ_id, index, token.clone()).await;
            }
        }
        self.send_vm(vm);
    }

    async fn start_ingame_scout(&mut self) {
        if let (Some(api), true) = (&self.api, self.config.lock().premade_ingame) {
            if let Some(token) = self.scout_token.take() {
                token.cancel();
            }
            let token = CancellationToken::new();
            self.scout_token = Some(token.clone());

            let api_c = api.clone();
            let event_tx_c = self.event_tx.clone();
            tokio::spawn(async move {
                if let Ok(session) = api_c.get_gameflow_session().await {
                    let me = api_c.get_current_summoner().await.unwrap_or_default();
                    let my_puuid = me
                        .get("puuid")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let id_name = api_c.get_champion_id_name_map().await.unwrap_or_default();
                    let (my_team, their_team, my_side, their_side) =
                        extract_teams_from_gameflow_session(&session, &my_puuid, &id_name);

                    if !my_team.is_empty() {
                        let scout = ScoutService::new(api_c, event_tx_c);
                        scout
                            .execute_full_scout(my_team, their_team, my_side, their_side, token)
                            .await;
                    }
                }
            });
        }
    }

    async fn start_champ_select_scout(&mut self, session: Value) {
        if let (Some(api), true) = (&self.api, self.config.lock().premade_champ_select) {
            if let Some(token) = self.scout_token.take() {
                token.cancel();
            }
            let token = CancellationToken::new();
            self.scout_token = Some(token.clone());

            let api_c = api.clone();
            let event_tx_c = self.event_tx.clone();
            tokio::spawn(async move {
                let (my_raw, _their_raw, my_side, their_side) =
                    extract_teams_from_session(&session);
                let my_team: Vec<(String, String)> = my_raw
                    .iter()
                    .map(|(p, n, _)| (p.clone(), n.clone()))
                    .collect();

                if !my_team.is_empty() {
                    let scout = ScoutService::new(api_c, event_tx_c);
                    scout
                        .execute_full_scout(my_team, Vec::new(), my_side, their_side, token)
                        .await;
                }
            });
        }
    }

    async fn handle_end_of_game(&mut self) {
        if let (Some(api), true) = (&self.api, self.config.lock().auto_honor_skip) {
            let api_c = api.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = api_c.skip_honor_vote().await;
            });
        }
    }

    async fn handle_tick(&mut self) {
        let mut changed = false;
        let mut vm = self.vm_tx.borrow().clone();

        if let Some(mut t) = self.hide_timer {
            if t > 0 {
                t -= 1;
                self.hide_timer = Some(t);
                if t == 0 {
                    vm.hud1_visible = false;
                    changed = true;
                }
            }
        }

        if let Some(mut t) = self.accept_timer {
            if t > 0 {
                t -= 1;
                self.accept_timer = Some(t);
                if t == 0 {
                    if let Some(api) = &self.api {
                        let _ = api.accept_ready_check().await;
                    }
                    self.accept_timer = None;
                }
            }
        }

        if changed {
            self.send_vm(vm);
        }
    }

    async fn handle_scout_result(
        &mut self,
        puuid: String,
        content: String,
        is_premade: bool,
        is_enemy: bool,
    ) {
        if is_premade {
            self.premade_msg = content;
        } else if is_enemy {
            self.their_scores.insert(puuid, content);
        } else {
            self.my_scores.insert(puuid, content);
        }

        let mut lines = Vec::new();
        if !self.premade_msg.is_empty() {
            for line in self.premade_msg.lines() {
                lines.push(line.to_string());
            }
            lines.push(String::new());
        }
        if !self.my_scores.is_empty() {
            lines.push("[我方评分]".to_string());
            let mut my_list: Vec<_> = self.my_scores.values().cloned().collect();
            my_list.sort();
            lines.extend(my_list);
        }
        if !self.their_scores.is_empty() {
            lines.push(String::new());
            lines.push("[敌方评分]".to_string());
            let mut their_list: Vec<_> = self.their_scores.values().cloned().collect();
            their_list.sort();
            lines.extend(their_list);
        }

        let mut vm = self.vm_tx.borrow().clone();
        vm.hud1_lines = lines;
        self.send_vm(vm);
    }

    async fn toggle_hud1(&mut self) {
        let mut vm = self.vm_tx.borrow().clone();
        vm.hud1_visible = !vm.hud1_visible;

        if vm.hud1_visible {
            let phase = &vm.current_phase;
            let is_in_game = phase == gameflow::IN_PROGRESS
                || phase == gameflow::GAME_START
                || phase == gameflow::RECONNECT;
            if is_in_game {
                self.hide_timer = Some(30);
            } else {
                self.hide_timer = None;
            }
        } else {
            self.hide_timer = None;
        }
        self.send_vm(vm);
    }

    async fn update_window_rect(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        zoom_scale: f64,
    ) {
        let mut vm = self.vm_tx.borrow().clone();
        if vm.lcu_rect.x != x
            || vm.lcu_rect.y != y
            || vm.lcu_rect.width != width
            || vm.lcu_rect.height != height
        {
            vm.lcu_rect.x = x;
            vm.lcu_rect.y = y;
            vm.lcu_rect.width = width;
            vm.lcu_rect.height = height;
            vm.zoom_scale = zoom_scale;
            self.send_vm(vm);
        }
    }
}
