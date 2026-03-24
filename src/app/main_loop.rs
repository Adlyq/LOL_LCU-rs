use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use parking_lot::Mutex;
use tracing::{info, warn, debug, error};
use serde_json::Value;
use std::time::Duration;
use std::collections::HashMap;

use crate::app::event::{AppEvent, TrayAction};
use crate::app::state::RuntimeState;
use crate::app::viewmodel::ViewModel;
use crate::app::config::SharedConfig;
use crate::lcu::api::{LcuClient, gameflow};
use crate::win::winapi;
use crate::app::scout::ScoutService;
use crate::app::sniper::SniperService;
use crate::app::premade::{extract_teams_from_gameflow_session, extract_teams_from_session};

pub struct MainLoop {
    event_tx: mpsc::Sender<AppEvent>,
    event_rx: mpsc::Receiver<AppEvent>,
    vm_tx: watch::Sender<ViewModel>,
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
}

impl MainLoop {
    pub fn new(
        event_tx: mpsc::Sender<AppEvent>,
        event_rx: mpsc::Receiver<AppEvent>,
        vm_tx: watch::Sender<ViewModel>,
        state: Arc<Mutex<RuntimeState>>,
        config: SharedConfig,
    ) -> Self {
        Self {
            event_tx,
            event_rx,
            vm_tx,
            state,
            config,
            api: None,
            hide_timer: None,
            accept_timer: None,
            premade_msg: String::new(),
            my_scores: HashMap::new(),
            their_scores: HashMap::new(),
        }
    }

    pub async fn run(&mut self) {
        info!("主逻辑循环已启动");
        
        while let Some(event) = self.event_rx.recv().await {
            debug!("收到事件: {:?}", event);
            match event {
                AppEvent::LcuConnected(api) => {
                    info!("事件处理: LcuConnected");
                    self.api = Some(api.clone());
                    let mut vm = self.vm_tx.borrow().clone();
                    vm.is_connected = true;
                    vm.hud1_visible = true;
                    vm.hud1_title = "已连接 LCU".to_string();
                    let _ = self.vm_tx.send(vm);

                    // 启动后台任务
                    let api_c = api.clone();
                    let config_c = self.config.clone();
                    tokio::spawn(async move {
                        crate::app::tasks::memory_monitor_loop(api_c, config_c).await;
                    });

                    let api_c2 = api.clone();
                    let event_tx_c = self.event_tx.clone();
                    tokio::spawn(async move {
                        crate::app::tasks::window_fix_loop(api_c2, event_tx_c).await;
                    });

                    info!("LCU 后台监控任务已启动");
                }
                AppEvent::LcuDisconnected => {
                    warn!("事件处理: LcuDisconnected, 执行清理");
                    self.api = None;
                    self.reset_scout_results();
                    let mut vm = self.vm_tx.borrow().clone();
                    vm.is_connected = false;
                    vm.current_phase = "Disconnected".to_string();
                    vm.hud1_title = "等待 LCU 连接...".to_string();
                    vm.hud1_lines.clear();
                    let _ = self.vm_tx.send(vm);
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
                    info!("事件处理: TrayAction({:?})", action);
                    self.handle_tray_action(action).await;
                }
                AppEvent::BenchClick(index) => {
                    info!("事件处理: BenchClick({})", index);
                    self.handle_bench_click(index).await;
                }
                AppEvent::HotKeyF1 => {
                    info!("事件处理: HotKeyF1");
                    self.toggle_hud1().await;
                }
                AppEvent::Tick => {
                    self.handle_tick().await;
                }
                AppEvent::ScoutResult { puuid, content, is_premade, is_enemy } => {
                    debug!("收到战绩分析结果: puuid={}, is_premade={}", puuid, is_premade);
                    self.handle_scout_result(puuid, content, is_premade, is_enemy).await;
                }
                AppEvent::ConfigChanged => {
                    info!("事件处理: ConfigChanged");
                }
                AppEvent::RequestWindowFix { zoom, forced } => {
                    debug!("事件处理: RequestWindowFix(zoom={}, forced={})", zoom, forced);
                    if let Some(target) = winapi::find_lcu_window() {
                        winapi::fix_lcu_window_by_zoom(target, zoom, forced);
                        if let Some(r) = winapi::get_window_rect(target) {
                            let _ = self.event_tx.try_send(AppEvent::WindowRectUpdated {
                                x: r.left, y: r.top, width: r.right - r.left, height: r.bottom - r.top, zoom_scale: zoom
                            });
                        }
                    }
                }
                AppEvent::WindowRectUpdated { x, y, width, height, zoom_scale } => {
                    self.update_window_rect(x, y, width, height, zoom_scale).await;
                }
                AppEvent::Quit => {
                    info!("收到退出信号，终止主循环");
                    break;
                }
            }
        }
    }

