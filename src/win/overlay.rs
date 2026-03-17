//! Overlay 窗口
//!
//! 对应 Python `OverlayWindow`（PySide6 QWidget）。
//!
//! Rust 实现采用纯 Win32 API 创建透明分层窗口（Layered Window）：
//! - 样式：`WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`
//! - 绘制：GDI `UpdateLayeredWindow` — 半透明容器框 + 槽位高亮；
//! - 点击穿透：`WM_NCHITTEST` — 槽位内返回 `HTCLIENT`，外部返回 `HTTRANSPARENT`；
//! - 点击回调：`WM_LBUTTONDOWN` → 计算槽位索引 → 通过 tokio mpsc 发给业务层；
//! - 位置同步：每 120ms 跟随 LCU 窗口。
//!
//! 指令类型见 [`OverlayCmd`]。

use std::ffi::OsStr;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::thread;

use tracing::{debug, info, warn};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ── 布局常量（对应 Python 模板坐标）─────────────────────────────

/// 模板分辨率宽度（px）
const TEMPLATE_W: f64 = 1920.0;
/// 模板分辨率高度（px）
const TEMPLATE_H: f64 = 1080.0;
/// 模板 bench 容器矩形 left
const BENCH_L: f64 = 528.0;
/// 模板 bench 容器矩形 top
const BENCH_T: f64 = 14.0;
/// 模板 bench 容器矩形 right
const BENCH_R: f64 = 1392.0;
/// 模板 bench 容器矩形 bottom
const BENCH_B: f64 = 90.0;
/// 模板单个槽位边长（px）
const SLOT_SIZE: f64 = 70.0;
/// Bench 最大席位数（固定为 10，与大乱斗展示一致）
const BENCH_SLOT_COUNT: usize = 10;

// ── 托盘常量 ─────────────────────────────────────────────────────

/// 托盘通知回调消息（WM_USER+100，避免与系统消息冲突）
const WM_TRAY_ICON: u32 = WM_USER + 100;
/// 托盘图标 ID（同一进程唯一即可）
const TRAY_UID: u32 = 1;
/// 托盘菜单"退出"项的命令 ID
const IDM_QUIT: usize = 1001;
/// 托盘菜单"热重载客户端"项的命令 ID
const IDM_RELOAD_UX: usize = 1002;
/// 托盘菜单"退出结算页面"项的命令 ID
const IDM_PLAY_AGAIN: usize = 1003;/// 托盘菜单“领取任务与宝箱”项的命令 ID
const IDM_AUTO_LOOT: usize = 1004;
// ── 指令类型 ─────────────────────────────────────────────────────

/// 发送给 Overlay 线程的指令（tokio mpsc）。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum OverlayCmd {
    /// 显示 overlay 窗口
    Show,
    /// 隐藏 overlay 窗口
    Hide,
    /// 更新 bench 英雄 ID 列表
    SetBenchIds(Vec<i64>),
    /// 清除选中槽位高亮
    ClearSelectedSlot,
    /// 更新选中槽位高亮
    SetSelectedSlot(usize),
    /// 触发窗口自动修复（zoom_scale）
    AutoFixWindow(f64),
    /// 退出消息循环
    Quit,
}

/// 托盘菜单动作（overlay 线程 → tokio 的单向事件）。
#[derive(Debug, Clone)]
pub enum TrayAction {
    /// 热重载 LCU 客户端 UX（不断开排队 / 游戏连接）
    ReloadUx,
    /// 退出结算界面，返回大厅
    PlayAgain,
    /// 手动领取已完成任务奖励 + 开启免费宝箱
    AutoLoot,
}

// ── Overlay 线程启动 ──────────────────────────────────────────────

