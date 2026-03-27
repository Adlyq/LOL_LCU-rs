# LOL LCU 自动化工具 (Rust)

这是一个使用 Rust 编写的高性能、异步 Windows 原生 LOL 客户端 (LCU) 自动化工具。

## 项目概述

- **用途：** 自动化英雄联盟客户端的重复性任务，包括自动接受对局、自动跳过点赞投票、选人阶段组黑分析以及客户端内存监控与自动重载。
- **核心技术：**
  - **运行时：** `tokio` (异步 I/O 与任务管理)。
  - **LCU 交互：** `reqwest` (REST API) 与 `tokio-tungstenite` (WebSocket)。
  - **界面/覆盖层：** 基于 `windows` crate 的原生 Win32 透明覆盖层 (Overlay) 与系统托盘。
  - **热键系统：** 基于 **WH_KEYBOARD_LL** 的底层键盘钩子，确保全屏游戏下的高可靠按键捕获。
  - **系统集成：** 使用 `windows` crate 直接调用 Windows API（处理 DPI 感知、窗口管理、进程扫描）。
  - **日志诊断：** `tracing` 结构化日志（输出至控制台及 `lol_lcu.log`）。

## 架构设计

项目采用模块化设计：

- `src/main.rs`: 程序入口，负责 DPI 感知、单实例守卫、主重连循环及**并发事件分发**。
- `src/app/`: 核心业务逻辑。
  - `handlers.rs`: 处理 LCU 事件（游戏流阶段、就绪检查、选人、组黑分析、抢人任务管理）。
  - `config.rs`: 持久化用户配置（存储于 `%APPDATA%\lol-lcu\config.json`）。
  - `state.rs`: 内存中的会话状态管理（包括抢人任务代次与异步句柄）。
  - `tasks.rs`: 后台监控任务（内存监控、窗口比例自动修复）。
  - `premade.rs`: 组黑（Premade）分析算法。
  - `prophet.rs`: 英雄先知（Prophet）评分系统，分析玩家 KDA 与评分。
- `src/lcu/`: LCU 客户端实现。
  - `api.rs`: LCU REST API 接口封装（含 Lobby、ChampSelect、Summoner 等）。
  - `connection.rs`: LCU 进程检测及从命令行参数提取凭据。
  - `websocket.rs`: WebSocket 事件监听与广播。
- `src/win/`: Windows 特有 UI 组件。
  - `overlay.rs`: 透明 Win32 覆盖层 (HUD1/HUD2) 与系统托盘，集成键盘钩子。
  - `winapi.rs`: 底层 Windows API 工具（模拟点击、窗口定位、DPI 处理）。

## 功能特性 (当前版本)

- **自动接受 (Auto-Accept):** 检测到对局时自动点击接受（支持可配置延迟）。
- **极速响应 HUD 系统 (HUD1 & HUD2 分离):**
  - **HUD1 (信息流):** 位于左上角。
    - **对局分析：** 选人阶段及进游戏后，同时显示**我方与对方**的组黑情况。
    - **Prophet 评分：** 显示双方玩家的评分、KDA 及代次评价（如“通天代”）。
    - **自动隐藏：** 进游戏 **2 分钟后自动隐藏** 窗口，但保留数据以供随时唤起。
    - **智能清理：** 退出对局或重连时自动清空历史评分，确保数据准确。
  - **HUD2 (交互层):** 覆盖在客户端板凳席上方。
    - **独立控制：** 显示状态由游戏阶段决定，不受 HUD1 显隐或快捷键影响。
    - **精准对齐：** 实时监控 LCU 窗口并自动同步位置，修复了切换阶段时的位置偏差。
- **全局快捷键 (F1):**
  - **底层捕获：** 使用 `WH_KEYBOARD_LL` 钩子，确保在英雄联盟全屏/高权限模式下依然生效。
  - **临时唤起：** 游戏中按下 **F1** 重新显示 HUD1（显示 30 秒后自动重新隐藏）。
  - **手动切换：** 若 HUD1 正在显示，按下 F1 可立即将其隐藏，操作完全不干扰 HUD2。
