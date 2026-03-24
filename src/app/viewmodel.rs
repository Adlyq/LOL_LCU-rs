#[derive(Debug, Clone, PartialEq, Default)]
pub struct ViewModel {
    /// HUD1 (左上角) 是否显示
    pub hud1_visible: bool,
    /// HUD1 显示的标题 (如 "对局就绪")
    pub hud1_title: String,
    /// HUD1 显示的内容行 (如 Prophet 评分)
    pub hud1_lines: Vec<String>,
    
    /// HUD2 (板凳席) 是否显示
    pub hud2_visible: bool,
    /// HUD2 当前高亮的槽位索引 (Sniper 任务中)
    pub hud2_selected_slot: Option<usize>,
    
    /// 倒计时数字 (可选)
    pub countdown_secs: Option<u32>,
    
    /// LCU 窗口坐标与缩放 (用于 HUD2 对齐)
    pub lcu_rect: LcuRect,
    pub zoom_scale: f64,
    
    /// 连接状态
    pub is_connected: bool,
    pub current_phase: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LcuRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