/// 启动 Overlay 后台线程（Win32 消息循环），返回 tokio mpsc 发送端。
///
/// 点击回调通过 `click_tx`（tokio::sync::mpsc）发送到 tokio 侧。
pub fn spawn_overlay_thread(
    click_tx: tokio::sync::mpsc::Sender<usize>,
) -> (
    tokio::sync::mpsc::Sender<OverlayCmd>,
    tokio::sync::mpsc::Receiver<TrayAction>,
) {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<OverlayCmd>(256);
    let (tray_tx, tray_rx) = tokio::sync::mpsc::channel::<TrayAction>(32);

    thread::Builder::new()
        .name("overlay-win32".to_owned())
        .spawn(move || {
            overlay_message_loop(cmd_rx, click_tx, tray_tx);
        })
        .expect("启动 overlay 线程失败");

    (cmd_tx, tray_rx)
}

// ── 几何计算（与 Python 实现完全对应）───────────────────────────

/// 浮点矩形
#[derive(Clone, Copy, Debug, Default)]
struct FRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl FRect {
    fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    fn right(&self) -> f64 { self.x + self.w }
    fn bottom(&self) -> f64 { self.y + self.h }
}

/// 将模板矩形缩放到实际窗口坐标（窗口相对）。
///
/// `win_w`/`win_h`：overlay 窗口当前宽高（即 LCU 窗口宽高）。
fn bench_container_rect(win_w: i32, win_h: i32) -> FRect {
    let scale_x = win_w as f64 / TEMPLATE_W;
    let scale_y = win_h as f64 / TEMPLATE_H;
    FRect {
        x: BENCH_L * scale_x,
        y: BENCH_T * scale_y,
        w: (BENCH_R - BENCH_L) * scale_x,
        h: (BENCH_B - BENCH_T) * scale_y,
    }
}

/// 计算指定槽位在窗口坐标中的矩形（两端对齐，等间距）。
///
/// 对应 Python `_slot_rect()`。
fn slot_rect(index: usize, slot_count: usize, container: FRect, win_w: i32, win_h: i32) -> FRect {
    let scale_x = win_w as f64 / TEMPLATE_W;
    let scale_y = win_h as f64 / TEMPLATE_H;
    let scale = f64::min(scale_x, scale_y);

    let slot_w = SLOT_SIZE * scale;
    let slot_h = SLOT_SIZE * scale;
    let edge_inset = f64::max(0.0, 1.5 * scale);
    let avail_w = f64::max(1.0, container.w - 2.0 * edge_inset);

    let gap = if slot_count <= 1 {
        0.0
    } else {
        f64::max(0.0, (avail_w - slot_w * slot_count as f64) / (slot_count - 1) as f64)
    };

    let x = container.x + edge_inset + index as f64 * (slot_w + gap);
    let y = container.y + (container.h - slot_h) / 2.0;
    FRect { x, y, w: slot_w, h: slot_h }
}

/// 根据窗口相对坐标 `(px, py)` 判断点击的槽位索引（无命中返回 `None`）。
fn hit_slot(px: f64, py: f64, slot_count: usize, win_w: i32, win_h: i32) -> Option<usize> {
    if slot_count == 0 {
        return None;
    }
    let container = bench_container_rect(win_w, win_h);
    if !container.contains(px, py) {
        return None;
    }
    for i in 0..slot_count {
        if slot_rect(i, slot_count, container, win_w, win_h).contains(px, py) {
            return Some(i);
        }
    }
    None
}

// ── GDI 辅助 ─────────────────────────────────────────────────────

