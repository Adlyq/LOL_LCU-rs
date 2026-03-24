# LOL LCU 助手架构重构计划书 (精修版)

## 1. 核心设计原则

- **被动观察者 (Pure Observer)**：助手不干预客户端 UI，所有状态切换必须以 LCU WebSocket 推送的 `GameflowPhase` 为准。
- **单向数据流 (MVVM)**：`LCU Event -> MainLoop (决策) -> AppState (更新) -> ViewModel (快照) -> UI (重绘)`。
- **职责彻底分离**：UI 线程只负责“画图”，不持有业务逻辑，不直接发起网络请求。

## 2. 模块化拆分方案 (物理文件)

### 2.1 基础架构 (Infrastructure)

- `src/app/event.rs`: 统一定义 `AppEvent` 枚举。包含：`LcuPhase(Phase)`, `LcuUpdate(Path, Value)`, `Tick(u32)`, `HotKey(F1)`, `UserClick(ID)`。
- `src/app/viewmodel.rs`: 定义 UI 专属的只读快照。包括：`hud1_text`, `hud2_visible`, `countdown_secs`, `prophet_list`。

### 2.2 核心中枢 (Logic Center)

- `src/app/main_loop.rs`:
  - 维持一个 `mpsc` 接收器，作为所有事件的终点。
  - 维护一个 `tokio::sync::watch` 发送器，作为所有 UI 的起点。
  - 拥有对 `AppState` 的写权限。

### 2.3 窗口组件 (UI Components)

- `src/win/hud1.rs`: 左上角信息面板。显示：连接状态、对局分析、倒计时文字。
- `src/win/hud2.rs`: 板凳席交互层。绘制：槽位、绿色遮罩。发送：`AppEvent::UserClick(SlotID)`。
- `src/win/tray.rs`: 托盘菜单。发送：配置修改事件或手动触发事件。

## 3. 细化的生命周期管理流程 (Observer 模式)

| 阶段              | 助手行为 (MainLoop 决策)                                                                            | UI 反馈 (HUD1/HUD2)                                                 |
| :---------------- | :-------------------------------------------------------------------------------------------------- | :------------------------------------------------------------------ |
| **1. 启动**       | 开启 LCU 探测任务。                                                                                 | HUD1: "等待连接..."                                                 |
| **2. 客户端运行** | 建立 WS 连接，同步当前阶段。                                                                        | HUD1: "连接成功 - 状态: {Phase}"                                    |
| **3. 匹配成功**   | 探测到 `ReadyCheck`。启动 `AcceptTimer`。                                                           | HUD1: "对局就绪! {n}s 后自动接受"                                   |
| **4. 进入选人**   | 1. 识别阶段为 `ChampSelect`。<br>2. 开启 `ScoutService(我方)`。<br>3. 开启抢人监听。                | HUD1: "分析中..."<br>HUD2: **显现**，准备交互。                     |
| **5. 加载界面**   | 1. 识别阶段为 `InProgress` (加载中)。<br>2. 开启 `ScoutService(双方)`。<br>3. **关闭 HUD2**。       | HUD1: 刷新显示 10 人评分报表。<br>HUD2: **隐藏**。                  |
| **6. 游戏中**     | 1. 识别已正式进入游戏。<br>2. 启动 `HideTimer(120s)`。                                              | HUD1: "已进入游戏 - {n}s 后自动隐藏"                                |
| **7. 隐藏触发**   | `HideTimer` 归零。                                                                                  | HUD1: **淡出/隐藏** (数据保留，F1可唤起)。                          |
| **8. 游戏结束**   | 1. 探测到 `EndOfGame`。<br>2. 识别到 `auto_honor` 开启，**发送静默点赞 API**。                      | HUD1: **唤起**，显示 "对局结束，已自动跳过荣誉点赞"。               |
| **9. 回到房间**   | 1. 探测到 `Lobby` (通过 WS 监听到用户在客户端点了 [再来一局])。<br>2. **调用 `reset_all_data()`**。 | HUD1: 清空评分，显示 "已回到房间，等待下一局"。<br>HUD2: 保持隐藏。 |

## 4. 实时倒计时管理 (Tick 机制)

- **TimerService**:
  - 任何需要倒计时的逻辑 (Accept, Hide, Reload) 都向 `MainLoop` 注册。
  - `MainLoop` 每秒产生一个 `AppEvent::Tick`。
  - ViewModel 更新 `countdown` 字段。
  - UI 自动重绘数字。

## 5. 模块拆分细则 (针对 champ_select.rs)

- **Parser**: 纯 JSON 提取器，输出 `SimplifiedSession` 结构。
- **ScoutService**: 并发战绩查询服务，返回 `AnalysisReport`。
- **SniperService**: 独立循环任务，执行 `PATCH /champ-select/bench/swap`。
- **InteractionManager**: 映射 UI 坐标到槽位 ID。

## 6. 实施路线图

### 阶段一：建立事件基础设施

- [ ] 定义 `AppEvent` 和 `ViewModel` 结构。
- [ ] 实现 `MainLoop` 的基本 `select!` 循环。
- [ ] 将 WebSocket 消息接入 `MainLoop`。

