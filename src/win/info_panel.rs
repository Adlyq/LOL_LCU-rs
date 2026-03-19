//! 信息面板窗口（egui/eframe 实现）
//!
//! 在独立线程上运行 eframe（glow 后端），通过 channel 接收内容更新与配置变更。
//! 与 overlay 线程解耦：toggle/update 均通过消息发送，不阻塞调用方。

use std::sync::{Arc, Mutex};

use eframe::egui;
use tracing::{info, warn};

use crate::app::config::SharedConfig;

// ── 面板动作（面板 → tokio 主循环）─────────────────────────────

/// 用户在面板上触发的动作，通过 tokio mpsc 发送给主循环处理。
#[derive(Debug, Clone)]
pub enum PanelAction {
    /// 热重载 LCU 客户端 UX
    ReloadUx,
    /// 退出结算界面，返回大厅
    PlayAgain,
    /// 手动领取已完成任务奖励 + 开启免费宝箱
    AutoLoot,
    /// 退出程序
    Quit,
}

// ── 公共数据类型 ─────────────────────────────────────────────────

/// 面板显示内容（可跨线程修改）。
#[derive(Debug, Default, Clone)]
pub struct PanelContent {
    pub connection: String,
    pub phase: String,
    pub premade: String,
    pub last_game: String,
}

// ── 内部消息 ─────────────────────────────────────────────────────

#[allow(dead_code)]
enum PanelMsg {
    /// 更新显示内容
    UpdateContent(PanelContent),
    /// 切换显隐
    Toggle,
    /// 显示
    Show,
}

// ── 公开句柄 ─────────────────────────────────────────────────────

/// 对信息面板的操作句柄（廉价 clone）。
#[derive(Clone)]
pub struct InfoPanel {
    tx: std::sync::mpsc::Sender<PanelMsg>,
    /// 当前内容（线程外读写用，eframe 内部也访问同一份）
    pub content: Arc<Mutex<PanelContent>>,
}

unsafe impl Send for InfoPanel {}
unsafe impl Sync for InfoPanel {}

impl InfoPanel {
    /// 在独立线程上启动 eframe 面板，返回操作句柄。
    ///
    /// `config` 由 UI 线程直接读写，tokio 侧通过同一份 Arc 读取。
    pub fn spawn(config: SharedConfig, action_tx: tokio::sync::mpsc::Sender<PanelAction>) -> Option<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<PanelMsg>();
        let content_shared = Arc::new(Mutex::new(PanelContent::default()));
        let content_for_thread = content_shared.clone();

        std::thread::Builder::new()
            .name("info-panel-egui".to_owned())
            .spawn(move || {
                run_panel_thread(rx, content_for_thread, config, action_tx);
            })
            .map_err(|e| warn!("启动信息面板线程失败: {e}"))
            .ok()?;

        info!("信息面板线程已启动");
        Some(InfoPanel { tx, content: content_shared })
    }

    /// 切换显隐。
    #[allow(dead_code)]
    pub fn toggle(&self) {
        let _ = self.tx.send(PanelMsg::Toggle);
    }

    /// 更新面板内容。
    pub fn update<F: FnOnce(&mut PanelContent)>(&self, f: F) {
        let mut c = self.content.lock().unwrap();
        f(&mut c);
        let snapshot = c.clone();
        drop(c);
        let _ = self.tx.send(PanelMsg::UpdateContent(snapshot));
    }

    /// 销毁（当前不需特殊处理，channel 关闭后线程自动退出）
    #[allow(dead_code)]
    pub fn destroy(&self) {}
}

// ── eframe App ───────────────────────────────────────────────────

struct PanelApp {
    rx: std::sync::mpsc::Receiver<PanelMsg>,
    content: Arc<Mutex<PanelContent>>,
    config: SharedConfig,
    action_tx: tokio::sync::mpsc::Sender<PanelAction>,
    /// 面板是否可见（eframe 本身就是窗口，用此控制内容区收折并隐藏窗口）
    visible: bool,
    /// 当前已应用的窗口透明度（用于变更检测）
    current_opacity: u8,
}