/// 用 GDI 在内存 DC 上绘制 overlay 内容，然后通过 `UpdateLayeredWindow` 推送。
///
/// - 灰色边框：容器矩形 + 各槽位
/// - 选中槽位：半透明绿色填充 `(130,255,130,65)`
/// - 其余区域：近透明（alpha = 1）以捕捉鼠标消息
unsafe fn paint_overlay(
    hwnd: HWND,
    win_w: i32,
    win_h: i32,
    slot_count: usize,
    selected: Option<usize>,
) {
    if win_w <= 0 || win_h <= 0 {
        return;
    }

    let hdc_screen = GetDC(HWND::default());
    if hdc_screen.is_invalid() {
        return;
    }
    let hdc_mem = CreateCompatibleDC(hdc_screen);
    if hdc_mem.is_invalid() {
        ReleaseDC(HWND::default(), hdc_screen);
        return;
    }

    // 创建 32bpp ARGB DIB
    let bi = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: win_w,
        biHeight: -win_h, // 顶-下
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let bi_info = BITMAPINFO {
        bmiHeader: bi,
        bmiColors: [RGBQUAD::default(); 1],
    };

    let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    let hbm = match CreateDIBSection(hdc_mem, &bi_info, DIB_RGB_COLORS, &mut bits_ptr, HANDLE::default(), 0) {
        Ok(h) if !h.is_invalid() => h,
        _ => {
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(HWND::default(), hdc_screen);
            return;
        }
    };

    let old_bm = SelectObject(hdc_mem, hbm);

    // 填充全透明背景（alpha=0）
    let total = (win_w * win_h * 4) as usize;
    std::ptr::write_bytes(bits_ptr as *mut u8, 0u8, total);

    // ── 在 32bpp DIB 上直接写像素（premultiplied ARGB）──
    let pixels = std::slice::from_raw_parts_mut(bits_ptr as *mut u32, (win_w * win_h) as usize);

    // 预乘 ARGB 像素
    #[inline(always)]
    fn premul(r: u8, g: u8, b: u8, a: u8) -> u32 {
        let af = a as f64 / 255.0;
        let pr = (r as f64 * af) as u8;
        let pg = (g as f64 * af) as u8;
        let pb = (b as f64 * af) as u8;
        ((a as u32) << 24) | ((pr as u32) << 16) | ((pg as u32) << 8) | (pb as u32)
    }

    // ── 圆角矩形：全局 SDF + 覆盖率 AA ──────────────────────────
    //
    // 使用完整圆角矩形 Signed Distance Field，直线段和弧线共享同一
    // coverage 公式，从根本上保证视觉粗细一致、无断裂、无锯齿。
    //
    //   fill  : cov = clamp(0.5 - dist, 0, 1)
    //   border: cov = clamp(1.0 - |dist|, 0, 1)   ← 1px AA 描边，任意处相同

    /// 圆角矩形 SDF（负=内部，0=边缘，正=外部）。
    #[inline(always)]
    fn sdf_rrect(px: f64, py: f64, cx: f64, cy: f64, hw: f64, hh: f64, rad: f64) -> f64 {
        let qx = (px - cx).abs() - (hw - rad);
        let qy = (py - cy).abs() - (hh - rad);
        qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - rad
    }

    /// 写入 AA 像素（premultiplied ARGB，cov ∈ (0,1]）
    #[inline(always)]
    fn write_aa(pixels: &mut [u32], win_w: i32, win_h: i32,
                px: i32, py: i32, r: u8, g: u8, b: u8, base_a: u8, cov: f64) {
        if (px as u32) >= (win_w as u32) || (py as u32) >= (win_h as u32) { return; }
        let a = ((base_a as f64) * cov) as u8;
        if a > 0 { pixels[(py * win_w + px) as usize] = premul(r, g, b, a); }
    }

    /// AA 圆角矩形填充（角部 SDF，中间行 scanline 加速）
    #[inline(always)]
    fn fill_rounded(pixels: &mut [u32], win_w: i32, win_h: i32,
                    rect: FRect, rad: i32, r: u8, g: u8, b: u8, a: u8) {
        let hw = rect.w * 0.5;
        let hh = rect.h * 0.5;
        let cx = rect.x + hw;
        let cy = rect.y + hh;
        let radf = (rad as f64).min(hw).min(hh).max(0.0);
        let solid = premul(r, g, b, a);
        let y0 = (rect.y as i32 - 1).max(0);
        let y1 = (rect.bottom() as i32).min(win_h - 1);
        let x0 = (rect.x as i32 - 1).max(0);
        let x1 = (rect.right() as i32).min(win_w - 1);
        let band   = radf as i32 + 1;
        let mid_y0 = rect.y as i32 + band;
        let mid_y1 = (rect.bottom() as i32 - 1) - band;
        let ix0 = rect.x as i32;
        let ix1 = (rect.right() as i32 - 1).min(win_w - 1);
        for py in y0..=y1 {
            let yf = py as f64 + 0.5;
            if py >= mid_y0 && py <= mid_y1 {
                // 中间行：直线边无角影响，直接写满
                for px in ix0.max(0)..=ix1 {
                    pixels[(py * win_w + px) as usize] = solid;
                }
            } else {
                // 顶/底角部行：SDF per pixel
                for px in x0..=x1 {
                    let d = sdf_rrect(px as f64 + 0.5, yf, cx, cy, hw, hh, radf);
                    if d < -0.5 {
                        pixels[(py * win_w + px) as usize] = solid;
                    } else if d < 0.5 {
                        write_aa(pixels, win_w, win_h, px, py, r, g, b, a, 0.5 - d);
                    }
                }
            }
        }
    }

    /// AA 圆角矩形描边（SDF，直线段与弧线使用完全相同的 coverage 公式）
    #[inline(always)]
    fn draw_rounded_border(pixels: &mut [u32], win_w: i32, win_h: i32,
                           rect: FRect, rad: i32, r: u8, g: u8, b: u8, a: u8) {
        let hw = rect.w * 0.5;
        let hh = rect.h * 0.5;
        let cx = rect.x + hw;
        let cy = rect.y + hh;
        let radf = (rad as f64).min(hw).min(hh).max(0.0);
        let y0 = (rect.y as i32 - 1).max(0);
        let y1 = (rect.bottom() as i32).min(win_h - 1);
        let x0 = (rect.x as i32 - 1).max(0);
        let x1 = (rect.right() as i32).min(win_w - 1);
        // 中间行只扫左右各 3px 条带，跳过内部像素
        let band   = radf as i32 + 2;
        let mid_y0 = rect.y as i32 + band;
        let mid_y1 = (rect.bottom() as i32 - 1) - band;
        let lx1 = (rect.x as i32 + 2).min(x1);
        let rx0 = (rect.right() as i32 - 3).max(x0);
        for py in y0..=y1 {
            let yf = py as f64 + 0.5;
            if py >= mid_y0 && py <= mid_y1 {
                // 中间行：左右各 3px 边缘条带
                for px in x0..=lx1 {
                    let d = sdf_rrect(px as f64 + 0.5, yf, cx, cy, hw, hh, radf);
                    let cov = (1.0 - d.abs()).clamp(0.0, 1.0);
                    if cov > 0.0 { write_aa(pixels, win_w, win_h, px, py, r, g, b, a, cov); }
                }
                for px in rx0..=x1 {
                    let d = sdf_rrect(px as f64 + 0.5, yf, cx, cy, hw, hh, radf);
                    let cov = (1.0 - d.abs()).clamp(0.0, 1.0);
                    if cov > 0.0 { write_aa(pixels, win_w, win_h, px, py, r, g, b, a, cov); }
                }
            } else {
                // 顶/底行及角部：全行扫描
                for px in x0..=x1 {
                    let d = sdf_rrect(px as f64 + 0.5, yf, cx, cy, hw, hh, radf);
                    let cov = (1.0 - d.abs()).clamp(0.0, 1.0);
                    if cov > 0.0 { write_aa(pixels, win_w, win_h, px, py, r, g, b, a, cov); }
                }
            }
        }
    }

    if slot_count > 0 {
        let container = bench_container_rect(win_w, win_h);
        // 圆角半径随屏幕缩放（1080p 下容器 5px、槽位 4px）
        let scale_y = win_h as f64 / TEMPLATE_H;
        let container_r = (5.0 * scale_y).round() as i32;
        let slot_r     = (4.0 * scale_y).round() as i32;

        // 容器背景（alpha=2，近透明，但足以捕获鼠标）
        fill_rounded(pixels, win_w, win_h, container, container_r, 0, 0, 0, 2);
        // 容器灰色边框
        draw_rounded_border(pixels, win_w, win_h, container, container_r, 128, 128, 128, 220);

        // 各槽位
        for i in 0..slot_count {
            let sr = slot_rect(i, slot_count, container, win_w, win_h);
            if selected == Some(i) {
                fill_rounded(pixels, win_w, win_h, sr, slot_r, 130, 255, 130, 65);
            } else {
                fill_rounded(pixels, win_w, win_h, sr, slot_r, 0, 0, 0, 2);
            }
            draw_rounded_border(pixels, win_w, win_h, sr, slot_r, 160, 160, 160, 220);
        }
    }

    // 获取窗口左上角屏幕坐标
    let pt_src = POINT::default();
    let sz = SIZE { cx: win_w, cy: win_h };
    let mut pt_dst = POINT::default();
    let mut wr = RECT::default();
    if GetWindowRect(hwnd, &mut wr).is_ok() {
        pt_dst.x = wr.left;
        pt_dst.y = wr.top;
    }

    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let _ = UpdateLayeredWindow(
        hwnd,
        hdc_screen,
        Some(&pt_dst),
        Some(&sz),
        hdc_mem,
        Some(&pt_src),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );

    // 清理
    SelectObject(hdc_mem, old_bm);
    let _ = DeleteObject(hbm);
    let _ = DeleteDC(hdc_mem);
    ReleaseDC(HWND::default(), hdc_screen);
}