### 阶段二：剥离 UI 逻辑

- [ ] 将 `overlay.rs` 的绘制代码搬迁至 `hud1.rs` 和 `hud2.rs`。
- [ ] 实现 `ViewModel` 的观察者更新机制。

### 阶段三：重构业务服务

- [ ] 剥离 `Prophet/Premade` 逻辑到 `ScoutService`。
- [ ] 剥离抢人逻辑到 `SniperService`。
- [ ] 实现全生命周期的状态自动转换。

## 7. 极其详细的代码级优化步骤 (Step-by-Step)

### 第一步：构建全新的核心总线 (Event & ViewModel)

1. **创建 `src/app/event.rs`**：
   - 定义 `enum AppEvent`。涵盖：`LcuPhaseChanged(String)`, `LcuSessionUpdated(Value)`, `TrayAction(TrayAction)`, `BenchClick(usize)`, `HotKeyF1`, `Tick(u32)`, `ScoutResult(String, String)`。
2. **创建 `src/app/viewmodel.rs`**：
   - 定义 `struct ViewModel`：包含 `hud1_visible: bool`, `hud1_texts: Vec<String>`, `show_bench: bool`, `selected_bench_slot: Option<usize>`, `window_rect: Rect`。
3. **改造 `src/app/state.rs`**：
   - 清理所有与 UI 相关的冗余逻辑（比如现有的 `active_pick_slot`），将其纯粹化为“业务数据的权威来源”。
4. **重写 `src/main.rs` 的循环 (`MainLoop`)**：
   - 移除原来的 `overlay_tx` 和零散的 `tokio::spawn`。
   - 建立唯一的事件管道：`let (event_tx, mut event_rx) = mpsc::channel(1024);`。
   - 建立 UI 广播通道：`let (vm_tx, vm_rx) = watch::channel(ViewModel::default());`。
   - 核心的大 `match` 块：在 `event_rx.recv()` 时，根据不同的 `AppEvent` 修改 `AppState`，并在处理完后调用 `vm_tx.send(new_vm_snapshot)`。

### 第二步：肢解 `src/win/overlay.rs` (UI 解耦)

1. **抽离底层基础 (`src/win/base.rs`)**：
   - 将注册窗口类 (`RegisterClassW`) 和创建窗口 (`CreateWindowExW`) 提取为通用工厂函数。
   - 将 `WH_KEYBOARD_LL` 钩子提取，并在捕获 F1 时向 `event_tx` 发送 `AppEvent::HotKeyF1`。
2. **重构托盘 (`src/win/tray.rs`)**：
   - 迁移 `tray_wnd_proc` 和 `show_tray_menu`。点击菜单后，直接 `event_tx.send(AppEvent::TrayAction(...))`。
3. **重构 HUD1 (`src/win/hud1.rs`)**：
   - 持有一个 `watch::Receiver<ViewModel>`。在独立线程的消息循环中，定时（或被信号唤醒）读取最新的 ViewModel。
   - 将原有的 `paint_hud` 逻辑迁入，只需根据 `ViewModel.hud1_texts` 无脑绘制文字，无需再判断任何状态。
4. **重构 HUD2 (`src/win/hud2.rs`)**：
   - 迁移 `bench_wnd_proc`。在收到 `WM_LBUTTONDOWN` 时，计算击中哪个槽位，并发送 `event_tx.send(AppEvent::BenchClick(idx))`。
   - `paint_bench` 根据 `ViewModel.show_bench` 和 `ViewModel.selected_bench_slot` 画绿色遮罩和槽位框。

### 第三步：重构 `champ_select.rs` 与业务服务

1. **数据解析分离**：
   - 将 `handle_champ_select` 退化为一个纯分发器。它将 LCU Session JSON 转化为简化的 `BenchUpdate` 结构，发给主循环。
2. **战绩抓取服务 (`ScoutService`)**：
   - 从原有的巨型 `tokio::spawn` 块中剥离。设计为一个独立的 Worker，接收需要查询的 `puuid` 列表，使用 `JoinSet` 并发查询，将最终的排版字符串通过 `AppEvent::ScoutResult` 送回主循环。
3. **抢人循环服务 (`BenchSniperService`)**：
   - 将 `src/app/handlers/overlay_click.rs` 中的 `loop_pick_until_refresh` 变成一个状态机。它监听 LCU 状态更新，只要目标英雄还在板凳上且未在自己手里，就以固定频率发送 `/swap` 请求。一旦完成或被取消，通知主循环清除 ViewModel 中的高亮。

### 第四步：重构后台任务 (`src/app/tasks.rs`)

1. **倒计时服务 (`TimerService`)**：
   - 新增一个简单的循环任务：`loop { sleep(1s); event_tx.send(AppEvent::Tick); }`。由主循环在收到 `Tick` 时，针对存在的定时器（如进游戏2分钟隐藏倒计时）做减法，并更新 ViewModel。
2. **窗口修复**：
   - 不再直接通过 `OverlayCmd` 命令 UI。而是将检测到的 `zoom_scale` 作为事件发给主循环，主循环决策调用 `winapi::fix_lcu_window_by_zoom()`，并同步更新 `ViewModel.window_rect`，触发 UI 对齐。