impl eframe::App for PanelApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 拦截窗口关闭请求 → 发送 Quit 动作给 tokio 主循环
        if ctx.input(|i| i.viewport().close_requested()) {
            let _ = self.action_tx.try_send(PanelAction::Quit);
        }
        // 消费消息
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                PanelMsg::UpdateContent(c) => {
                    *self.content.lock().unwrap() = c;
                    ctx.request_repaint();
                }
                PanelMsg::Toggle => {
                    self.visible = !self.visible;
                    ctx.request_repaint();
                }
                PanelMsg::Show => {
                    self.visible = true;
                    ctx.request_repaint();
                }
            }
        }

        // 窗口始终存在，但内容区通过 visible 控制
        // 当不可见时渲染一个极小占位 Panel
        if !self.visible {
            // 收缩到最小：只保留一个标题条
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(egui::Color32::from_rgb(20, 20, 28)))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("LOL-LCU  ·  已隐藏")
                                .color(egui::Color32::from_rgb(120, 120, 130))
                                .size(12.0),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("▶ 展开").clicked() {
                                self.visible = true;
                            }
                        });
                    });
                });
            return;
        }

        // 主面板
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgb(20, 20, 28))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(45, 48, 65))),
            )
            .show(ctx, |ui| {
                self.draw_title_bar(ui);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::NONE
                            .inner_margin(egui::Margin::symmetric(14_i8, 6_i8))
                            .show(ui, |ui| {
                                self.draw_status_section(ui);
                                ui.add_space(4.0);
                                self.draw_premade_section(ui);
                                ui.add_space(4.0);
                                self.draw_last_game_section(ui);
                                ui.add_space(6.0);
                                self.draw_actions_section(ui);
                                ui.add_space(8.0);
                                self.draw_settings_section(ui);
                                ui.add_space(6.0);
                            });
                    });
            });

        // 应用窗口透明度
        let desired_opacity = self.config.lock().opacity;
        if desired_opacity != self.current_opacity {
            if apply_window_opacity(desired_opacity) {
                self.current_opacity = desired_opacity;
            }
        }

        // 持续轮询（有消息时 repaint），100ms 刷新一次
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