    fn reset_scout_results(&mut self) {
        debug!("重置所有战绩分析缓存");
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
        info!("阶段状态转移: {} -> {}", self.vm_tx.borrow().current_phase, phase);
        let mut vm = self.vm_tx.borrow().clone();
        vm.current_phase = phase.clone();
        
        vm.hud2_visible = phase == gameflow::CHAMP_SELECT;
        vm.hud1_visible = true;
        
        // 关键逻辑修复：不仅仅是 NONE 阶段需要重置
        let is_in_game_process = phase == gameflow::CHAMP_SELECT || phase == gameflow::IN_PROGRESS || phase == gameflow::GAME_START || phase == gameflow::RECONNECT;
        
        if !is_in_game_process {
            debug!("检测到不在游戏进程中，执行 UI 内容清理");
            vm.hud1_title = if phase == gameflow::NONE { "已连接".to_string() } else { format!("当前阶段: {}", phase) };
            vm.hud1_lines.clear();
            self.reset_scout_results();
            self.hide_timer = None;
        } else {
            vm.hud1_title = format!("当前阶段: {}", phase);
        }

        {
            let mut state = self.state.lock();
            if phase != gameflow::CHAMP_SELECT && phase != gameflow::IN_PROGRESS {
                debug!("重置业务状态 (Premade/PickTask)");
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
                } else {
                    false
                }
            };
            if should_analyze {
                info!("触发游戏内对局分析 (ScoutService)");
                self.start_ingame_scout().await;
            }
            info!("启动 HUD1 自动隐藏计时器 (120s)");
            self.hide_timer = Some(120); 
        } else if phase == gameflow::CHAMP_SELECT {
            self.hide_timer = None; 
        } else if phase == gameflow::END_OF_GAME {
            info!("检测到对局结束，准备跳过点赞");
            self.handle_end_of_game().await;
        }

