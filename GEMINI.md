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
- `src/main.rs`: 程序入口，负责 DPI 感知设置及主重连循环。
- `src/app/`: 核心业务逻辑。
  - `handlers.rs`: 处理 LCU 事件（游戏流阶段、就绪检查、选人、点赞）。
  - `config.rs`: 持久化用户配置（存储于 `%APPDATA%\lol-lcu\config.json`）。
  - `state.rs`: 内存中的会话状态管理。
  - `tasks.rs`: 后台循环任务（如内存监控）。
  - `premade.rs`: 组黑（Premade）分析逻辑。
- `src/lcu/`: LCU 客户端实现。
  - `api.rs`: LCU REST API 接口封装。
  - `connection.rs`: LCU 进程检测及从命令行参数提取凭据。
  - `websocket.rs`: WebSocket 事件监听与广播。
- `src/win/`: Windows 特有 UI 组件。
  - `overlay.rs`: 透明 Win32 覆盖层 (HUD) 与系统托盘集成。
  - `winapi.rs`: 底层 Windows API 工具（如模拟点击、查找窗口）。

## 编译与运行 (WSL 环境特别说明)

由于本项目依赖 Windows 原生 API，**必须使用 Windows 侧的 Rust 工具链进行编译**。

### 环境要求
- **OS:** Windows 10/11。
- **工具链:** `x86_64-pc-windows-msvc`。

### 开发指令
在 WSL 终端中，请通过 `.exe` 后缀调用 Windows 侧的 Cargo：
- **运行：** `cargo.exe run`
- **发布构建：** `cargo.exe build --release`
  - 发布版本会启用 `#![windows_subsystem = "windows"]` 以隐藏控制台窗口。

## 功能特性
- **自动接受 (Auto-Accept):** 检测到对局时自动点击接受（延迟可配置）。
- **自动点赞跳过 (Auto-Honor Skip):** 自动跳过点赞阶段，加速进入结算界面。
- **组黑分析 (Premade Analysis):** 在选人及游戏过程中分析并识别预组队玩家，结果显示在 HUD 上。
- **内存监控 (Memory Monitor):** 若 `LeagueClientUx` 内存占用过高（默认 1500MB），自动触发重载。
- **HUD 覆盖层:** 在客户端上方实时显示状态信息（如 ARAM 板凳席英雄）。

## 开发规范
- **错误处理：** 应用层使用 `anyhow::Result`，库/模块层使用 `thiserror`。
- **并发管理：** 配置与状态使用 `Arc<Mutex<T>>` (基于 `parking_lot`) 确保线程安全。
- **WSL 互操作：** 在 WSL 中操作源码时，注意不要使用 Linux 原生 `cargo` 编译，否则会因缺少 Windows 标头文件而失败。

## 更新日志 (Session Logs)

### 2026-03-21
- **初始化环境：** 配置 WSL 访问 Windows 的 HTTP 桥接 (172.18.160.1:8080) 以执行原生命令。
- **项目审计：** 确认项目结构、技术栈及编译流程。
- **编译修复：** 修正 `src/win/overlay.rs` 中 `TextOutW` 因 `windows` crate 版本升级导致的签名不匹配错误。
- **代码重构：** 
    - 统一 `to_wide` 工具函数，并将其设为 `pub(crate)` 位于 `src/win/winapi.rs`。
    - 清理 `src/win/overlay.rs` 中的冗余定义及未使用的 `std` 引用。
- **健康检查：** 确认项目在 Windows 工具链下通过 `cargo check`，WebSocket 与事件处理逻辑审计完毕。