impl PanelApp {
    fn draw_title_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let bar_frame = egui::Frame::NONE
            .fill(egui::Color32::from_rgb(30, 30, 42))
            .inner_margin(egui::Margin::symmetric(10_i8, 6_i8));
        bar_frame.show(ui, |ui| {
            ui.horizontal(|ui| {
                // 标题文字（可拖拽区域）
                let title = ui.label(
                    egui::RichText::new("  LOL-LCU 助手")
                        .strong()
                        .color(egui::Color32::from_rgb(180, 180, 220))
                        .size(14.0),
                );
                // 标题和按钮之间的空白也可拖拽
                let space = ui.allocate_response(
                    ui.available_size_before_wrap(),
                    egui::Sense::click_and_drag(),
                );
                if title.dragged() || space.dragged() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // 关闭按钮（退出程序）
                    let close_btn = ui.add(
                        egui::Button::new(
                            egui::RichText::new(" ✕ ")
                                .color(egui::Color32::from_rgb(220, 80, 80))
                                .size(13.0),
                        )
                        .frame(false),
                    );
                    if close_btn.clicked() {
                        let _ = self.action_tx.try_send(PanelAction::Quit);
                    }
                    close_btn.on_hover_text("退出程序");

                    // 最小化按钮
                    let min_btn = ui.add(
                        egui::Button::new(
                            egui::RichText::new(" − ")
                                .color(egui::Color32::from_rgb(180, 180, 100))
                                .size(13.0),
                        )
                        .frame(false),
                    );
                    if min_btn.clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                    min_btn.on_hover_text("最小化");


                });
            });
        });
        ui.add(egui::Separator::default().spacing(0.0));
    }

    fn draw_status_section(&self, ui: &mut egui::Ui) {
        let content = self.content.lock().unwrap().clone();
        section_header(ui, "▌ 状态");
        egui::Grid::new("status_grid")
            .num_columns(2)
            .spacing([8.0, 3.0])
            .show(ui, |ui| {
                label_key(ui, "连接");
                label_val(ui, if content.connection.is_empty() { "等待中..." } else { &content.connection });
                ui.end_row();
                label_key(ui, "阶段");
                label_val(ui, if content.phase.is_empty() { "—" } else { &content.phase });
                ui.end_row();
            });
    }

    fn draw_premade_section(&self, ui: &mut egui::Ui) {
        let premade = self.content.lock().unwrap().premade.clone();
        if premade.is_empty() {
            return;
        }
        ui.add(egui::Separator::default().spacing(4.0));
        section_header(ui, "▌ 组黑分析");
        ui.add_space(2.0);
        let text = egui::RichText::new(&premade)
            .font(egui::FontId::monospace(12.0))
            .color(egui::Color32::from_rgb(200, 210, 230));
        ui.label(text);
    }

    fn draw_last_game_section(&self, ui: &mut egui::Ui) {
        let last_game = self.content.lock().unwrap().last_game.clone();
        if last_game.is_empty() {
            return;
        }
        ui.add(egui::Separator::default().spacing(4.0));
        section_header(ui, "▌ 上一局");
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(&last_game)
                .font(egui::FontId::monospace(12.0))
                .color(egui::Color32::from_rgb(200, 210, 230)),
        );
    }

    fn draw_actions_section(&self, ui: &mut egui::Ui) {
        ui.add(egui::Separator::default().spacing(4.0));
        section_header(ui, "▌ 操作");
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("退出结算页面")
                            .color(egui::Color32::from_rgb(210, 210, 240))
                            .size(12.0),
                    )
                    .min_size(egui::vec2(120.0, 24.0)),
                )
                .clicked()
            {
                let _ = self.action_tx.try_send(PanelAction::PlayAgain);
            }
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("热重载客户端")
                            .color(egui::Color32::from_rgb(210, 210, 240))
                            .size(12.0),
                    )
                    .min_size(egui::vec2(120.0, 24.0)),
                )
                .clicked()
            {
                let _ = self.action_tx.try_send(PanelAction::ReloadUx);
            }
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("领取任务与宝箱")
                            .color(egui::Color32::from_rgb(210, 210, 240))
                            .size(12.0),
                    )
                    .min_size(egui::vec2(120.0, 24.0)),
                )
                .clicked()
            {
                let _ = self.action_tx.try_send(PanelAction::AutoLoot);
            }
        });
    }

    fn draw_settings_section(&mut self, ui: &mut egui::Ui) {
        ui.add(egui::Separator::default().spacing(4.0));
        section_header(ui, "▌ 设置");
        ui.add_space(4.0);

        let mut cfg = self.config.lock();
        let old = cfg.clone();

        egui::Grid::new("settings_grid")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                sub_header(ui, "对局匹配");
                ui.end_row();

                label_key(ui, "自动接受对局");
                ui.checkbox(&mut cfg.auto_accept_enabled, "");
                ui.end_row();

                if cfg.auto_accept_enabled {
                    label_key(ui, "接受延迟（秒）");
                    ui.add(
                        egui::Slider::new(&mut cfg.auto_accept_delay_secs, 0..=15)
                            .suffix("s"),
                    );
                    ui.end_row();
                }

                sub_header(ui, "战绩分析");
                ui.end_row();

                label_key(ui, "选人阶段组黑");
                ui.checkbox(&mut cfg.premade_champ_select, "");
                ui.end_row();

                label_key(ui, "游戏中组黑");
                ui.checkbox(&mut cfg.premade_ingame, "");
                ui.end_row();

                sub_header(ui, "点赞 & 结算");
                ui.end_row();

                label_key(ui, "自动跳过点赞");
                ui.checkbox(&mut cfg.auto_honor_skip, "");
                ui.end_row();

                sub_header(ui, "内存监控");
                ui.end_row();

                label_key(ui, "自动热重载");
                ui.checkbox(&mut cfg.memory_monitor, "");
                ui.end_row();

                if cfg.memory_monitor {
                    label_key(ui, "触发阈值（MB）");
                    ui.add(
                        egui::Slider::new(&mut cfg.memory_threshold_mb, 500..=4000)
                            .suffix(" MB"),
                    );
                    ui.end_row();
                }

                sub_header(ui, "窗口");
                ui.end_row();

                label_key(ui, "透明度");
                ui.add(
                    egui::Slider::new(&mut cfg.opacity, 30..=100)
                        .suffix("%"),
                );
                ui.end_row();
            });

        if *cfg != old {
            cfg.save();
        }
    }
}

// ── 窗口透明度 ───────────────────────────────────────────────────

/// 通过 Windows API 设置窗口整体透明度（30–100%）。
/// 返回是否成功找到窗口并设置。
fn apply_window_opacity(percent: u8) -> bool {
    use windows::Win32::Foundation::*;
    use windows::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        let title: Vec<u16> = "LOL-LCU 助手\0".encode_utf16().collect();
        let hwnd = match FindWindowW(
            windows::core::PCWSTR::null(),
            windows::core::PCWSTR(title.as_ptr()),
        ) {
            Ok(h) if !h.0.is_null() => h,
            _ => return false,
        };
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style | WS_EX_LAYERED.0 as i32);
        let alpha = (percent.clamp(30, 100) as f32 / 100.0 * 255.0) as u8;
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
        true
    }
}

// ── 辅助 UI 函数 ─────────────────────────────────────────────────

fn section_header(ui: &mut egui::Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(text)
            .strong()
            .color(egui::Color32::from_rgb(130, 160, 220))
            .size(13.0),
    );
    ui.add_space(2.0);
}

