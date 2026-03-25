pub mod config;
pub mod event;
/// 业务逻辑层：游戏事件处理
///
/// 对应 Python 侧 `app/game_handlers.py`。
pub mod handlers;
pub mod main_loop;
pub mod premade;
pub mod prophet;
pub mod scout;
pub mod sniper;
pub mod state;
pub mod tasks;
pub mod viewmodel;