// ── Win32 窗口过程共享状态（thread_local）───────────────────────

/// 窗口过程可访问的状态（存储在 thread_local，由 GWLP_USERDATA 指向）
struct WndState {
    slot_count: usize,
    selected_slot: Option<usize>,
    win_w: i32,
    win_h: i32,
    /// 向 tokio 发送槽位点击事件
    click_tx: tokio::sync::mpsc::Sender<usize>,
    /// 向 tokio 发送托盘菜单动作
    tray_tx: tokio::sync::mpsc::Sender<TrayAction>,
}

/// Win32 窗口过程
unsafe extern "system" fn overlay_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            // 默认先穿透
            let mut result = HTTRANSPARENT as isize;

            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndState;
            if !ptr.is_null() {
                let state = &*ptr;
                // 鼠标屏幕坐标
                let sx = (lparam.0 & 0xFFFF) as i16 as i32;
                let sy = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                // 转为窗口相对坐标
                let mut wr = RECT::default();
                if GetWindowRect(hwnd, &mut wr).is_ok() {
                    let px = (sx - wr.left) as f64;
                    let py = (sy - wr.top) as f64;
                    if hit_slot(px, py, state.slot_count, state.win_w, state.win_h).is_some() {
                        result = HTCLIENT as isize;
                    }
                }
            }
            LRESULT(result)
        }

        WM_LBUTTONDOWN => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndState;
            if !ptr.is_null() {
                let state = &*ptr;
                let cx = (lparam.0 & 0xFFFF) as i16 as i32;
                let cy = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if let Some(idx) = hit_slot(cx as f64, cy as f64, state.slot_count, state.win_w, state.win_h) {
                    debug!("Overlay 点击槽位 {idx}");
                    let _ = state.click_tx.try_send(idx);
                }
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }

        // 托盘图标通知回调
        WM_TRAY_ICON => {
            let event = (lparam.0 as u32) & 0xFFFF;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                // 构建右键菜单
                let hmenu = CreatePopupMenu();
                if let Ok(hmenu) = hmenu {
                    let play_again_text = to_wide("退出结算页面");
                    let _ = AppendMenuW(hmenu, MF_STRING, IDM_PLAY_AGAIN, windows::core::PCWSTR(play_again_text.as_ptr()));
                    let reload_text = to_wide("热重载客户端");
                    let _ = AppendMenuW(hmenu, MF_STRING, IDM_RELOAD_UX, windows::core::PCWSTR(reload_text.as_ptr()));
                    let auto_loot_text = to_wide("领取任务与宝箱");
                    let _ = AppendMenuW(hmenu, MF_STRING, IDM_AUTO_LOOT, windows::core::PCWSTR(auto_loot_text.as_ptr()));
                    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, windows::core::PCWSTR(std::ptr::null()));
                    let quit_text = to_wide("退出");
                    let _ = AppendMenuW(hmenu, MF_STRING, IDM_QUIT, windows::core::PCWSTR(quit_text.as_ptr()));

                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    // SetForegroundWindow 确保菜单在失焦后能自动关闭（托盘经典做法）
                    let _ = SetForegroundWindow(hwnd);
                    let cmd = TrackPopupMenu(
                        hmenu,
                        TPM_RETURNCMD | TPM_RIGHTBUTTON,
                        pt.x,
                        pt.y,
                        0,
                        hwnd,
                        None,
                    );
                    // 修复 SetForegroundWindow 后台化的任务栏 bug
                    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
                    let _ = DestroyMenu(hmenu);

                    let cmd_id = cmd.0 as usize;
                    if cmd_id == IDM_PLAY_AGAIN {
                        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WndState;
                        if !ptr.is_null() {
                            let _ = (*ptr).tray_tx.try_send(TrayAction::PlayAgain);
                        }
                    } else if cmd_id == IDM_RELOAD_UX {
                        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WndState;
                        if !ptr.is_null() {
                            let _ = (*ptr).tray_tx.try_send(TrayAction::ReloadUx);
                        }
                    } else if cmd_id == IDM_AUTO_LOOT {
                        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WndState;
                        if !ptr.is_null() {
                            let _ = (*ptr).tray_tx.try_send(TrayAction::AutoLoot);
                        }
                    } else if cmd_id == IDM_QUIT {
                        // 对应 Python `app.quit()`：直接退出进程，
                        // Windows 会自动清理托盘图标
                        std::process::exit(0);
                    }
                }
            }
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ── Win32 字符串辅助 ──────────────────────────────────────────────

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(once(0)).collect()
}

