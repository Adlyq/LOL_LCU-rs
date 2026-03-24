# 重构方案达成一致记录 (REFACTOR_AGREEMENT.md)

日期: 2026-03-24
参与者: Infrastructure Agent, Logic Agent, UI Agent

## 1. 核心共识

所有 Agent 一致同意遵循 `REFACTOR_PLAN.md` 的核心设计：
- **被动观察者模式**：以 LCU WebSocket 推送为唯一真相来源。
- **MVVM 架构**：引入 `AppEvent` (Event) -> `MainLoop` (Logic) -> `ViewModel` (UI) 的单向数据流。
- **模块化解耦**：将 UI 拆分为 HUD1, HUD2, Tray，将业务拆分为 ScoutService, SniperService。

## 2. 针对原计划的优化调整

经过三方讨论，对 `REFACTOR_PLAN.md` 做出以下优化调整：

1.  **MainLoop 位置**：确定将核心循环放在 `src/app/dispatcher.rs` 或新文件 `src/app/mod.rs` 中，保持 `main.rs` 尽可能简洁（仅负责初始化、DPI 感知和顶层错误捕获）。
2.  **ViewModel 优化**：`ViewModel` 将实现 `Clone` 和 `Default`。UI 组件在接收到更新时，应比对关键字段以减少不必要的重绘（尤其是 HUD1 的文本和 HUD2 的显示状态）。
3.  **坐标同步机制**：`ViewModel` 必须包含 LCU 窗口的 `Rect` 坐标以及 `zoom_scale`。UI Agent 负责根据这些数据实时调整 HUD2 的偏移量。
4.  **事件去重**：`AppEvent` 在发送到 `MainLoop` 前，由产生者负责初步过滤（例如 WebSocket 推送的重复 Phase），减轻 `MainLoop` 负担。
5.  **Graceful Shutdown**：在 `AppEvent` 中增加 `Quit` 信号，确保托盘退出时能干净地关闭所有后台 Service 和钩子。

## 3. 任务分配

- **Infrastructure Agent**: 负责 `AppEvent`, `ViewModel` 定义，以及 `MainLoop` 的脚手架搭建。
- **Logic Agent**: 负责 `ScoutService`, `SniperService` 的提取，以及 `RuntimeState` 的精简。
- **UI Agent**: 负责 `HUD1`, `HUD2`, `Tray` 的物理分离，以及基于 `ViewModel` 的重绘逻辑。

---
记录人: Main Agent (Gemini CLI)