fn sub_header(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .color(egui::Color32::from_rgb(160, 160, 180))
            .size(12.0),
    );
}

fn label_key(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .color(egui::Color32::from_rgb(160, 170, 190))
            .size(12.0),
    );
}

fn label_val(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .color(egui::Color32::from_rgb(210, 220, 240))
            .size(12.0),
    );
}

// ── 线程入口 ─────────────────────────────────────────────────────

fn run_panel_thread(
    rx: std::sync::mpsc::Receiver<PanelMsg>,
    content: Arc<Mutex<PanelContent>>,
    config: SharedConfig,
    action_tx: tokio::sync::mpsc::Sender<PanelAction>,
) {
    let win_size = [420.0_f32, 580.0_f32];
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("LOL-LCU 助手")
        .with_inner_size(win_size)
        .with_min_inner_size([300.0, 120.0])
        .with_resizable(true)
        .with_decorations(false)   // 自绘标题栏
        .with_always_on_top()
        .with_taskbar(true);       // 出现在任务栏

    // 优先在副屏居中显示
    if let Some(pos) = find_secondary_monitor_pos(win_size[0], win_size[1]) {
        viewport = viewport.with_position(pos);
    }

    let options = eframe::NativeOptions {
        viewport,
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            builder.with_any_thread(true);
        })),
        ..Default::default()
    };

    let app = PanelApp {
        rx,
        content,
        config,
        action_tx,
        visible: true,
        current_opacity: 0,
    };

    if let Err(e) = eframe::run_native(
        "LOL-LCU 助手",
        options,
        Box::new(|cc| {
            // 应用深色主题
            let mut style = (*cc.egui_ctx.style()).clone();
            style.visuals = dark_visuals();
            cc.egui_ctx.set_style(style);
            // 设置中文字体支持（使用系统 SimHei 或 Microsoft YaHei）
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    ) {
        warn!("eframe 面板退出: {e}");
    }
}

/// 检测副屏位置，返回窗口应放置的坐标（居中于副屏工作区域）。
/// 若无副屏则返回 None，窗口将显示在默认位置。
fn find_secondary_monitor_pos(win_w: f32, win_h: f32) -> Option<egui::Pos2> {
    use windows::Win32::Foundation::*;
    use windows::Win32::Graphics::Gdi::*;

    unsafe extern "system" fn enum_cb(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let v = &mut *(data.0 as *mut Vec<HMONITOR>);
        v.push(hmon);
        BOOL(1)
    }

    unsafe {
        let mut monitors: Vec<HMONITOR> = Vec::new();
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(enum_cb),
            LPARAM(&mut monitors as *mut Vec<HMONITOR> as isize),
        );

        for &hmon in &monitors {
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(hmon, &mut mi).as_bool()
                && mi.dwFlags & 1 == 0  // MONITORINFOF_PRIMARY
            {
                let rc = mi.rcWork; // 工作区域（排除任务栏）
                let area_w = (rc.right - rc.left) as f32;
                let area_h = (rc.bottom - rc.top) as f32;
                let x = rc.left as f32 + (area_w - win_w) / 2.0;
                let y = rc.top as f32 + (area_h - win_h) / 2.0;
                return Some(egui::Pos2::new(x.max(rc.left as f32), y.max(rc.top as f32)));
            }
        }
    }
    None
}

fn dark_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.window_fill = egui::Color32::from_rgb(20, 20, 28);
    v.panel_fill = egui::Color32::from_rgb(20, 20, 28);
    v.override_text_color = Some(egui::Color32::from_rgb(210, 215, 230));
    v.widgets.inactive.bg_fill = egui::Color32::from_rgb(40, 42, 58);
    v.widgets.hovered.bg_fill = egui::Color32::from_rgb(55, 58, 80);
    v.widgets.active.bg_fill = egui::Color32::from_rgb(70, 75, 110);
    v
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // 尝试加载系统中文字体（Windows 上通常存在 msyh.ttc 或 simhei.ttf）
    let font_paths = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyhl.ttc",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
    ];

    let mut loaded = false;
    for path in &font_paths {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "chinese".to_owned(),
                egui::FontData::from_owned(bytes).into(),
            );
            // 追加到 Proportional 和 Monospace 字体族的末尾（回退字体）
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("chinese".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("chinese".to_owned());
            loaded = true;
            info!("已加载中文字体: {path}");
            break;
        }
    }

    if !loaded {
        warn!("未找到系统中文字体，汉字可能显示为方块");
    }

    ctx.set_fonts(fonts);
}