- **抢英雄增强:**
  - **视觉反馈：** 点击槽位后覆盖**绿色半透明遮罩**，任务进行中持续显示。
  - **循环抢人：** 后台高频尝试交换英雄，支持点击手动取消。
- **战利品找回 (Loot Tool):**
  - **手动触发：** 托盘菜单手动扫描遗忘奖励（如任务奖励、免费宝箱）。
  - **弹窗确认：** 列出清单供确认后一键领取。
- **自动窗口修复:** 后台监控 LCU 窗口比例，自动执行拉伸修复。
- **智能启动同步:** 启动时主动获取当前状态，实现即时衔接。

## 开发规范

- **错误处理：** 应用层使用 `anyhow::Result`，库/模块层使用 `thiserror`。
- **并发管理：** 事件分发使用 `tokio::spawn` 异步派发；状态同步基于 `parking_lot::Mutex`。

## 参考资料

### LCU API 查询

- [Kebs LCU Explorer](https://lcu.kebs.dev/?i=1): 实时 API 字段参考。
- [LCU Schema Tool](https://www.mingweisamuel.com/lcu-schema/tool/#/): 完整的 Swagger 定义与数据结构。
- [Riot Games 官方 API 文档](https://developer.riotgames.com/apis)英雄联盟开发者平台：官方 LCU API 文档（较为简略）。

### 优秀类似项目

- [LeagueAkari](https://github.com/LeagueAkari/LeagueAkari): 功能最全面的 LCU 工具。
- [fix-lcu-window](https://github.com/LeagueTavern/fix-lcu-window): 专业的 LCU 窗口修复工具实现。
- [Willump](https://github.com/elliejs/Willump/issues): 经典的 Python LCU 封装库。
- [Ezhana-lcu](https://github.com/Ezhana/lcu): 一个封装完好的LCU API及SGP支持的Go Mod。

### 技术指南

- [Hextech Docs](https://hextechdocs.dev/tag/lcu/): LCU 逆向工程与 WebSocket 协议详解。
- [英雄联盟官方 LCU 说明](https://lol.qq.com/cguide/Guide/LCU/LCUapi.html): 官方对本地 LCU 接口的基本描述。

## 更新日志 (Session Logs)

### 2026-03-22

- **架构解耦：** 将左上角信息流 (HUD1) 与板凳席交互层 (HUD2) 的可见性完全分离，快捷键不再干扰抢人操作。
- **评分增强：** Prophet 系统现在支持同时显示**我方与对方**的评分和 KDA（选人阶段及进游戏后均适用）。
- **热键重构：** 弃用 `RegisterHotKey`，改用 **WH_KEYBOARD_LL 底层键盘钩子**，解决了 F1 在游戏中失效的问题。
- **隐藏逻辑优化：** 进游戏 2 分钟后隐藏 HUD1 窗口但保留数据，配合 F1 实现临时唤起 30 秒功能。
- **对齐修复：** 解决了 HUD2 在进入选人阶段时位置不同步的 Bug，增加了强制同步机制。
- **稳定性：** 修复了 `Send` 约束冲突，清理了所有编译警告，达到 **0 Error 0 Warning**。

### 2026-03-21

- **环境适配：** 配置 WSL 与 Windows 桥接环境，解决编译错误。
- **性能重构：** WebSocket 事件并发分发，HUD 响应达到毫秒级。
- **功能补全：** 恢复 LCU 窗口比例自动修复，增加启动时主动状态获取。
- **交互优化：** 实现透明窗口鼠标事件捕获（Alpha=2），新增手动战利品找回功能。
- **HUD 修复：** 修正 HUD 异常隐藏问题，确保状态始终可见。
