use crate::lcu::api::LcuClient;
use crate::lcu::websocket::LcuEvent;

#[derive(Debug, Clone)]
pub enum AppEvent {
    /// LCU 已连接
    LcuConnected(LcuClient),

    /// LCU 已断开
    LcuDisconnected,

    /// 原始 LCU WebSocket 事件
    LcuEvent(LcuEvent),

    /// 托盘菜单动作
    TrayAction(TrayAction),

    /// 板凳席槽位点击 (HUD2)
    BenchClick(usize),

    /// 抢英雄任务结束 (自然结束或失败)
    SniperFinished(usize),

    /// 全局快捷键 F1
    HotKeyF1,

    /// 每秒一次的计时器信号
    Tick,

    /// 战绩分析结果 (Prophet/Premade)
    ScoutResult {
        puuid: String,
        content: String,
        is_premade: bool,
        is_enemy: bool,
    },

    /// 窗口比例/位置更新
    WindowRectUpdated {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        zoom_scale: f64,
    },

    /// 程序退出信号
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    ReloadUx,
    PlayAgain,
    FindForgottenLoot,
    FixWindow,
    ToggleAutoAccept,
    ToggleAutoHonor,
    TogglePremadeChamp,
    ToggleMemoryMonitor,
    Exit,
}