// ── Win32 消息循环（在独立线程运行）──────────────────────────────

fn overlay_message_loop(
    mut cmd_rx: tokio::sync::mpsc::Receiver<OverlayCmd>,
    click_tx: tokio::sync::mpsc::Sender<usize>,
    tray_tx: tokio::sync::mpsc::Sender<TrayAction>,
) {
    use std::time::{Duration, Instant};
    use crate::win::winapi;

    info!("Overlay 线程已启动（Win32 消息循环）");

    // ── 状态 ────────────────────────────────────────────
    let mut bench_ids: Vec<i64> = Vec::new();
    let mut selected_slot: Option<usize> = None;
    let mut visible = false;
    let mut target_hwnd: Option<HWND> = None;
    let mut overlay_hwnd: Option<HWND> = None;
    let mut tray_added = false;      // 托盘图标是否已注册
    let mut last_sync = Instant::now();
    let mut last_redraw = Instant::now();

    // 固定 30Hz（~33ms/frame），overlay 非核心显示，无需高刷
    let frame_duration = Duration::from_millis(33);

    let auto_redraw_enabled = std::env::var("OVERLAY_AUTO_REDRAW")
        .map(|v| v != "0" && v.to_lowercase() != "false")
        .unwrap_or(true);
    let redraw_interval = Duration::from_secs_f64(
        std::env::var("OVERLAY_AUTO_REDRAW_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.5_f64),
    );
    let sync_interval = Duration::from_millis(120);

    // ── 注册窗口类并创建分层窗口 ────────────────────────
    let class_name = to_wide("LcuOverlayClass");
    let hwnd_result: Result<HWND, _> = unsafe {
        let hinstance = GetModuleHandleW(None)
            .map(|h| windows::Win32::Foundation::HINSTANCE(h.0))
            .unwrap_or_default();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overlay_wnd_proc),
            hInstance: hinstance,
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc); // 忽略重复注册错误

        let win_name = to_wide("LcuOverlay");
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            windows::core::PCWSTR(class_name.as_ptr()),
            windows::core::PCWSTR(win_name.as_ptr()),
            WS_POPUP,
            0, 0, 1920, 1080, // 初始尺寸，稍后由位置同步更新
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
    };

    match hwnd_result {
        Ok(hwnd) if !hwnd.is_invalid() => {
            info!("Overlay Win32 窗口已创建 hwnd={hwnd:?}");
            overlay_hwnd = Some(hwnd);

            // 将 WndState 分配到堆上，通过 GWLP_USERDATA 挂载
            let state = Box::new(WndState {
                slot_count: BENCH_SLOT_COUNT,
                selected_slot: None,
                win_w: 1920,
                win_h: 1080,
                click_tx: click_tx.clone(),
                tray_tx: tray_tx.clone(),
            });
            let state_ptr = Box::into_raw(state);
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
            }

            // ── 添加系统托盘图标 ─────────────────────────
            unsafe { tray_added = add_tray_icon(hwnd); }
        }
        Err(e) => {
            warn!("创建 Overlay 窗口失败: {e}，将以无 GUI 模式运行");
        }
        _ => {
            warn!("创建 Overlay 窗口返回无效句柄，将以无 GUI 模式运行");
        }
    }

    // ── 主循环 ──────────────────────────────────────────
    loop {
        // ── 处理来自 tokio 的指令 ──────────────────────
        let mut needs_repaint = false;
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => match cmd {
                    OverlayCmd::Show => {
                        visible = true;
                        needs_repaint = true;
                        debug!("Overlay: Show");
                        if let Some(hwnd) = overlay_hwnd {
                            unsafe { let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE); }
                        }
                    }
                    OverlayCmd::Hide => {
                        visible = false;
                        selected_slot = None;
                        bench_ids.clear();
                        needs_repaint = true;
                        debug!("Overlay: Hide");
                        if let Some(hwnd) = overlay_hwnd {
                            unsafe { let _ = ShowWindow(hwnd, SW_HIDE); }
                        }
                    }
                    OverlayCmd::SetBenchIds(ids) => {
                        bench_ids = ids;
                        needs_repaint = true;
                        debug!("Overlay: SetBenchIds({:?})", bench_ids);
                    }
                    OverlayCmd::ClearSelectedSlot => {
                        selected_slot = None;
                        needs_repaint = true;
                        debug!("Overlay: ClearSelectedSlot");
                    }
                    OverlayCmd::SetSelectedSlot(idx) => {
                        selected_slot = Some(idx);
                        needs_repaint = true;
                        debug!("Overlay: SetSelectedSlot({idx})");
                    }
                    OverlayCmd::AutoFixWindow(zoom) => {
                        if let Some(hwnd) = target_hwnd {
                            winapi::fix_lcu_window_by_zoom(hwnd, zoom, false);
                        }
                    }
                    OverlayCmd::Quit => {
                        info!("Overlay: Quit");
                        // 先移除托盘图标，再销毁窗口
                        if let Some(hwnd) = overlay_hwnd {
                            unsafe {
                                if tray_added {
                                    remove_tray_icon(hwnd);
                                }
                                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndState;
                                if !ptr.is_null() {
                                    drop(Box::from_raw(ptr));
                                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                                }
                                let _ = DestroyWindow(hwnd);
                            }
                        }
                        return;
                    }
                },
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    info!("Overlay 指令通道已关闭，退出线程");
                    if let Some(hwnd) = overlay_hwnd {
                        unsafe {
                            if tray_added {
                                remove_tray_icon(hwnd);
                            }
                            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndState;
                            if !ptr.is_null() {
                                drop(Box::from_raw(ptr));
                                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                            }
                            let _ = DestroyWindow(hwnd);
                        }
                    }
                    return;
                }
            }
        }

        // ── 同步 LCU 窗口位置 ──────────────────────────
        let now = Instant::now();
        if now.duration_since(last_sync) >= sync_interval {
            last_sync = now;
            target_hwnd = winapi::find_lcu_window();

            if let (Some(lcu), Some(ov)) = (target_hwnd, overlay_hwnd) {
                if let Some(rect) = winapi::get_window_rect(lcu) {
                    let new_w = rect.right - rect.left;
                    let new_h = rect.bottom - rect.top;

                    // 更新 WndState 中的尺寸
                    unsafe {
                        let ptr = GetWindowLongPtrW(ov, GWLP_USERDATA) as *mut WndState;
                        if !ptr.is_null() {
                            let state = &mut *ptr;
                            let size_changed = state.win_w != new_w || state.win_h != new_h;
                            state.win_w = new_w;
                            state.win_h = new_h;
                            state.slot_count = BENCH_SLOT_COUNT; // 固定 10 个席位
                            state.selected_slot = selected_slot;
                            if size_changed { needs_repaint = true; }
                        }
                    }

                    // 将 overlay 放置在 LCU 窗口正上方
                    if visible {
                        winapi::place_window_above_target(ov, lcu, &rect);
                    }
                }
            }

            if auto_redraw_enabled
                && target_hwnd.is_some()
                && now.duration_since(last_redraw) >= redraw_interval
            {
                winapi::auto_redraw_lcu_window(target_hwnd.unwrap());
                last_redraw = now;
            }
        }

        // ── 重绘 overlay ────────────────────────────────
        // 注意：直接使用本地 selected_slot，而非 state.selected_slot
        // state.selected_slot 仅在 120ms 间隔的 sync 块里更新，
        // 若用它会导致 ClearSelectedSlot/SetSelectedSlot 到达后
        // 立即重绘时仍读到旧值，造成绿色遮罩滞留 120ms+。
        if needs_repaint {
            if let Some(hwnd) = overlay_hwnd {
                unsafe {
                    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WndState;
                    if !ptr.is_null() {
                        let state = &*ptr;
                        paint_overlay(hwnd, state.win_w, state.win_h, state.slot_count, selected_slot);
                    }
                }
            }
        }

        // ── Win32 消息泵 ───────────────────────────────
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    info!("Overlay 收到 WM_QUIT");
                    return;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // 每帧休眠，间隔跟随系统刷新率
        thread::sleep(frame_duration);
    }
}

