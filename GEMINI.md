# LOL LCU 自动化工具 (Rust)

这是一个使用 Rust 编写的高性能、异步 Windows 原生 LOL 客户端 (LCU) 自动化工具。

## 项目概述

- **用途：** 自动化英雄联盟客户端的重复性任务，包括自动接受对局、自动跳过点赞投票、选人阶段组黑分析以及客户端内存监控与自动重载。
- **核心技术：**
  - **运行时：** `tokio` (异步 I/O 与任务管理)。
  - **LCU 交互：** `reqwest` (REST API) 与 `tokio-tungstenite` (WebSocket)。
  - **界面/覆盖层：** 基于 `windows` crate 的原生 Win32 透明覆盖层 (Overlay) 与系统托盘。
  - **系统集成：** 使用 `windows` crate 直接调用 Windows API（处理 DPI 感知、窗口管理、进程扫描）。
  - **日志诊断：** `tracing` 结构化日志（输出至控制台及 `lol_lcu.log`）。

## 架构设计

项目采用模块化设计：
- `src/main.rs`: 程序入口，负责 DPI 感知、单实例守卫、主重连循环及**并发事件分发**。
- `src/app/`: 核心业务逻辑。
  - `handlers.rs`: 处理 LCU 事件（游戏流阶段、就绪检查、选人、组黑分析、抢人任务管理）。
  - `config.rs`: 持久化用户配置（存储于 `%APPDATA%\lol-lcu\config.json`）。
  - `state.rs`: 内存中的会话状态管理（包括抢人任务代次与异步句柄）。
  - `tasks.rs`: 后台监控任务（内存监控、**窗口比例自动修复**）。
  - `premade.rs`: 组黑（Premade）分析算法。
- `src/lcu/`: LCU 客户端实现。
  - `api.rs`: LCU REST API 接口封装（含 Lobby、ChampSelect、Summoner 等）。
  - `connection.rs`: LCU 进程检测及从命令行参数提取凭据。
  - `websocket.rs`: WebSocket 事件监听与广播。
- `src/win/`: Windows 特有 UI 组件。
  - `overlay.rs`: 透明 Win32 覆盖层 (HUD & Bench) 与系统托盘。
  - `winapi.rs`: 底层 Windows API 工具（模拟点击、窗口定位、DPI 处理）。

## 功能特性 (当前版本)

- **自动接受 (Auto-Accept):** 检测到对局时自动点击接受（支持可配置延迟）。
- **极速响应 HUD:**
    - 使用并发分发模型，确保 WebSocket 事件处理不阻塞主循环。
    - 选人阶段专注显示**组黑分析结果**（含召唤师昵称）。
    - 游戏内分析结果在显示 **2 分钟后完全隐藏**（清空文字并关闭背景）。
    - **可见性优化：** 确保只要连接正常且处于有效游戏阶段，HUD 始终显示状态。
- **抢英雄增强:**
    - **交互修复：** 解决了鼠标穿透问题，板凳席槽位支持精确点击。
    - **视觉反馈：** 点击槽位后覆盖**绿色半透明遮罩**，任务进行中持续显示。
    - **循环抢人：** 后台高频尝试交换英雄，支持再次点击手动取消，且在**成功抢到或英雄消失后自动去除绿色遮罩**并清理任务状态。
- **战利品找回 (Loot Tool):** 
    - **手动触发：** 托盘菜单新增“找回一些遗忘的东西”，手动扫描可领取奖励。
    - **弹窗确认：** 自动识别任务奖励、免费宝箱等，弹窗列出清单并由用户确认后领取（参考 LeagueAkari）。
- **自动窗口修复:** 后台监控 LCU 窗口，若比例不符或缩放变化，自动执行比例拉伸修复。
- **智能启动同步:** 程序启动时主动通过 REST API 获取当前游戏阶段、房间、选人 session，实现即时衔接。
- **内存监控:** 若 `LeagueClientUx` 内存占用过高，自动触发 UI 重载。

## 开发规范
- **错误处理：** 应用层使用 `anyhow::Result`，库/模块层使用 `thiserror`。
- **并发管理：** 事件分发使用 `tokio::spawn` 异步派发；状态同步基于 `parking_lot::Mutex`。
- **WSL 互操作：** Windows 侧 Rust 工具链编译，通过 HTTP 桥接执行原生指令。

## 参考资料

### LCU API 查询
- [Kebs LCU Explorer](https://lcu.kebs.dev/?i=1): 实时 API 字段参考。
- [LCU Schema Tool](https://www.mingweisamuel.com/lcu-schema/tool/#/): 完整的 Swagger 定义与数据结构。

### 优秀类似项目
- [LeagueAkari](https://github.com/LeagueAkari/LeagueAkari): 功能最全面的 Rust 实现 LCU 工具。
- [fix-lcu-window](https://github.com/LeagueTavern/fix-lcu-window): 专业的 LCU 窗口修复工具实现。
- [Willump](https://github.com/elliejs/Willump/issues): 经典的 Python LCU 封装库。

### 技术指南
- [Hextech Docs](https://hextechdocs.dev/tag/lcu/): LCU 逆向工程与 WebSocket 协议详解。
- [英雄联盟官方 LCU 说明](https://lol.qq.com/cguide/Guide/LCU/LCUapi.html): 官方对本地 LCU 接口的基本描述。

## 更新日志 (Session Logs)

### 2026-03-21
- **环境适配：** 配置 WSL 与 Windows 桥接环境，解决 `TextOutW` 签名不匹配导致的编译错误。
- **性能重构：** 将 WebSocket 事件处理从顺序 `await` 升级为 `tokio::spawn` 并发分发，HUD 响应达到毫秒级。
- **功能补全：**
    - 恢复并优化了 LCU 窗口比例自动修复功能（增加 CEF 深度对齐）。
    - 增加了启动时主动获取 LCU 当前状态的逻辑。
- **交互优化：** 
    - 实现了透明窗口的鼠标事件捕获（Alpha=2 技巧），恢复抢英雄槽位点击功能。
    - 实现了抢英雄任务的视觉反馈（绿色遮罩）与代次管理逻辑。
    - **手动战利品找回：** 参考 LeagueAkari 实现手动触发的战利品扫描与弹窗确认领取功能。
- **视觉定制：**
    - 选人阶段改为仅显示组黑信息（昵称），移除冗余板凳席英雄列表。
    - 游戏内组黑信息显示 2 分钟后自动完全隐藏（文字与容器背景均清空）。
    - **HUD 修复：** 修正了 HUD 在非选人阶段异常隐藏的问题，确保状态始终可见。
- **稳定性增强：** 增加了单实例守卫，完善了 Release 模式下的控制台输出重定向。