        let _ = self.vm_tx.send(vm);
    }

    async fn handle_ready_check(&mut self, payload: Value) {
        if let Some(state) = payload.get("state").and_then(|v| v.as_str()) {
            debug!("ReadyCheck 状态: {}", state);
            if state == "InProgress" && self.config.lock().auto_accept_enabled {
                if self.accept_timer.is_none() {
                    info!("检测到对局就绪，启动 2s 自动接受计时器");
                    self.accept_timer = Some(2); 
                }
            } else {
                if self.accept_timer.is_some() { debug!("ReadyCheck 已被取消或接受"); }
                self.accept_timer = None;
            }
        }
    }

    async fn handle_session_updated(&mut self, session: Value) {
        let bench = LcuClient::extract_bench_champion_ids(&session);
        {
            let mut s = self.state.lock();
            if s.current_bench_ids != bench {
                debug!("板凳席数据更新: {:?}", bench);
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
            info!("触发选人阶段组黑分析");
            self.start_champ_select_scout(session).await;
        }
    }

    async fn handle_tray_action(&mut self, action: TrayAction) {
        if let Some(api) = &self.api {
            match action {
                TrayAction::FixWindow => {
                    if let Ok(zoom) = api.get_riotclient_zoom_scale().await {
                        if let Some(target) = winapi::find_lcu_window() {
                            info!("执行手动窗口比例修复: zoom={}", zoom);
                            let _ = winapi::fix_lcu_window_by_zoom(target, zoom, true);
                        }
                    }
                }
                TrayAction::ReloadUx => { info!("执行手动热重载 UX"); let _ = api.reload_ux().await; }
                TrayAction::PlayAgain => { info!("执行再来一局 (退出结算)"); let _ = api.play_again().await; }
                TrayAction::FindForgottenLoot => { 
                    info!("执行战利品找回");
                    let api_c = api.clone();
                    tokio::spawn(async move { crate::app::handlers::handle_find_forgotten_loot(api_c).await; });
                }
                TrayAction::ToggleAutoAccept => { let mut c = self.config.lock(); c.auto_accept_enabled = !c.auto_accept_enabled; info!("自动接受: {}", c.auto_accept_enabled); c.save(); }
                TrayAction::ToggleAutoHonor => { let mut c = self.config.lock(); c.auto_honor_skip = !c.auto_honor_skip; info!("自动点赞: {}", c.auto_honor_skip); c.save(); }
                TrayAction::TogglePremadeChamp => { let mut c = self.config.lock(); c.premade_champ_select = !c.premade_champ_select; info!("选人组黑分析: {}", c.premade_champ_select); c.save(); }
                TrayAction::ToggleMemoryMonitor => { let mut c = self.config.lock(); c.memory_monitor = !c.memory_monitor; info!("内存监控: {}", c.memory_monitor); c.save(); }
                TrayAction::Exit => { info!("通过托盘退出程序"); let _ = self.event_tx.send(AppEvent::Quit).await; }
                _ => {}
            }
        }
    }

    async fn handle_bench_click(&mut self, index: usize) {
        let (champ_id, already_sniping) = {
            let mut s = self.state.lock();
            if index >= s.current_bench_ids.len() { 
                warn!("点击了无效的板凳席索引: {}", index);
                return; 
            }
            let id = s.current_bench_ids[index];
            let already = s.active_pick_slot == Some(index);
            if already {
                debug!("取消抢人任务: slot={}", index);
                s.cancel_pick_task();
            } else {
                debug!("启动抢人任务: slot={}, champ_id={}", index, id);
                s.active_pick_slot = Some(index);
            }
            (id, already)
        };

        let mut vm = self.vm_tx.borrow().clone();
        if already_sniping {
            vm.hud2_selected_slot = None;
        } else {
            vm.hud2_selected_slot = Some(index);
            if let Some(api) = &self.api {
                let sniper = SniperService::new(api.clone(), self.event_tx.clone());
                sniper.start_sniping(champ_id, index).await;
            }
        }
        let _ = self.vm_tx.send(vm);
    }

    async fn start_ingame_scout(&self) {
        if let (Some(api), true) = (&self.api, self.config.lock().premade_ingame) {
            let api_c = api.clone();
            let event_tx_c = self.event_tx.clone();
            tokio::spawn(async move {
                debug!("Scout: 正在获取游戏内 Session...");
                if let Ok(session) = api_c.get_gameflow_session().await {
                    let me = api_c.get_current_summoner().await.unwrap_or_default();
                    let my_puuid = me.get("puuid").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                    let id_name = api_c.get_champion_id_name_map().await.unwrap_or_default();
                    let (my_team, their_team, my_side, their_side) = extract_teams_from_gameflow_session(&session, &my_puuid, &id_name);
                    
                    if !my_team.is_empty() {
                        let scout = ScoutService::new(api_c, event_tx_c);
                        scout.execute_full_scout(my_team, their_team, my_side, their_side).await;
                    }
                }
            });
        }
    }

    async fn start_champ_select_scout(&self, session: Value) {
        if let (Some(api), true) = (&self.api, self.config.lock().premade_champ_select) {
            let api_c = api.clone();
            let event_tx_c = self.event_tx.clone();
            tokio::spawn(async move {
                let (my_raw, _their_raw, my_side, their_side) = extract_teams_from_session(&session);
                let my_team: Vec<(String, String)> = my_raw.iter().map(|(p, n, _)| (p.clone(), n.clone())).collect();
                
                if !my_team.is_empty() {
                    let scout = ScoutService::new(api_c, event_tx_c);
                    scout.execute_full_scout(my_team, Vec::new(), my_side, their_side).await;
                }
            });
        }
    }

    async fn handle_end_of_game(&mut self) {
        if let (Some(api), true) = (&self.api, self.config.lock().auto_honor_skip) {
            let api_c = api.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                match api_c.skip_honor_vote().await {
                    Ok(true) => info!("自动跳过点赞成功"),
                    Ok(false) => debug!("暂无可跳过的点赞页面"),
                    Err(e) => error!("自动跳过点赞失败: {}", e),
                }
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
                    info!("HUD1 自动隐藏触发");
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
                        info!("执行 LCU 接受对局操作");
                        let _ = api.accept_ready_check().await;
                    }
                    self.accept_timer = None;
                }
            }
        }

        if changed {
            let _ = self.vm_tx.send(vm);
        }
    }

    async fn handle_scout_result(&mut self, puuid: String, content: String, is_premade: bool, is_enemy: bool) {
        if is_premade {
            self.premade_msg = content;
        } else if is_enemy {
            self.their_scores.insert(puuid, content);
        } else {
            self.my_scores.insert(puuid, content);
        }

        // 重新合成 hud1_lines
        let mut lines = Vec::new();
        
        // 1. 组黑消息
        if !self.premade_msg.is_empty() {
            for line in self.premade_msg.lines() {
                lines.push(line.to_string());
            }
            lines.push(String::new()); // 分隔线
        }

        // 2. 我方评分
        if !self.my_scores.is_empty() {
            lines.push("[我方评分]".to_string());
            let mut my_list: Vec<_> = self.my_scores.values().cloned().collect();
            my_list.sort(); // 保持顺序稳定
            lines.extend(my_list);
        }

        // 3. 敌方评分
        if !self.their_scores.is_empty() {
            lines.push(String::new()); // 分隔线
            lines.push("[敌方评分]".to_string());
            let mut their_list: Vec<_> = self.their_scores.values().cloned().collect();
            their_list.sort();
            lines.extend(their_list);
        }

        let mut vm = self.vm_tx.borrow().clone();
        vm.hud1_lines = lines;
        let _ = self.vm_tx.send(vm);
    }

    async fn toggle_hud1(&mut self) {
        let mut vm = self.vm_tx.borrow().clone();
        vm.hud1_visible = !vm.hud1_visible;
        
        if vm.hud1_visible {
            let phase = &vm.current_phase;
            let is_in_game = phase == gameflow::IN_PROGRESS || phase == gameflow::GAME_START || phase == gameflow::RECONNECT;
            
            if is_in_game {
                info!("用户手动呼出 HUD1，设置 30s 临时显示计时器");
                self.hide_timer = Some(30); 
            } else {
                info!("用户手动呼出 HUD1，当前不在对局中，不设置自动隐藏");
                self.hide_timer = None;
            }
        } else {
            info!("用户手动隐藏 HUD1");
            self.hide_timer = None;
        }
        let _ = self.vm_tx.send(vm);
    }

    async fn update_window_rect(&mut self, x: i32, y: i32, width: i32, height: i32, zoom_scale: f64) {
        let mut vm = self.vm_tx.borrow().clone();
        if vm.lcu_rect.x != x || vm.lcu_rect.y != y || vm.lcu_rect.width != width || vm.lcu_rect.height != height {
            debug!("更新 LCU 窗口坐标: {}x{} @ ({},{})", width, height, x, y);
            vm.lcu_rect.x = x;
            vm.lcu_rect.y = y;
            vm.lcu_rect.width = width;
            vm.lcu_rect.height = height;
            vm.zoom_scale = zoom_scale;
            let _ = self.vm_tx.send(vm);
        }
    }
}