// ── 托盘图标辅助 ──────────────────────────────────────────────────

/// 向系统注册托盘图标，返回是否成功。
///
/// - 图标优先从 exe 内嵌资源（resource ID 1，由 build.rs + winres 编译）加载，
///   失败时降级到系统默认图标 `IDI_APPLICATION`。
/// - Tooltip 固定为 `"LOL_LCU"`（对应 Python `tray.setToolTip("LOL_LCU")`）。
unsafe fn add_tray_icon(hwnd: HWND) -> bool {
    let hinstance = GetModuleHandleW(None)
        .map(|h| HINSTANCE(h.0))
        .unwrap_or_default();

    // 加载内嵌图标（winres 默认资源 ID = 1）
    let hicon = LoadIconW(hinstance, windows::core::PCWSTR(1usize as *const u16))
        .or_else(|_| LoadIconW(HINSTANCE::default(), IDI_APPLICATION))
        .unwrap_or_default();

    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY_ICON;
    nid.hIcon = hicon;

    // Tooltip（最多 127 个 UTF-16 码元 + 空终止符）
    let tip = "LOL_LCU";
    for (i, c) in tip.encode_utf16().enumerate() {
        if i >= 127 {
            break;
        }
        nid.szTip[i] = c;
    }

    let ok = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
    if ok {
        info!("系统托盘图标已添加");
    } else {
        warn!("添加系统托盘图标失败（Shell_NotifyIconW NIM_ADD）");
    }
    ok
}

/// 从系统注销托盘图标。
unsafe fn remove_tray_icon(hwnd: HWND) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}
