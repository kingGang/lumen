//! Lumen 主程序：winit 事件循环，组装 PTY → 终端状态机 → 渲染器 → egui 外壳。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod action;
mod background;
/// 文件路径补全逻辑引擎（M4.4 批1）：token 提取 + 路径枚举，纯逻辑无 egui 依赖。
#[cfg(feature = "input-editor")]
mod completion;
/// 命令补全 sidecar 进程管理（M4.4 批2）：持久 pwsh 进程 + JSON 协议 + 异步响应唤醒。
#[cfg(feature = "input-editor")]
mod completion_sidecar;
/// footer 输入区视图组装（M4.1 批C，feature = "input-editor"）——设计稿 §7.1。
#[cfg(feature = "input-editor")]
mod composer;
/// footer 鼠标事件处理：像素→Position、click-count 状态机、词/行选区（第十一轮）。
#[cfg(feature = "input-editor")]
mod footer_mouse;
/// 命令历史库（M4.1 批D2，feature = "input-editor"）——设计稿 §8。
#[cfg(feature = "input-editor")]
mod history;
mod i18n;
mod input;
mod keymap;
mod mode;
mod profile;
mod session;
mod sessions_store;
mod settings;
mod shell;
mod single_instance;
// M3.8 批2 Snap Layouts 子类化（仅 Windows）。
#[cfg(target_os = "windows")]
mod snap_layouts;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{error, info};
use lumen_pty::PtyEvent;
use lumen_renderer::{wgpu, Renderer};
use lumen_term::{SelPoint, Selection};
use session::{Session, SessionId, Tab, TabId, MAX_PANES};
use shell::layout::{DividerKind, PaneLayout};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Icon, Window, WindowId};
// M3.8 自绘标题栏：Windows 平台扩展（无边框阴影 / 圆角）。
#[cfg(target_os = "windows")]
use winit::platform::windows::WindowAttributesExtWindows;

/// scrollback 容量（行）。
const SCROLLBACK: usize = 10_000;

/// 渲染静默窗口（trailing debounce）：每批 PTY 数据都把渲染往后
/// 推这么久，只有数据流静默后才上屏。TUI 程序一帧重绘往往分多次
/// write 到达，且帧尾光标常停在临时位置、之后才补「移回输入框」
/// 的序列——必须等整组数据到齐再画，否则光标/半成品行会闪烁。
/// 对打字回显是无感的延迟。
const REDRAW_DEBOUNCE: Duration = Duration::from_millis(5);
/// 最低刷新保障：数据持续不断（大量输出）时静默窗口会被一直推后，
/// 自首批未渲染数据起最多等这么久就强制渲染一次（约 30fps）。
const REDRAW_HARD_CAP: Duration = Duration::from_millis(33);
/// 绝对兜底：强制渲染时刻若恰处于 DEC 2026 同步区间会小步顺延等
/// 帧完成，但等待不超过该时长（防应用卡死在 BSU 画面冻结）。
const REDRAW_ABS_CAP: Duration = Duration::from_millis(100);
/// 光标「帧尾未归位」冻结的超时：ESU 后应用迟迟不发「显示光标」
/// 归位序列时，超过该时长就信任当前位置。
/// 打字光标走同行近距直通不受此值影响，它只兜「跨行大跳」
/// （动画残留位）——经实战验证 50ms 能盖住 codex 归位批的延迟，
/// 调小到 10ms 时 ESU 直渲下残留位会在超时后漏画（闪烁回归）。
const CURSOR_FREEZE_CAP: Duration = Duration::from_millis(50);
/// 后台会话单次 wake 的消化字节上限：`advance()` 在主线程跑，后台
/// `yes` 级输出不限量会抢占主线程拖慢前台打字。超限的事件留在**本
/// 会话自己的通道**里（靠 bounded 容量反压该会话的读线程，不连坐
/// 其他会话），并补发一个 wake 下轮继续消化。
const BG_DRAIN_CAP: usize = 256 * 1024;

/// 自定义事件：PTY 有新输出待处理（去重信号，数据在 channel 里）。
///
/// 不直接携带数据：主循环收到信号后一次 drain 全部积压字节再渲染，
/// 避免把 TUI 重绘的中间状态（光标游走、半成品行）画到屏幕上。
#[derive(Debug)]
struct PtyWake;

/// 从 PNG 字节流解码并构造 winit 窗口图标。
///
/// 解码失败（格式损坏、尺寸越界）时返回 `None` 并打印 warn，
/// 不 panic——图标是视觉增强，缺失不影响功能。
///
/// # Examples
///
/// ```no_run
/// let icon = load_icon(include_bytes!("../../../icons/lumen-icon-32.png"));
/// // icon 可能为 None（损坏）；正常情况下为 Some(Icon)
/// ```
fn load_icon(bytes: &[u8]) -> Option<Icon> {
    let img = match image::load_from_memory(bytes) {
        Ok(i) => i.into_rgba8(),
        Err(e) => {
            log::warn!("窗口图标解码失败，跳过设置：{e}");
            return None;
        }
    };
    let (width, height) = img.dimensions();
    match Icon::from_rgba(img.into_raw(), width, height) {
        Ok(icon) => Some(icon),
        Err(e) => {
            log::warn!("构造窗口 Icon 失败，跳过设置：{e}");
            None
        }
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // F8 单实例限制（事件循环创建前检测）：release 默认单开——已有
    // 实例在跑时通知其前台化、本实例静默退出；debug 构建与
    // --multi-instance / LUMEN_MULTI_INSTANCE=1 放行多开。
    // `instance` 持有命名互斥量，必须存活到 main 结束（单实例锁覆盖
    // 整个运行期）。
    let instance = single_instance::acquire();
    if matches!(instance, single_instance::InstanceCheck::AlreadyRunning) {
        info!("已有 Lumen 实例在运行，已通知其前台化，本实例退出");
        return Ok(());
    }
    let event_loop = EventLoop::<PtyWake>::with_user_event()
        .build()
        .context("创建事件循环失败")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    // 第一实例：起前台化监听线程（第二实例 SetEvent → 置标志 + 借
    // PtyWake 唤醒主循环，见 single_instance 模块文档）。
    if let single_instance::InstanceCheck::Primary(guard) = &instance {
        single_instance::spawn_foreground_listener(guard, proxy.clone());
    }
    let mut app = App { proxy, state: None };
    event_loop.run_app(&mut app).context("事件循环异常退出")?;
    Ok(())
}

struct App {
    proxy: EventLoopProxy<PtyWake>,
    state: Option<AppState>,
}

/// PTY 原始字节的人类可读转义表示（LUMEN_DUMP_PTY 取证设施，B3）。
///
/// 格式规则：
/// - 可打印 ASCII（0x20..=0x7e）原样输出；
/// - CR(`\r`)→`<CR>`、LF(`\n`)→`<LF>\n`（保留换行让文本文件可读）；
/// - ESC（0x1b）后跟 `[`：完整 CSI 序列以 `<ESC[...终止符>` 表示；
/// - ESC 后跟 `]`：完整 OSC 序列以 `<OSC...ST>` 表示（含 BEL/ST 终止）；
/// - 其余控制字符以 `<XX>` 十六进制表示。
fn dump_pty_readable(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            // 可打印 ASCII 原样输出
            0x20..=0x7e => {
                out.push(b as char);
                i += 1;
            }
            // CR
            b'\r' => {
                out.push_str("<CR>");
                i += 1;
            }
            // LF：保留一个真换行让 .txt 文件可读
            b'\n' => {
                out.push_str("<LF>\n");
                i += 1;
            }
            // BEL
            0x07 => {
                out.push_str("<BEL>");
                i += 1;
            }
            // BS
            0x08 => {
                out.push_str("<BS>");
                i += 1;
            }
            // ESC 序列
            0x1b => {
                let next = bytes.get(i + 1).copied();
                match next {
                    // CSI：ESC [ ... 终止符（0x40..=0x7e）
                    Some(b'[') => {
                        let start = i;
                        i += 2; // 跳过 ESC [
                        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                            i += 1;
                        }
                        if i < bytes.len() {
                            i += 1; // 包含终止符
                        }
                        out.push_str("<ESC");
                        for &c in &bytes[start + 1..i] {
                            if (0x20..=0x7e).contains(&c) {
                                out.push(c as char);
                            } else {
                                out.push_str(&format!("\\x{c:02x}"));
                            }
                        }
                        out.push('>');
                    }
                    // OSC：ESC ] ... BEL(0x07) 或 ST(ESC \)
                    Some(b']') => {
                        let start = i;
                        i += 2; // 跳过 ESC ]
                        loop {
                            if i >= bytes.len() {
                                break;
                            }
                            if bytes[i] == 0x07 {
                                i += 1; // BEL 终止
                                break;
                            }
                            // ST = ESC \
                            if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                        out.push_str("<OSC");
                        for &c in &bytes[start + 2..i] {
                            if c == 0x07 || (c == b'\\' && i > 0) {
                                break;
                            }
                            if (0x20..=0x7e).contains(&c) {
                                out.push(c as char);
                            } else {
                                out.push_str(&format!("\\x{c:02x}"));
                            }
                        }
                        out.push_str("...ST>");
                    }
                    // 其它 ESC 序列（ESC x）
                    Some(c) => {
                        out.push_str(&format!("<ESC{}>", c as char));
                        i += 2;
                    }
                    None => {
                        out.push_str("<ESC>");
                        i += 1;
                    }
                }
            }
            // 其余控制字符
            c => {
                out.push_str(&format!("<{c:02X}>"));
                i += 1;
            }
        }
    }
    out
}

/// 窗格 drain 的轮询顺序：激活 tab 的焦点窗格最先（回显延迟对排队
/// 最敏感），其余激活 tab 窗格次之（可见、正在渲染），最后是后台
/// tab 的窗格。`pane_counts` 为各 tab 的窗格数，`active_tab` /
/// `focused` 为激活 tab 与其焦点窗格下标；抽成纯函数便于单测，
/// 下标越界（防御）时退化为纯下标序。
fn drain_order(pane_counts: &[usize], active_tab: usize, focused: usize) -> Vec<(usize, usize)> {
    let mut order = Vec::with_capacity(pane_counts.iter().sum::<usize>());
    if let Some(&n) = pane_counts.get(active_tab) {
        if focused < n {
            order.push((active_tab, focused));
        }
        order.extend((0..n).filter(|&p| p != focused).map(|p| (active_tab, p)));
    }
    for (t, &n) in pane_counts.iter().enumerate() {
        if t == active_tab {
            continue;
        }
        order.extend((0..n).map(|p| (t, p)));
    }
    order
}

/// 面板宽度写盘判定（P10；B1 抽成纯函数加单测）：指针松开后的实际
/// 宽度 `actual` 在合法范围 ±1 容差内、且与已存值 `stored` 差 ≥1 逻辑
/// 像素才值得写盘——窗口过窄被临时压缩到范围外的瞬态宽度不写（重启
/// 还原用户最后一次主动调整的值），亚像素抖动不写（避免每帧白写）。
/// NaN/Inf（防御）一律不写。
fn width_worth_persisting(actual: f32, stored: f32, min: f32, max: f32) -> bool {
    (min - 1.0..=max + 1.0).contains(&actual) && (actual - stored).abs() >= 1.0
}

/// 最大化时窗口四边超出工作区的物理像素越界量（纯函数，单测友好）。
///
/// # 机理（M3.8 / 第十轮问题1）
///
/// Windows 无边框窗口（`WS_THICKFRAME` + `WM_NCCALCSIZE` 铺满客户区）
/// 最大化时，系统把窗口 outer rect 向四周各扩约 8px，使粗边框恰好
/// 隐藏在屏幕外——这是 VSCode/Chromium 等无边框应用的标准行为，
/// 俗称「隐形边框」。egui 按完整 `inner_size` 布局，四边各 ~8px 画在
/// 屏幕外，右/下贴边内容被裁。
///
/// 本函数比较窗口矩形与显示器工作区矩形，计算各边超出量（物理像素）：
/// - `left`  = `work.left  - win.left`（win 比 work 更偏左时为正）
/// - `top`   = `work.top   - win.top`
/// - `right` = `win.right  - work.right`（win 右端超出 work 时为正）
/// - `bottom`= `win.bottom - work.bottom`
///
/// 非最大化时窗口在工作区内，各边差值 ≤ 0，函数返回全零（0,0,0,0）。
/// 跨显示器负坐标（副显示器在主屏左侧）由 i32 算术自然处理。
///
/// # 参数
/// - `win`  : `(left, top, right, bottom)` 窗口 outer rect 物理像素（屏幕坐标）。
/// - `work` : `(left, top, right, bottom)` 显示器工作区物理像素（屏幕坐标）。
///
/// # 返回
/// `(left, top, right, bottom)` 各边越界量（物理像素，最小 0）。
fn maximized_overflow(
    win: (i32, i32, i32, i32),
    work: (i32, i32, i32, i32),
) -> (i32, i32, i32, i32) {
    let left = (work.0 - win.0).max(0);
    let top = (work.1 - win.1).max(0);
    let right = (win.2 - work.2).max(0);
    let bottom = (win.3 - work.3).max(0);
    (left, top, right, bottom)
}

/// 查询当前窗口相对所在显示器工作区的四边越界量（物理像素）。
///
/// 仅在 Windows + 最大化时实际调用；非最大化时直接返回 `(0,0,0,0)`
/// 以避免不必要的 Win32 调用。失败时静默返回 `(0,0,0,0)`（退化安全）。
///
/// # 实现说明
/// - `GetWindowRect` 取窗口 outer rect（含不可见 THICKFRAME 部分）。
/// - `MonitorFromWindow(MONITOR_DEFAULTTONEAREST)` 取所在（或最近）显示器。
/// - `GetMonitorInfoW` 取该显示器的工作区（rcWork，不含任务栏）。
/// - 二者差值由 [`maximized_overflow`] 纯函数计算（便于单测）。
// ALLOW: 此函数第十轮引入，第十一轮已确认无需在运行路径中调用（见上方注释），
// 但保留供将来如需平台相关调试用。单测覆盖靠 maximized_overflow 纯函数。
#[cfg(target_os = "windows")]
#[allow(dead_code)]
fn query_maximized_overflow(hwnd: windows_sys::Win32::Foundation::HWND) -> (i32, i32, i32, i32) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;

    // SAFETY: hwnd 由 winit 创建并由调用方保证在消息循环期间有效；
    // 两个 RECT 均以 zeroed 初始化，API 成功时完整填写，失败时
    // 我们检查返回值并返回 (0,0,0,0)，不读取未初始化内存。
    unsafe {
        let mut win_rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut win_rect) == 0 {
            return (0, 0, 0, 0);
        }

        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if hmon.is_null() {
            return (0, 0, 0, 0);
        }

        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: std::mem::zeroed(),
            rcWork: std::mem::zeroed(),
            dwFlags: 0,
        };
        if GetMonitorInfoW(hmon, &mut mi) == 0 {
            return (0, 0, 0, 0);
        }

        let win = (win_rect.left, win_rect.top, win_rect.right, win_rect.bottom);
        let work = (
            mi.rcWork.left,
            mi.rcWork.top,
            mi.rcWork.right,
            mi.rcWork.bottom,
        );
        maximized_overflow(win, work)
    }
}

/// 恢复路径各窗格的初始内容区估算（B2 修复，抽成纯函数加单测）。
///
/// spawn 发生在首帧 egui 布局之前，旧实现给所有窗格统一按**整个
/// 终端区**估算行列——多窗格布局下首帧要做腰斩级缩行 resize，恰
/// 与 shell 打印首个提示符的时间窗重叠：ConPTY/PSReadLine 跨
/// resize 的差量重绘按陈旧坐标落格，是 B2 症状②「提示符丢字 +
/// 回显错位混叠」的温床；缩行擦除则联动症状①。这里用与 shell
/// 首帧完全相同的布局引擎按还原权重预切窗格矩形、再扣窗格标题栏
/// 占高，估算与首帧实际值的偏差只剩面板像素级出入（行列 ±1 级），
/// 首帧 resize 从「腰斩」降为「微调或无」。
///
/// `area` 为终端工作区估算（逻辑点）；`maximized` 窗格按独占整区
/// 计算，其余窗格仍按布局矩形——还原最大化时回到布局矩形，届时
/// resize 近似无损。返回各窗格内容区物理像素 (宽, 高)，顺序与窗格
/// 一致；布局与 n 不符（防御）时按均分计算。
fn estimate_restored_pane_px(
    area: egui::Rect,
    layout: &PaneLayout,
    n: usize,
    maximized: Option<usize>,
    scale: f32,
) -> Vec<(u32, u32)> {
    let rects = if layout.pane_count() == n {
        layout.pane_rects(area)
    } else {
        PaneLayout::uniform(n).pane_rects(area)
    };
    rects
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let r = match maximized {
                Some(m) if m == i => area,
                _ => r,
            };
            // 与 shell/mod.rs 的窗格标题栏占高同源（极矮窗格防御：
            // 最多占一半高）。
            let title_h = shell::PANE_TITLE_HEIGHT.min(r.height() / 2.0);
            let w = (r.width() * scale).round().max(1.0) as u32;
            let h = ((r.height() - title_h) * scale).round().max(1.0) as u32;
            (w, h)
        })
        .collect()
}

struct AppState {
    /// 性能埋点输出（LUMEN_PERF=<路径> 启用）。
    perf: Option<std::fs::File>,
    perf_t0: Instant,
    /// 最近一帧（任何内容）的渲染时刻：事件驱动重绘的 8ms 合帧下限
    /// 以它为基准（整帧负载视角，UI 帧与终端帧都算）。
    last_render_at: Option<Instant>,
    /// 最近一次**终端离屏**真正渲染的时刻：ESU 直渲的 8ms 限频以它
    /// 为基准。与 last_render_at 分开——鼠标驱动的纯 UI 重绘（同步
    /// 区间内跳过终端渲染）不该反向推迟 ESU 完成帧的上屏。
    last_term_render_at: Option<Instant>,
    window: Arc<Window>,
    renderer: Renderer,
    /// 全部 tab（每 tab 1~6 个终端窗格 = [`Session`]，见 [`Tab`]）。
    /// 至少一个；最后一个关闭即退出应用。
    tabs: Vec<Tab>,
    /// 激活 tab 在 `tabs` 中的下标。
    active_tab: usize,
    /// 会话（窗格）id 自增分配器（关闭不回收；通道随会话销毁，残留
    /// 事件无需按 id 过滤）。
    next_session_id: SessionId,
    /// tab id 自增分配器（同上，关闭不回收）。
    next_tab_id: TabId,
    /// 与转发线程共享的「wake 已挂起」标志，用于事件去重（全局一个，
    /// 任一会话的转发线程都可触发，唤醒协议与单会话时代零变化）。
    wake_pending: Arc<AtomicBool>,
    /// 事件循环唤醒句柄（补发 wake / 新建会话的转发线程用）。
    proxy: EventLoopProxy<PtyWake>,
    /// 应用设置（设置页编辑的数据源；变更即写盘）。
    settings: settings::Settings,
    /// 系统当前是否深色模式（P12 Sync with OS）：启动时取
    /// `window.theme()`、运行中由 `WindowEvent::ThemeChanged` 维护；
    /// 开启跟随时主题按它在深/浅槽位间解析。
    os_dark: bool,
    /// 最近一次写盘的会话列表快照（F4 持久化去重：cwd 上报/结构
    /// 变更都先与它比对，无变化不重复写盘）。None = 本次运行尚未写。
    last_sessions_snapshot: Option<sessions_store::SessionsFile>,
    /// 分隔条拖动改过比例、尚未确认落盘（B1 加固）：正常由
    /// drag_stopped → divider_drag_ended 触发落盘，但 egui 的拖动结束
    /// 事件在边角场景可能错失（拖动中窗口失焦/指针态被清）。指针
    /// 松开的帧看到此标志即兜底落盘（快照一致时内部自动跳过），
    /// 保证「拖过的比例一定进盘」。
    layout_dirty: bool,
    /// 启动首帧的「外壳布局实际应用值」日志已输出（B1 恢复面验收：
    /// 只凭加载日志不能证明 UI 真用了持久化值，首帧布局后把实际
    /// 侧栏/文件树宽与窗格权重打进日志，一次性）。
    layout_apply_logged: bool,
    /// 登录档案（mock）：None = 未登录。顶栏头像、头像菜单、设置页
    /// Account 三处 UI 同源此字段；登录写盘 / 登出删盘（profile.json）。
    profile: Option<profile::Profile>,
    modifiers: ModifiersState,
    clipboard: Option<arboard::Clipboard>,
    /// 最近一次按键时刻（端到端延迟埋点用，跟随激活会话即可）。
    last_key_at: Option<Instant>,
    /// 鼠标最近一次的窗口内像素位置。
    mouse_pos: (f64, f64),

    // —— egui 外壳 ——
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    /// 各窗格离屏纹理的 egui 句柄（键 = 会话 id；离屏重建后原地
    /// 换绑，id 不变）。窗格关闭时移入 [`Self::pending_tex_free`]。
    pane_textures: HashMap<SessionId, egui::TextureId>,
    /// 待注销的 egui 纹理 id：窗格关闭动作可能发生在 run_ui 之后
    /// （本帧 shape 仍引用该纹理），推迟到帧呈现后统一 free。
    pending_tex_free: Vec<egui::TextureId>,
    /// 激活 tab 各窗格的矩形（会话 id, 物理像素 x/y/w/h），来自最近
    /// 一帧 egui 布局（鼠标命中/IME 候选框定位用）。tab 结构变更后
    /// 的陈旧条目按 id 解析不到窗格、自然失效。
    pane_rects_px: Vec<(SessionId, (f32, f32, f32, f32))>,
    /// 各窗格右上角关闭按钮的命中矩形（物理像素 x/y/w/h；仅多窗格
    /// 时非空），来自最近一帧 egui 布局。raw 鼠标路由对它让位：✕ 的
    /// 点击由 egui 处理（pane_close 动作），按下不聚焦/不建选区。
    pane_close_rects_px: Vec<(f32, f32, f32, f32)>,
    /// 各分隔条命中矩形（物理像素 x/y/w/h；F7③），来自最近一帧
    /// egui 布局。raw 鼠标路由对它让位：按下不聚焦/不建选区、不交出
    /// 终端焦点（调完比例接着打字不该断流），拖动与双击由 egui 侧
    /// 处理（divider_drag / divider_reset）。
    divider_rects_px: Vec<(f32, f32, f32, f32)>,
    /// 侧栏/文件树栏拖宽手柄的命中矩形（物理像素 x/y/w/h；P10），
    /// 来自最近一帧 egui 布局。文件树右缘的手柄向终端区探入数像素，
    /// raw 鼠标路由对它让位（与分隔条同款：按下不聚焦/不建选区/不
    /// 交出终端焦点，拖宽由 egui 面板处理）。
    panel_resize_rects_px: Vec<(f32, f32, f32, f32)>,
    /// 终端是否持有键盘/IME 焦点：点击终端区 true、点击 egui 面板
    /// false。egui 不会为非控件区域持焦点，键盘与 IME 路由全靠它。
    terminal_focused: bool,
    /// egui 主动要求的下次重绘时刻（动画等），about_to_wait 里与
    /// 终端渲染计划合流取 min。事件驱动重绘触发过密（<8ms）时也
    /// 合并进此计划（见 window_event 入口的合帧下限）。
    egui_repaint_at: Option<Instant>,
    /// 上一帧是否有 egui 弹层（右键菜单/头像菜单等 Popup）打开。
    /// 用于检测「弹层关闭」边沿，按关闭方式仲裁焦点归属。
    was_popup_open: bool,
    /// 外壳 UI 的跨帧状态（重命名编辑等）。
    shell_state: shell::ShellState,
    /// B3-8：整窗 resize 事件（WindowEvent::Resized）已到达、等待本帧
    /// RedrawRequested 处理。置位时 divider_resize_held 门控对所有窗格
    /// **无效**——整窗 resize 是 OS 级行为，与分隔条拖动完全独立；
    /// 若拖动状态因焦点/指针事件丢失未被 egui 正常清除，此标志保证
    /// window resize 的 term/PTY resize 不被永久卡住。每帧 RedrawRequested
    /// 处理完窗格 resize 后即清零（单次消耗）。
    window_just_resized: bool,
    /// 背景图纹理（P13）：已成功加载时为 Some，未启用/加载失败时为 None。
    /// egui 层在终端工作区底部绘制；关闭时 free 旧纹理防泄漏。
    bg_texture: Option<background::BgTexture>,
    /// 经典直通模式开关（M4.1 批B，Ctrl+Shift+E 切换）。
    ///
    /// 置位后 [`mode::effective_mode`] 强制返回 [`mode::InputMode::Fallback`]，
    /// 所有按键直通 PTY（= M2 现状）。设计稿 §2「手动逃生」。
    /// **禁止在此字段之外的地方保存输入模式副本**（设计稿铁律）。
    force_fallback: bool,
    /// 命令历史库（M4.1 批D2）：启动时加载，提交时追加写，退出时原子重写。
    /// feature = "input-editor" 门控（Fallback/无 feature 时历史功能禁用）。
    #[cfg(feature = "input-editor")]
    history: history::HistoryStore,
    /// ghost text 缓存（M4.1 批3）：(编辑器 revision, 联想后缀)。
    /// revision 变化时重算；不变时复用上帧结果，避免每帧遍历历史库。
    /// feature = "input-editor" 门控。
    #[cfg(feature = "input-editor")]
    ghost_cache: (u64, Option<String>),
    /// 补全弹窗候选列表（M4.4 批1 + 批2）：Tab 键触发后存入，render 时构造 CompletionView。
    /// feature = "input-editor" 门控。
    #[cfg(feature = "input-editor")]
    completion_candidates: Vec<completion::Completion>,
    /// 命令补全 sidecar 进程管理器（M4.4 批2）。
    /// feature = "input-editor" 门控。
    #[cfg(feature = "input-editor")]
    completion_sidecar: completion_sidecar::CompletionSidecar,
    /// 当前在途的 sidecar 请求 id（M4.4 批2）：用于丢弃过期响应。
    /// 0 = 无在途请求（保留无效值，request 从 1 开始分配）。
    /// feature = "input-editor" 门控。
    #[cfg(feature = "input-editor")]
    completion_req_id: u64,

    // ── footer 鼠标状态机（第十一轮，feature = "input-editor"）──────────
    /// footer 区域的鼠标是否正在拖选中（左键按住未松开）。
    #[cfg(feature = "input-editor")]
    footer_dragging: bool,
    /// footer 拖选锚点（按下时记录，松开前不变）。
    #[cfg(feature = "input-editor")]
    footer_drag_anchor: lumen_editor::Position,
    /// click-count 状态机（单击/双击/三击）。
    #[cfg(feature = "input-editor")]
    footer_click_state: footer_mouse::ClickState,
    /// 右键菜单请求（含菜单弹出的窗口物理像素位置）；Some 时由 egui 帧弹出菜单。
    #[cfg(feature = "input-editor")]
    footer_context_menu_at: Option<(f64, f64)>,
}

/// 提交文本编码为 PTY 载荷（M4.1 批D1/D2）——设计稿 §3.2 步骤 2。
///
/// - 单行：`text + "\r"`
/// - 多行：**无条件**用 `"\x1b[200~" + text + "\x1b[201~\r"` 括号粘贴包裹。
///
/// # 关于多行无条件包裹（6e9635b 实测核验，D2 拍板）
///
/// 原设计草案依赖 `term.bracketed_paste()` 查询决定是否包裹，但实测
/// `term.bracketed_paste()` 始终为 `false`（PSReadLine 未发送 DEC 2004h 声明）。
/// 实测证明：PSReadLine 不声明 bracketed paste，但**确实正确处理** ESC[200~...ESC[201~
/// 序列——将其作为一整块不触发 `>>` 续行。因此改为**无条件**包裹多行：
/// 无论 `term.bracketed_paste()` 返回何值，多行提交始终用 200~/201~ 包装。
/// `bracketed_paste()` 的返回值仅作日志/取证参考，不再影响提交路径。
///
/// 此纯函数无副作用，可独立单测。
#[cfg(feature = "input-editor")]
fn encode_submit(text: &str) -> Vec<u8> {
    let line_count = text.lines().count();
    if line_count > 1 {
        // 多行：无条件括号粘贴包裹（见函数文档，6e9635b 实测核验）。
        let mut buf = Vec::with_capacity(text.len() + 14);
        buf.extend_from_slice(b"\x1b[200~");
        buf.extend_from_slice(text.as_bytes());
        buf.extend_from_slice(b"\x1b[201~\r");
        buf
    } else {
        let mut buf = text.as_bytes().to_vec();
        buf.push(b'\r');
        buf
    }
}

/// 将 app 层 [`action::EditAction`] 转换为 `lumen_editor::EditAction`（M4.1 批D1）。
///
/// 两个枚举结构相同；M4 阶段 app 层将直接引用 lumen_editor 的类型，届时此函数删除。
///
/// # Errors
/// 本函数不返回 Result；任何映射失败（两枚举不同步）在编译期即可发现。
#[cfg(feature = "input-editor")]
fn app_to_editor_action(ea: &action::EditAction) -> lumen_editor::EditAction {
    use action::{EditAction as AEa, Motion as AMotion};
    use lumen_editor::EditAction as EEa;
    use lumen_editor::Motion as EMotion;

    /// 将 app 层 Motion 转为 editor 层 Motion。
    fn to_motion(m: &AMotion) -> EMotion {
        match m {
            AMotion::GraphemeLeft => EMotion::GraphemeLeft,
            AMotion::GraphemeRight => EMotion::GraphemeRight,
            AMotion::WordLeft => EMotion::WordLeft,
            AMotion::WordRight => EMotion::WordRight,
            AMotion::LineStart => EMotion::LineStart,
            AMotion::LineEnd => EMotion::LineEnd,
            AMotion::Up => EMotion::Up,
            AMotion::Down => EMotion::Down,
            AMotion::DocStart => EMotion::DocStart,
            AMotion::DocEnd => EMotion::DocEnd,
        }
    }

    match ea {
        AEa::InsertText(s) => EEa::InsertText(s.clone()),
        AEa::InsertNewline => EEa::InsertNewline,
        AEa::DeleteBackward => EEa::DeleteBackward,
        AEa::DeleteForward => EEa::DeleteForward,
        AEa::DeleteWordBackward => EEa::DeleteWordBackward,
        AEa::Move { motion, extend } => EEa::Move {
            motion: to_motion(motion),
            extend: *extend,
        },
        AEa::SetSelection(s) => EEa::SetSelection(lumen_editor::Selection {
            anchor: lumen_editor::Position {
                line: s.anchor.line,
                byte: s.anchor.byte,
            },
            cursor: lumen_editor::Position {
                line: s.head.line,
                byte: s.head.byte,
            },
        }),
        AEa::SelectAll => EEa::SelectAll,
        AEa::SetText(s) => EEa::SetText(s.clone()),
        AEa::Undo => EEa::Undo,
        AEa::Redo => EEa::Redo,
        AEa::Clear => EEa::Clear,
    }
}

/// lumen_editor::EditAction → app 层 action::Action 转换（footer 鼠标路径用）。
///
/// 仅转换 footer 鼠标事件实际产生的 EditAction 变体（SetSelection / SelectAll）；
/// 其余变体若意外传入，按 SelectAll 保守兜底（不应发生，加 debug 日志）。
///
/// # Errors
/// 不返回 Result；映射失败以 debug log 标注。
#[cfg(feature = "input-editor")]
fn lumen_editor_action_to_app_action(ea: lumen_editor::EditAction) -> action::Action {
    use action::{Action, EditAction as AEa, Position as APos, Selection as ASel};
    use lumen_editor::EditAction as EEa;

    let app_ea = match ea {
        EEa::SetSelection(s) => AEa::SetSelection(ASel {
            anchor: APos {
                line: s.anchor.line,
                byte: s.anchor.byte,
            },
            head: APos {
                line: s.cursor.line,
                byte: s.cursor.byte,
            },
        }),
        EEa::SelectAll => AEa::SelectAll,
        other => {
            log::debug!("[footer_mouse] 意外的 EditAction: {other:?}，兜底 SelectAll");
            AEa::SelectAll
        }
    };
    Action::Edit(app_ea)
}

impl AppState {
    /// 性能埋点：LUMEN_PERF 启用时写一行带时间戳的记录。
    fn perf_log(&mut self, msg: std::fmt::Arguments<'_>) {
        if let Some(f) = self.perf.as_mut() {
            use std::io::Write;
            let t = self.perf_t0.elapsed().as_millis();
            let _ = writeln!(f, "[{t:>7}ms] {msg}");
        }
    }

    /// 按设置与系统深浅模式应用当前生效主题（P12）：终端配色（含
    /// 行排版缓存失效）+ 外壳 egui 样式联动。设置页主题/槽位/Sync
    /// with OS 变更与系统深浅切换共用此链路。
    fn apply_theme(&mut self) {
        let info = settings::theme_info(self.settings.effective_theme_id(self.os_dark));
        self.renderer.set_theme(info.theme());
        let pal = shell::theme::shell_palette(info);
        // 问题5：将 panel_outline 描边色注入 renderer，更新 footer 上边框颜色。
        let [r, g, b, _] = pal.panel_outline.to_array();
        self.renderer.set_footer_border_color(r, g, b);
        shell::theme::apply_style(&self.egui_ctx, &pal);
        info!("主题已应用：{}（id {}）", info.name, info.id);
    }

    /// 加载或重载背景图纹理（P13）。
    ///
    /// - 启用且有路径：解码图片 → 上传 GPU → 更新 `bg_texture`；
    ///   路径变更时先 free 旧纹理（防泄漏），再加载新纹理。
    /// - 禁用或路径清除：free 旧纹理、置 `bg_texture = None`。
    /// - 加载失败（文件不存在/解码失败/尺寸超限）：toast error，
    ///   `bg_texture` 置 None（本次运行视为未启用，不改写 settings）。
    fn apply_background_image(&mut self) {
        let bg = &self.settings.appearance.background;
        let should_load = bg.enabled && bg.path.is_some();
        let path = bg.path.clone();

        // 先 free 旧纹理（无论是关闭还是换图）。
        if let Some(old) = self.bg_texture.take() {
            self.pending_tex_free.push(old.texture_id);
        }

        if !should_load {
            // 禁用或无路径：关闭透明通路。
            self.renderer.set_transparent_background(false);
            return;
        }

        // should_load 已保证 path.is_some()（第 427 行），此处不可为 None。
        let path_str = path
            .as_deref()
            .expect("path is Some: checked by should_load above");
        match background::load_background_texture(
            path_str,
            self.renderer.device(),
            self.renderer.queue(),
            &mut self.egui_renderer,
        ) {
            Ok(tex) => {
                log::info!("背景图已加载：{path_str} ({}×{})", tex.width, tex.height);
                self.renderer.set_transparent_background(true);
                self.bg_texture = Some(tex);
            }
            Err(e) => {
                log::error!("背景图加载失败：{e}");
                self.shell_state.toast.push(
                    shell::toast::ToastKind::Error,
                    i18n::fmt1(i18n::strings().toast_bg_load_failed_fmt, &e),
                );
                // 加载失败 = 本次运行禁用背景图（不改写 settings）。
                self.renderer.set_transparent_background(false);
                self.window.request_redraw();
            }
        }
    }

    /// 焦点窗格 = 激活 tab 的焦点窗格（键盘/IME/滚轮/选区/粘贴/块
    /// 操作的路由目标；`tabs` 恒非空——空仅出现在退出流程，调用方
    /// 已挡）。
    fn focused_pane(&self) -> &Session {
        self.tabs[self.active_tab].focused_pane()
    }

    /// 焦点窗格（可变）。
    fn focused_pane_mut(&mut self) -> &mut Session {
        self.tabs[self.active_tab].focused_pane_mut()
    }

    /// 唯一状态变更入口（M4.1 批B）——设计稿 §6。
    ///
    /// **凡绕过此方法直接改状态的代码，code review 一律打回。**
    ///
    /// winit 事件处理层只做「事件 → keymap → Action」翻译，不直接碰
    /// editor / pty / term 状态；M4 远程消息反序列化为同一 `Action` 后
    /// 经由此方法执行，保证本地与远端行为一致。
    ///
    /// 批B 实现范围：
    /// - `Term(TermAction)` 完整实现（VT 编码下沉、写 PTY、翻屏、块跳转）。
    /// - `Edit(_)` / `Composer(_)` 仅记 debug log，批D 接编辑器时填充。
    ///
    /// # 返回值
    /// 返回 [`Vec<action::StateEvent>`]，消费方后批接（渲染 / 历史库 /
    /// 状态条 / M4 状态增量同步）。批B 仅返回 `ModeChanged` 和
    /// `FallbackToggled` 事件，其余批次逐步填充。
    fn dispatch(
        &mut self,
        action: action::Action,
        ti: usize,
        pi: usize,
    ) -> Vec<action::StateEvent> {
        use action::{Action, StateEvent, TermAction};

        let mut events = Vec::new();

        match action {
            // ── Edit：M4.1 批D1 —— 发给编辑器状态机 ──────────────────
            #[cfg(feature = "input-editor")]
            Action::Edit(ref ea) => {
                // 双重门控：必须在 Compose 态才走编辑器路径
                let current_mode =
                    mode::effective_mode(&self.tabs[ti].panes[pi].term, self.force_fallback);
                if current_mode == mode::InputMode::Compose {
                    // 任意编辑动作 → 退出历史导航态（设计稿 §8：编辑即回到当前）。
                    // 仅在正在导航时才重置（is_navigating 纯判断，无副作用）。
                    if self.history.is_navigating() {
                        self.history.exit_navigation();
                    }
                    // app 层 EditAction → lumen_editor::EditAction 转换
                    let editor_action = app_to_editor_action(ea);
                    let _outcome = self.tabs[ti].panes[pi].editor.apply(&editor_action);
                    // 编辑器变更驱动 request_redraw，走设计稿 §7.4「节拍纪律」；
                    // 不走 PTY debounce（编辑器修改不触发 pty 写入）。
                    self.window.request_redraw();
                    events.push(StateEvent::EditorRevision(
                        self.tabs[ti].panes[pi].editor.revision(),
                    ));
                } else {
                    log::debug!("[dispatch] Edit({ea:?}) 非 Compose 态（{current_mode:?}）拒绝");
                }
            }
            #[cfg(not(feature = "input-editor"))]
            Action::Edit(ea) => {
                log::debug!("[dispatch] Edit({ea:?}) input-editor feature 未启用，忽略");
            }

            // ── Composer：M4.1 批D1 —— 提交全链路 ────────────────────
            #[cfg(feature = "input-editor")]
            Action::Composer(ref ca) => {
                use action::ComposerAction;
                let current_mode =
                    mode::effective_mode(&self.tabs[ti].panes[pi].term, self.force_fallback);
                match ca {
                    ComposerAction::Submit if current_mode == mode::InputMode::Compose => {
                        // M4.2 批2：续行检测——文档末尾未闭合（引号/括号/here-string/
                        // 块注释、行尾管道 `|` 或续行反引号）时，Enter 自动换行而非提交
                        // （设计稿 §4），复用 lumen-editor tokenizer 判定。
                        if self.tabs[ti].panes[pi].editor.needs_continuation() {
                            self.tabs[ti].panes[pi]
                                .editor
                                .apply(&lumen_editor::EditAction::InsertNewline);
                            events.push(StateEvent::EditorRevision(
                                self.tabs[ti].panes[pi].editor.revision(),
                            ));
                            self.window.request_redraw();
                        } else {
                            // 步骤 1：门控（双重检查，keymap 已检查过一次）
                            // 步骤 2：编码（纯函数，单行 + CR；多行 + 括号粘贴无条件包裹）
                            let raw_text = self.tabs[ti].panes[pi].editor.view().text();
                            let payload = encode_submit(&raw_text);
                            // 步骤 3：滚动到底 + 写 PTY
                            self.tabs[ti].panes[pi].term.grid_mut().scroll_to_bottom();
                            if let Err(e) = self.tabs[ti].panes[pi].write_user_input(&payload) {
                                log::error!("提交写 PTY 失败: {e:#}");
                            }
                            // 步骤 4：清空编辑器缓冲 + 记录 pending_submit + 写历史库
                            let submitted_at = std::time::Instant::now();
                            // 取当前 cwd（OSC 9;9 上报值）
                            let cwd = self.tabs[ti].panes[pi]
                                .term
                                .cwd()
                                .map(|p| p.display().to_string());
                            // 写历史库并取条目下标（用于块闭合时回填）
                            let history_idx = self.history.append_submitted(raw_text.clone(), cwd);
                            // 退出历史导航态（提交 = 新命令基线）
                            self.history.exit_navigation();
                            // 同步 abandoned 到历史库
                            let abandoned = self.tabs[ti].panes[pi]
                                .editor
                                .abandoned()
                                .map(|s| s.to_owned());
                            self.history.set_abandoned(abandoned);
                            self.tabs[ti].panes[pi]
                                .editor
                                .apply(&lumen_editor::EditAction::Clear);
                            // 清 IME preedit
                            self.tabs[ti].panes[pi].preedit = None;
                            // 清退出码角标（提交新命令时角标已无意义）
                            self.tabs[ti].panes[pi].exit_badge = None;
                            self.tabs[ti].panes[pi].pending_submit =
                                Some((raw_text.clone(), submitted_at, history_idx));
                            events.push(StateEvent::SubmittedText {
                                text: raw_text,
                                submitted_at,
                                history_idx,
                            });
                            self.window.request_redraw();
                        }
                    }
                    ComposerAction::CancelLine if current_mode == mode::InputMode::Compose => {
                        // Ctrl+C 缓冲非空：清空并存放弃稿
                        let text = self.tabs[ti].panes[pi].editor.view().text();
                        self.tabs[ti].panes[pi].editor.stash_abandoned(text.clone());
                        // 同步 abandoned 到历史库
                        self.history.set_abandoned(Some(text));
                        self.tabs[ti].panes[pi]
                            .editor
                            .apply(&lumen_editor::EditAction::Clear);
                        self.window.request_redraw();
                        events.push(StateEvent::EditorRevision(
                            self.tabs[ti].panes[pi].editor.revision(),
                        ));
                    }
                    ComposerAction::HistoryPrev if current_mode == mode::InputMode::Compose => {
                        // ↑ 历史向上导航（M4.1 批D2）
                        // 同步 abandoned 到历史库（每次进入导航前刷新）
                        let abandoned = self.tabs[ti].panes[pi]
                            .editor
                            .abandoned()
                            .map(|s| s.to_owned());
                        self.history.set_abandoned(abandoned);
                        let current = self.tabs[ti].panes[pi].editor.view().text();
                        if let Some(text) = self.history.navigate_up(&current) {
                            self.tabs[ti].panes[pi]
                                .editor
                                .apply(&lumen_editor::EditAction::SetText(text));
                            // 光标移到行末（历史条目视觉跟手）
                            self.tabs[ti].panes[pi]
                                .editor
                                .apply(&lumen_editor::EditAction::Move {
                                    motion: lumen_editor::Motion::DocEnd,
                                    extend: false,
                                });
                            self.window.request_redraw();
                            events.push(StateEvent::EditorRevision(
                                self.tabs[ti].panes[pi].editor.revision(),
                            ));
                        }
                    }
                    ComposerAction::HistoryNext if current_mode == mode::InputMode::Compose => {
                        // ↓ 历史向下导航（M4.1 批D2）
                        if let Some(text) = self.history.navigate_down() {
                            self.tabs[ti].panes[pi]
                                .editor
                                .apply(&lumen_editor::EditAction::SetText(text));
                            self.tabs[ti].panes[pi]
                                .editor
                                .apply(&lumen_editor::EditAction::Move {
                                    motion: lumen_editor::Motion::DocEnd,
                                    extend: false,
                                });
                            self.window.request_redraw();
                            events.push(StateEvent::EditorRevision(
                                self.tabs[ti].panes[pi].editor.revision(),
                            ));
                        }
                    }
                    _ => {
                        log::debug!(
                            "[dispatch] Composer({ca:?}) 非 Compose 态（{current_mode:?}）或占位 variant，忽略"
                        );
                    }
                }
            }
            #[cfg(not(feature = "input-editor"))]
            Action::Composer(ca) => {
                log::debug!("[dispatch] Composer({ca:?}) input-editor feature 未启用，忽略");
            }

            // ── Term：本批完整实现 ─────────────────────────────────────
            Action::Term(ta) => match ta {
                TermAction::Interrupt => {
                    if let Err(e) = self.tabs[ti].panes[pi].write_user_input(b"\x03") {
                        log::error!("写入 PTY 失败（Interrupt）: {e:#}");
                    }
                }

                TermAction::SendKey(ks) => {
                    // KeyStroke → winit KeyEvent 的反向转换（批D 可能走此路径）
                    // 批B 暂时通过 PassThrough 路径处理，此分支为 M4 远程预留。
                    log::debug!("[dispatch] SendKey({ks:?}) 暂由 PassThrough 处理");
                }

                TermAction::SendText(text) => {
                    if let Err(e) = self.tabs[ti].panes[pi].write_user_input(text.as_bytes()) {
                        log::error!("写入 PTY 失败（SendText）: {e:#}");
                    }
                }

                TermAction::Scroll(dir) => {
                    let rows = self.tabs[ti].panes[pi].term.grid().rows() as isize;
                    let delta = match dir {
                        action::ScrollDir::Up => rows - 1,
                        action::ScrollDir::Down => -(rows - 1),
                    };
                    self.tabs[ti].panes[pi]
                        .term
                        .grid_mut()
                        .scroll_display(delta);
                    self.window.request_redraw();
                }

                TermAction::JumpBlock(dir) => {
                    if self.tabs[ti].panes[pi].jump_block(dir) {
                        self.window.request_redraw();
                    }
                }

                TermAction::PasteClipboard => {
                    // Compose 态粘贴进编辑器而非 PTY（海风哥第十三轮实测 bug：
                    // keymap 注释早已声明此语义但 dispatch 从未分流，Ctrl+V
                    // 一直直写命令行）。dispatch 内实时查模式（与 Submit 的
                    // 二次门控同理，防按键时刻与执行时刻模式漂移）；编辑器
                    // 路径复用 Edit(InsertText)：替换选区/undo/revision/重绘
                    // 全部继承，多行文本直接落多行编辑（设计稿 §4 Ctrl+V 行）。
                    #[cfg(feature = "input-editor")]
                    {
                        let mode = mode::effective_mode(
                            &self.tabs[ti].panes[pi].term,
                            self.force_fallback,
                        );
                        if mode == mode::InputMode::Compose {
                            let text = self
                                .clipboard
                                .as_mut()
                                .and_then(|c| c.get_text().ok())
                                .unwrap_or_default();
                            if !text.is_empty() {
                                return self.dispatch(
                                    action::Action::Edit(action::EditAction::InsertText(text)),
                                    ti,
                                    pi,
                                );
                            }
                            return Vec::new();
                        }
                    }
                    self.tabs[ti].panes[pi].paste_clipboard(&mut self.clipboard);
                }

                TermAction::CopySelection => {
                    if self.tabs[ti].panes[pi].copy_selection(&mut self.clipboard) {
                        self.tabs[ti].panes[pi].selection = None;
                        self.window.request_redraw();
                    }
                }

                TermAction::CopyBlock => {
                    self.tabs[ti].panes[pi].copy_selected_block(&mut self.clipboard);
                    self.tabs[ti].panes[pi].selected_block = None;
                    self.window.request_redraw();
                }

                TermAction::ScrollToBottom => {
                    self.tabs[ti].panes[pi].term.grid_mut().scroll_to_bottom();
                }

                TermAction::Paste(text) => {
                    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
                    let payload = if self.tabs[ti].panes[pi].term.bracketed_paste() {
                        let mut p = Vec::with_capacity(normalized.len() + 12);
                        p.extend_from_slice(b"\x1b[200~");
                        p.extend_from_slice(normalized.as_bytes());
                        p.extend_from_slice(b"\x1b[201~");
                        p
                    } else {
                        normalized.into_bytes()
                    };
                    if let Err(e) = self.tabs[ti].panes[pi].write_user_input(&payload) {
                        log::error!("写入 PTY 失败（Paste）: {e:#}");
                    }
                }

                TermAction::ToggleFallback => {
                    self.force_fallback = !self.force_fallback;
                    // 第十八轮：同步持久化设置并立即写盘，重启后恢复。
                    // 对齐 language_changed 模式：直接调用 settings.save()，
                    // 失败弹 toast 告知用户（写不进盘不影响终端使用）。
                    self.settings.classic_mode = self.force_fallback;
                    if let Some(err) = self.settings.save() {
                        self.shell_state.toast.push(
                            shell::toast::ToastKind::Error,
                            i18n::fmt1(i18n::strings().toast_settings_save_failed_fmt, &err),
                        );
                    }
                    let s = i18n::strings();
                    let msg = if self.force_fallback {
                        s.toast_fallback_enabled
                    } else {
                        s.toast_fallback_disabled
                    };
                    self.shell_state
                        .toast
                        .push(shell::toast::ToastKind::Info, msg);
                    self.window.request_redraw();
                    events.push(StateEvent::FallbackToggled(self.force_fallback));
                }

                // ── CopyEditorSelection（第十一轮，input-editor feature）──
                // 复制 Compose 态编辑器选区文本到剪贴板。
                // 有选区时复制并 toast；无选区时静默无操作。
                #[cfg(feature = "input-editor")]
                TermAction::CopyEditorSelection => {
                    let view = self.tabs[ti].panes[pi].editor.view();
                    if view.has_selection() {
                        let sel = view.selection();
                        let (start, end) = sel.ordered();
                        // 从各行拼接选区文本
                        let mut text = String::new();
                        for row in start.line..=end.line {
                            let line = view.line(row);
                            let from = if row == start.line { start.byte } else { 0 };
                            let to = if row == end.line {
                                end.byte
                            } else {
                                line.len()
                            };
                            text.push_str(&line[from..to]);
                            if row < end.line {
                                text.push('\n');
                            }
                        }
                        if let Some(cb) = self.clipboard.as_mut() {
                            if let Err(e) = cb.set_text(text.clone()) {
                                log::warn!("复制编辑器选区失败: {e}");
                                self.shell_state.toast.push(
                                    shell::toast::ToastKind::Warn,
                                    i18n::strings().toast_copy_failed,
                                );
                            } else {
                                let preview: String = text.chars().take(40).collect();
                                let preview = if text.len() > preview.len() {
                                    format!("{preview}…")
                                } else {
                                    preview
                                };
                                self.shell_state.toast.push(
                                    shell::toast::ToastKind::Info,
                                    i18n::fmt1(i18n::strings().toast_copied_fmt, preview),
                                );
                            }
                        }
                    }
                }

                // ── CutEditorSelection（第十一轮，input-editor feature）───
                // 剪切 Compose 态编辑器选区：复制 + 删除选区。
                #[cfg(feature = "input-editor")]
                TermAction::CutEditorSelection => {
                    // 先复制（复用同一逻辑）
                    let has_sel = self.tabs[ti].panes[pi].editor.view().has_selection();
                    if has_sel {
                        // 触发内部 CopyEditorSelection 逻辑（递归 dispatch 不合适，内联）
                        // 用块作用域限制 view 借用，确保在访问 clipboard 前已释放。
                        let text = {
                            let view = self.tabs[ti].panes[pi].editor.view();
                            let sel = view.selection();
                            let (start, end) = sel.ordered();
                            let mut t = String::new();
                            for row in start.line..=end.line {
                                let line = view.line(row);
                                let from = if row == start.line { start.byte } else { 0 };
                                let to = if row == end.line {
                                    end.byte
                                } else {
                                    line.len()
                                };
                                t.push_str(&line[from..to]);
                                if row < end.line {
                                    t.push('\n');
                                }
                            }
                            t
                        };
                        if let Some(cb) = self.clipboard.as_mut() {
                            if let Err(e) = cb.set_text(text.clone()) {
                                log::warn!("剪切编辑器选区（复制阶段）失败: {e}");
                            }
                        }
                        // 删除选区
                        let outcome = self.tabs[ti].panes[pi]
                            .editor
                            .apply(&lumen_editor::EditAction::DeleteBackward);
                        if outcome.doc_changed {
                            self.window.request_redraw();
                            events.push(StateEvent::EditorRevision(
                                self.tabs[ti].panes[pi].editor.revision(),
                            ));
                        }
                    }
                }

                // ── 无 feature 时的死代码分支（保持编译通过）─────────────
                #[cfg(not(feature = "input-editor"))]
                TermAction::CopyEditorSelection | TermAction::CutEditorSelection => {}
            },
        }

        // 每次 dispatch 后推导当前模式，若变化则发 ModeChanged 事件。
        // 此推导调用符合设计稿「按键处理后实时计算」的纪律，不缓存。
        let _current_mode =
            mode::effective_mode(&self.tabs[ti].panes[pi].term, self.force_fallback);
        // 批B：ModeChanged 事件待消费方（状态条）就绪后填充。
        // events.push(StateEvent::ModeChanged(current_mode));

        events
    }

    /// 按会话 id 定位窗格：返回 (tab 下标, 窗格下标)。
    fn find_pane(&self, sid: SessionId) -> Option<(usize, usize)> {
        self.tabs
            .iter()
            .enumerate()
            .find_map(|(ti, t)| t.panes.iter().position(|p| p.id == sid).map(|pi| (ti, pi)))
    }

    /// 鼠标当前位置命中的激活 tab 窗格下标（且未被 egui 弹层盖住）；
    /// 不在任何窗格上返回 None。
    ///
    /// 终端区鼠标交互（选区/块点击/滚轮）以此为闸，不依赖 egui 的
    /// consumed（CentralPanel 覆盖终端区，悬停即视为「在 egui 区域
    /// 上」，consumed 对鼠标无判别力）。右键菜单等弹层可能盖在终端
    /// 上：面板与 CentralPanel 同属 Background 层，弹层在更高层——
    /// 命中非背景层即视为「鼠标在 egui 弹层上」，交互归 egui。
    /// 矩形按会话 id 配对（来自上一帧布局）：tab 结构刚变更时陈旧
    /// 条目在当前激活 tab 里解析不到窗格，自然返回 None。
    fn pane_under_mouse(&self) -> Option<usize> {
        // 窗格关闭按钮的命中区让位（F5 批2）：✕ 上的点击/滚轮/右键
        // 都不算「在窗格上」，点击由 egui 侧的 pane_close 动作处理。
        if self.mouse_on_pane_close() {
            return None;
        }
        // 分隔条命中区让位（F7③）：分隔条上的按下是调比例的开始，
        // 不算「在窗格上」（拖动由 egui 侧处理）。
        if self.mouse_on_pane_divider() {
            return None;
        }
        // 面板拖宽手柄让位（P10）：文件树右缘的手柄盖住终端区左缘
        // 数像素，按下是拖宽的开始，不算「在窗格上」。
        if self.mouse_on_panel_resize() {
            return None;
        }
        let (mx, my) = self.mouse_pos;
        let (sid, _) = self.pane_rects_px.iter().find(|(_, (x, y, w, h))| {
            mx >= *x as f64 && my >= *y as f64 && mx < (*x + *w) as f64 && my < (*y + *h) as f64
        })?;
        let ppp = self.egui_ctx.pixels_per_point();
        let pos = egui::pos2(mx as f32 / ppp, my as f32 / ppp);
        if self
            .egui_ctx
            .layer_id_at(pos)
            .is_some_and(|l| l.order != egui::Order::Background)
        {
            return None;
        }
        self.tabs[self.active_tab]
            .panes
            .iter()
            .position(|p| p.id == *sid)
    }

    /// 鼠标当前位置是否落在某个窗格关闭按钮上（上一帧布局的命中区，
    /// 与 pane_rects_px 同源同陈旧度）。
    fn mouse_on_pane_close(&self) -> bool {
        let (mx, my) = self.mouse_pos;
        self.pane_close_rects_px.iter().any(|(x, y, w, h)| {
            mx >= *x as f64 && my >= *y as f64 && mx < (*x + *w) as f64 && my < (*y + *h) as f64
        })
    }

    /// 鼠标当前位置是否落在某个分隔条命中区上（上一帧布局，F7③）。
    fn mouse_on_pane_divider(&self) -> bool {
        let (mx, my) = self.mouse_pos;
        self.divider_rects_px.iter().any(|(x, y, w, h)| {
            mx >= *x as f64 && my >= *y as f64 && mx < (*x + *w) as f64 && my < (*y + *h) as f64
        })
    }

    /// 鼠标当前位置是否落在侧栏/文件树栏的拖宽手柄上（上一帧布局，
    /// P10）。
    fn mouse_on_panel_resize(&self) -> bool {
        let (mx, my) = self.mouse_pos;
        self.panel_resize_rects_px.iter().any(|(x, y, w, h)| {
            mx >= *x as f64 && my >= *y as f64 && mx < (*x + *w) as f64 && my < (*y + *h) as f64
        })
    }

    /// 焦点窗格 footer 区域的物理像素矩形 (x, y, w, h)。
    ///
    /// 与 `sel_point_at_mouse` 使用相同几何源（同函数计算 footer_px），
    /// 确保命中判定与渲染几何一致、不漂移。
    /// Compose/Fallback（可见）态才返回 Some；AltScreen/Hidden 态返回 None。
    #[cfg(feature = "input-editor")]
    fn focused_footer_rect_px(&self) -> Option<(f32, f32, f32, f32)> {
        let (x, y, w, h) = self.focused_pane_rect_px()?;
        let pane = self.focused_pane();
        let mode = mode::effective_mode(&pane.term, self.force_fallback);
        let cv = composer::compose_view_for_mode(
            mode,
            pane.editor.view(),
            pane.preedit.clone(),
            pane.exit_badge.clone(),
            None,
        );
        if !cv.is_visible() {
            return None;
        }
        let (_, cell_h) = self.renderer.cell_size();
        let fp = self.renderer.padding() * 0.4;
        let max_h = h / 3.0;
        let footer_h =
            lumen_renderer::composer_view::footer_height_px(Some(&cv), cell_h, fp, max_h);
        if footer_h <= 0.0 {
            return None;
        }
        // footer 区域 = 窗格底部 footer_h 像素带
        Some((x, y + h - footer_h, w, footer_h))
    }

    /// 当前鼠标位置是否落在焦点窗格的 footer 区域内（Compose/可见态）。
    #[cfg(feature = "input-editor")]
    fn mouse_on_footer(&self) -> bool {
        let Some((fx, fy, fw, fh)) = self.focused_footer_rect_px() else {
            return false;
        };
        let (mx, my) = self.mouse_pos;
        mx >= fx as f64 && my >= fy as f64 && mx < (fx + fw) as f64 && my < (fy + fh) as f64
    }

    /// 当前鼠标物理像素位置换算为 footer 内相对坐标（相对 footer 左上角）。
    /// 返回 (rel_x, rel_y, cell_w, cell_h, footer_padding, lines)，
    /// 便于调用 `footer_mouse::pixel_to_position`。
    #[cfg(feature = "input-editor")]
    fn mouse_footer_relative(&self) -> Option<(f32, f32, f32, f32, f32, Vec<String>)> {
        let (fx, fy, _fw, _fh) = self.focused_footer_rect_px()?;
        let (mx, my) = self.mouse_pos;
        let rel_x = mx as f32 - fx;
        let rel_y = my as f32 - fy;
        let (cell_w, cell_h) = self.renderer.cell_size();
        let fp = self.renderer.padding() * 0.4;
        let pane = self.focused_pane();
        let lines: Vec<String> = pane.editor.view().lines().map(|l| l.to_owned()).collect();
        Some((rel_x, rel_y, cell_w, cell_h, fp, lines))
    }

    /// 焦点窗格的物理像素矩形 (x, y, w, h)。首帧布局前/结构刚变更
    /// 时可能为 None。
    fn focused_pane_rect_px(&self) -> Option<(f32, f32, f32, f32)> {
        let fid = self.focused_pane().id;
        self.pane_rects_px
            .iter()
            .find(|(id, _)| *id == fid)
            .map(|(_, r)| *r)
    }

    /// 把当前鼠标像素位置换算成**焦点窗格**的选区端点（绝对行号）。
    /// cell_at 接相对窗格原点的坐标并按窗格尺寸夹紧；焦点窗格矩形
    /// 未知（首帧布局前）时返回 None。
    ///
    /// M4.1 批C：footer 区域（底部 footer_px 像素）的点击夹紧到末行，
    /// 不映射进 footer（footer 有自己的点击处理，批D 实现）。
    fn sel_point_at_mouse(&self) -> Option<SelPoint> {
        let (x, y, w, h) = self.focused_pane_rect_px()?;
        // M4.1 批C：计算 footer 高度以排除 footer 区域的点击。
        #[cfg(feature = "input-editor")]
        let footer_px = {
            let pane = self.focused_pane();
            let mode = mode::effective_mode(&pane.term, self.force_fallback);
            let cv = composer::compose_view_for_mode(
                mode,
                pane.editor.view(),
                pane.preedit.clone(),
                pane.exit_badge.clone(),
                None, // ghost 仅用于渲染，高度计算不需要
            );
            let (_, cell_h) = self.renderer.cell_size();
            let fp = self.renderer.padding() * 0.4;
            let max_h = h / 3.0;
            lumen_renderer::composer_view::footer_height_px(Some(&cv), cell_h, fp, max_h)
        };
        #[cfg(not(feature = "input-editor"))]
        let footer_px: f32 = 0.0;
        let (row, col) = self.renderer.cell_at_with_footer(
            self.mouse_pos.0 - x as f64,
            self.mouse_pos.1 - y as f64,
            w.max(1.0) as u32,
            h.max(1.0) as u32,
            footer_px,
        );
        Some(SelPoint {
            line: self.focused_pane().term.grid().view_top_abs_line() + row as u64,
            col,
        })
    }

    /// 切换激活 tab：清掉目标 tab **全部窗格**的冻结计时与渲染计划
    /// （属于「上次激活期间」的旧时间轴，带过来会借用过期的调度），
    /// 清未读点，同步窗口标题并立即重绘。无覆盖层/重命名时终端拿
    /// 键盘/IME 焦点。
    fn activate(&mut self, idx: usize) {
        // 换出 tab 的拖选手势随切换结束：按住左键 Ctrl+Tab 切走后，
        // Released 只检查新焦点窗格的 selecting，旧窗格的标志会永久
        // 残留——切回时不按键鼠标一动就「幽灵拖选」，且 Ctrl+C 被
        // 选区复制分支吞掉。close_tab 路径下旧下标可能已越界
        // （删的是末位激活 tab），用 get_mut 防御。
        if let Some(prev) = self.tabs.get_mut(self.active_tab) {
            for p in &mut prev.panes {
                p.selecting = false;
            }
        }
        self.active_tab = idx;
        for s in &mut self.tabs[idx].panes {
            s.cursor_frozen_at = None;
            s.redraw_at = None;
            s.redraw_hard_at = None;
            s.redraw_abs_at = None;
            // 离屏纹理里还是后台期间的旧画面：下一帧必须渲染本窗格，
            // 即使它正处于 DEC 2026 同步区间（画半成品也好过画旧帧）。
            // 欠帧起点回拨 REDRAW_ABS_CAP 让它直接「超龄」：若新数据
            // 赶在重绘执行前重新武装了渲染计划，门控也不许把旧画面多
            // 留哪怕一帧（checked_sub 仅防进程启动极早期的理论下溢）。
            s.term_frame_due_since = Some(
                Instant::now()
                    .checked_sub(REDRAW_ABS_CAP)
                    .unwrap_or_else(Instant::now),
            );
            s.has_unseen_output = false;
        }
        // 焦点归属按覆盖层/重命名状态计算，不无条件抢回：后台 shell
        // 自行退出触发的 activate 可能发生在用户正往设置页/登录表单/
        // 重命名框打字时，无脑置 true 会让在途按键直写邻位会话的 PTY
        // （bypass_egui 即刻生效，等不到下一帧的纠偏）。
        self.terminal_focused = !(self.shell_state.settings.open
            || self.shell_state.login.open
            || self.shell_state.history_search.open
            || self.shell_state.completion.open
            || self.shell_state.renaming.is_some()
            || self.shell_state.filetree.dialog_open());
        self.update_window_title();
        self.window.request_redraw();
        // 激活下标是持久化状态的一部分：切换即落盘（F4）。
        self.persist_sessions();
    }

    /// 切换激活 tab 内的焦点窗格（点击窗格 / F5 焦点路由）。窗口
    /// 标题、文件树 cwd、键盘/IME/滚轮路由随之跟随新焦点窗格。
    fn focus_pane(&mut self, idx: usize) {
        let tab = &mut self.tabs[self.active_tab];
        if idx >= tab.panes.len() || idx == tab.focused {
            return;
        }
        // 最大化期间焦点强制为最大化格（P14）：隐藏窗格的矩形不在
        // 本帧布局里、正常路径点不到，纯防御（陈旧矩形/竞态）。
        if tab.maximized.is_some_and(|m| m != idx) {
            return;
        }
        // 旧焦点窗格的拖选手势随切焦点结束（与 activate 同理：标志
        // 残留会在切回时产生幽灵拖选）。窗格本身保持可见、渲染计划
        // 与冻结计时是「正在上屏」的活状态，不清。
        tab.panes[tab.focused].selecting = false;
        tab.focused = idx;
        // accent 边框移动 + 标题跟随需要一帧重绘。
        self.update_window_title();
        self.window.request_redraw();
        // 焦点窗格下标是持久化状态的一部分（F5）。
        self.persist_sessions();
    }

    /// 释放窗格的渲染资源：离屏纹理 + 行排版缓存即刻释放；egui 侧
    /// 的纹理注册推迟到帧呈现后注销（关闭动作可能发生在 run_ui 之
    /// 后，本帧 shape 仍引用该纹理；离屏视图被 egui 注册表持有引用
    /// 计数，先行 drop 不影响本帧采样）。
    fn release_pane_resources(&mut self, sid: SessionId) {
        self.renderer.drop_offscreen(sid);
        if let Some(tex) = self.pane_textures.remove(&sid) {
            self.pending_tex_free.push(tex);
        }
    }

    /// 关闭整个 tab：窗格全部移除即随 `PtySession` Drop 杀掉子进程；
    /// 各窗格通道的接收端同时销毁，转发线程 send 失败自然退出（残留
    /// 事件随通道一并丢弃，无需清理）。
    /// 返回是否已无 tab（调用方应退出应用）。
    fn close_tab(&mut self, idx: usize) -> bool {
        let removed = self.tabs.remove(idx);
        info!(
            "关闭 tab id={}（{} 个窗格）",
            removed.id,
            removed.panes.len()
        );
        let sids: Vec<SessionId> = removed.panes.iter().map(|p| p.id).collect();
        drop(removed);
        for sid in sids {
            self.release_pane_resources(sid);
        }
        if self.tabs.is_empty() {
            // 最后一个 tab 关闭即退出：以退出瞬间的（空）列表落盘，
            // 下次启动回到单默认会话（F4）。
            self.persist_sessions();
            return true;
        }
        // tab 列表变化必须立即反映到侧栏：后台 shell 自行退出关 tab
        // 不经过 activate()，没有这句时已死条目会一直挂在侧栏，直到
        // 下一个无关事件碰巧触发重绘。激活路径里 activate() 也会
        // request_redraw，重复请求由 winit 合并，无害。
        self.window.request_redraw();
        if idx < self.active_tab {
            // 移除位在激活位之前：激活 tab 整体左移一位，无需切换。
            self.active_tab -= 1;
        } else if idx == self.active_tab {
            // 关闭激活 tab：切到邻位（右邻顶上原位；无右邻取末位）。
            self.activate(idx.min(self.tabs.len() - 1));
        }
        // 关 tab 是结构性变更：落盘（activate 路径已写过时快照一致，
        // 自动跳过）。
        self.persist_sessions();
        false
    }

    /// 关闭单个窗格（shell 退出 / Ctrl+Shift+W）：最后一个窗格时 =
    /// 关整个 tab。返回是否已无 tab（调用方应退出应用）。
    fn close_pane(&mut self, ti: usize, pi: usize) -> bool {
        if self.tabs[ti].panes.len() <= 1 {
            return self.close_tab(ti);
        }
        let removed = self.tabs[ti].panes.remove(pi);
        let sid = removed.id;
        info!("关闭窗格 id={sid}（tab id={}）", self.tabs[ti].id);
        drop(removed);
        self.release_pane_resources(sid);
        // 焦点下标调整（与关 tab 的激活下标同款规则：移除位之前的
        // 整体左移；关焦点窗格时右邻顶上原位、无右邻取末位）。
        let tab = &mut self.tabs[ti];
        if pi < tab.focused {
            tab.focused -= 1;
        } else if pi == tab.focused {
            tab.focused = pi.min(tab.panes.len() - 1);
        }
        // 最大化下标随移除调整（P14）：关最大化格自动退出（其余格
        // 还原可见）；关它之前的隐藏格（shell 自行退出）下标左移。
        tab.maximized = match tab.maximized {
            Some(m) if pi == m => None,
            Some(m) if pi < m => Some(m - 1),
            other => other,
        };
        // 剩单格无最大化语义（不变量：Some 时必有多格）。
        if tab.panes.len() == 1 {
            tab.maximized = None;
        }
        // 删窗格重置比例为均分（与增窗格同理，F7 拍板）。
        tab.layout = PaneLayout::uniform(tab.panes.len());
        if ti == self.active_tab {
            // 可见窗格布局变化 + 标题可能跟随新焦点窗格；后台 tab 关
            // 窗格也要重绘侧栏（未读点可能随窗格消失），统一请求。
            self.update_window_title();
        }
        self.window.request_redraw();
        // 关窗格是结构性变更：落盘（F5）。
        self.persist_sessions();
        false
    }

    /// 单窗格 shell 退出后在原位重启一个新 shell（海风哥 2026-06-13 体验
    /// 优化：单窗口 `exit` 不关应用，而是换一个干净的 PowerShell 继续用；
    /// 多窗格场景仍走 [`Self::close_pane`] 关掉退出的那格）。
    ///
    /// 沿用旧窗格的网格行列与 cwd（最后上报的 OSC 9;9 目录，失效回初始/
    /// 默认目录），id 新分配、旧窗格渲染资源释放。返回 `true` 表示重启失败
    /// 且退回关闭后已无 tab（调用方应退出应用）。
    fn respawn_pane(&mut self, ti: usize, pi: usize) -> bool {
        let (rows, cols, cwd, old_id) = {
            let old = &self.tabs[ti].panes[pi];
            let g = old.term.grid();
            let cwd = old
                .term
                .cwd()
                .map(std::path::Path::to_path_buf)
                .or_else(|| old.initial_cwd.clone())
                // spawn 约定由调用方先验证目录仍存在。
                .filter(|p| p.is_dir());
            (g.rows(), g.cols(), cwd, old.id)
        };
        let id = self.next_session_id;
        self.next_session_id += 1;
        match Session::spawn(
            id,
            rows,
            cols,
            SCROLLBACK,
            self.wake_pending.clone(),
            self.proxy.clone(),
            cwd.as_deref(),
        ) {
            Ok(s) => {
                // 原位替换：旧 Session 随赋值 Drop（杀旧进程——已退出）。
                self.tabs[ti].panes[pi] = s;
                self.release_pane_resources(old_id);
                info!("单窗格 shell 退出，已在原位重启新 shell（id {old_id}→{id}）");
                self.update_window_title();
                self.window.request_redraw();
                // 会话内容变更（id 换绑）：落盘。
                self.persist_sessions();
                false
            }
            Err(e) => {
                // 重启失败（系统起不了进程）：退回关闭，避免卡死无响应窗格。
                error!("单窗格 shell 重启失败: {e:#}，退回关闭窗格");
                self.close_pane(ti, pi)
            }
        }
    }

    /// 新建 tab（单窗格，继承当前 shell 配置）并切换为激活。
    /// 行列数先取焦点窗格网格，下一帧按实际窗格矩形校正。
    fn new_tab(&mut self) {
        let g = self.focused_pane().term.grid();
        let (rows, cols) = (g.rows(), g.cols());
        let id = self.next_session_id;
        self.next_session_id += 1;
        match Session::spawn(
            id,
            rows,
            cols,
            SCROLLBACK,
            self.wake_pending.clone(),
            self.proxy.clone(),
            None,
        ) {
            Ok(s) => {
                let tab_id = self.next_tab_id;
                self.next_tab_id += 1;
                self.tabs.push(Tab {
                    id: tab_id,
                    custom_title: None,
                    panes: vec![s],
                    focused: 0,
                    layout: PaneLayout::uniform(1),
                    maximized: None,
                });
                // activate 内部会落盘会话快照（新建是结构性变更）。
                self.activate(self.tabs.len() - 1);
            }
            Err(e) => error!("新建会话失败: {e:#}"),
        }
    }

    /// 激活 tab 内新增一个窗格（F5：Ctrl+Shift+D；+ 按钮归批次2）。
    /// 满 [`MAX_PANES`] 时 toast 提示。新窗格继承焦点窗格的 cwd
    /// （Warp/Windows Terminal 分屏惯例；OSC 9;9 未上报时退恢复时的
    /// 初始目录，目录已失效则回默认），并自动成为焦点。
    ///
    /// # B3 根治：spawn 前预计算新窗格真实尺寸
    ///
    /// 旧实现用「焦点窗格当前行列」spawn，但新格加入后布局从 N 变
    /// N+1 格均分，各格真实尺寸完全不同。时序：
    ///   shell 按错误宽度打印首个提示符
    ///   → 下一帧 egui 出真实矩形 → resize
    ///   → ConPTY 按新列宽 reflow，PSReadLine 坐标簿仍按旧假设
    ///   → 该行后续编辑持续列错位（截图混叠形态）
    ///   → 回车开新行 PSReadLine 重新定位 → 正常
    ///
    /// 修复：spawn 前按「加入后布局」预计算新格矩形，复用
    /// [`estimate_restored_pane_px`] 同源逻辑（n+1 均分、扣标题栏、
    /// 换算行列），spawn 即终态尺寸，首帧零 resize。
    fn new_pane(&mut self) {
        if self.tabs[self.active_tab].panes.len() >= MAX_PANES {
            self.shell_state.toast.push(
                shell::toast::ToastKind::Warn,
                i18n::fmt1(i18n::strings().toast_max_panes_fmt, MAX_PANES),
            );
            // push 不在 egui 帧内：请求一帧立即显示。
            self.window.request_redraw();
            return;
        }

        // —— 预计算新窗格真实尺寸（B3 根治）——
        // 新格加入后共 n+1 格，布局均分，新格是最后一个（index=n）。
        // area 估算与 estimate_restored_pane_px 同源：扣侧栏/顶栏/文件树。
        let n = self.tabs[self.active_tab].panes.len();
        let scale = self.egui_ctx.pixels_per_point();
        let inner = self.window.inner_size();
        let sidebar_px = (self.settings.layout.sidebar_width * scale).round();
        let topbar_px = (shell::topbar::HEIGHT * scale).round();
        let ft_width = self
            .shell_state
            .filetree
            .effective_width(self.settings.layout.filetree_width);
        let ft_px = (ft_width * scale).round();
        let term_w_px = (inner.width as f32 - sidebar_px).max(1.0);
        let term_h_px = (inner.height as f32 - topbar_px).max(1.0);
        // est_area 为逻辑点（与 estimate_restored_pane_px 入参单位一致）。
        let est_area = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2((term_w_px - ft_px).max(1.0) / scale, term_h_px / scale),
        );
        // n+1 格均分，取第 n 个矩形（新格）。
        let new_pane_layout = PaneLayout::uniform(n + 1);
        let est_px = estimate_restored_pane_px(est_area, &new_pane_layout, n + 1, None, scale);
        // 估算不可用时兜底焦点窗格当前尺寸（防御，不应发生）。
        // M4.1 批C 注：新窗格 spawn 时 term 尚无 block 数据 → Fallback 态 →
        // footer_px=0，此处用 grid_size_for（等价 footer_px=0）正确。
        // 首帧实际布局后 RedrawRequested 会按真实 footer 高度做精确 resize。
        let (rows, cols) = est_px
            .get(n)
            .map(|&(w, h)| self.renderer.grid_size_for(w, h))
            .unwrap_or_else(|| {
                let g = self.focused_pane().term.grid();
                (g.rows(), g.cols())
            });
        info!(
            "new_pane 预计算：n+1={} 格，新格 rows={rows} cols={cols}（est_area={:?}）",
            n + 1,
            est_area,
        );

        let cwd = self
            .focused_pane()
            .term
            .cwd()
            .map(std::path::Path::to_path_buf)
            .or_else(|| self.focused_pane().initial_cwd.clone())
            // spawn 约定由调用方先验证目录仍存在。
            .filter(|p| p.is_dir());
        let id = self.next_session_id;
        self.next_session_id += 1;
        match Session::spawn(
            id,
            rows,
            cols,
            SCROLLBACK,
            self.wake_pending.clone(),
            self.proxy.clone(),
            cwd.as_deref(),
        ) {
            Ok(s) => {
                let tab = &mut self.tabs[self.active_tab];
                // 最大化态先自动退出再加格（P14：新格要可见，隐藏着
                // 加格没有意义）。
                tab.maximized = None;
                tab.panes.push(s);
                tab.focused = tab.panes.len() - 1;
                // 增窗格重置比例为均分（F7 拍板：简单正确优先——网格
                // 结构随数量变化，旧权重的形状已不适用）。
                tab.layout = PaneLayout::uniform(tab.panes.len());
                // 布局变化：下一帧 egui 产出新窗格矩形并触发逐窗格
                // 离屏重建 + term/pty resize。
                self.update_window_title();
                self.window.request_redraw();
                // 增窗格是结构性变更：落盘（F5）。
                self.persist_sessions();
            }
            Err(e) => {
                error!("新建窗格失败: {e:#}");
                self.shell_state.toast.push(
                    shell::toast::ToastKind::Error,
                    i18n::fmt1(i18n::strings().toast_new_pane_failed_fmt, &e),
                );
                self.window.request_redraw();
            }
        }
    }

    /// 最大化/还原激活 tab 的窗格 `pi`（P14：标题栏按钮 /
    /// Ctrl+Shift+Enter）。已处于最大化态时还原（无论 `pi` 是哪格
    /// ——可见的只有最大化格，按钮/快捷键都落在它身上）；普通态把
    /// `pi` 最大化并强制聚焦。布局权重不动：还原即回原比例。
    fn toggle_maximize_pane(&mut self, pi: usize) {
        let tab = &mut self.tabs[self.active_tab];
        if pi >= tab.panes.len() {
            return; // 防御：结构刚变更的过渡帧
        }
        if tab.maximized.is_some() {
            tab.maximized = None;
            // 还原后其余窗格的离屏纹理还是隐藏前的旧画面：强制下一帧
            // 渲染（同 activate 的「超龄欠帧」手法——即使正处同步
            // 区间也不许把旧帧多留一帧）。
            for s in &mut tab.panes {
                s.term_frame_due_since = Some(
                    Instant::now()
                        .checked_sub(REDRAW_ABS_CAP)
                        .unwrap_or_else(Instant::now),
                );
            }
        } else {
            if tab.panes.len() <= 1 {
                return; // 单窗格本就满屏，无最大化语义
            }
            // 焦点强制为最大化格（旧焦点窗格的拖选手势随切焦点结束，
            // 与 focus_pane 同理）。
            tab.panes[tab.focused].selecting = false;
            tab.focused = pi;
            tab.maximized = Some(pi);
            // 隐藏窗格清残留渲染计划（后台消化不再武装新计划，残留
            // 计划会让 about_to_wait 空打一帧）。
            for (i, s) in tab.panes.iter_mut().enumerate() {
                if i != pi {
                    s.redraw_at = None;
                    s.redraw_hard_at = None;
                    s.redraw_abs_at = None;
                }
            }
        }
        // 布局变化：下一帧 egui 产出新矩形并触发离屏重建 + resize；
        // 最大化态是持久化状态的一部分（P14，重启保持）。
        self.update_window_title();
        self.window.request_redraw();
        self.persist_sessions();
    }

    /// 一键恢复默认布局（P15：顶栏「▦」）：激活 tab 的行/列权重全部
    /// 恢复均分；处于最大化态先退出（其余窗格还原可见并强制补帧）。
    /// 复位后落盘。单窗格/已均分且非最大化时无事可做。
    fn reset_pane_layout(&mut self) {
        let tab = &mut self.tabs[self.active_tab];
        if tab.panes.len() <= 1 {
            return; // 顶栏按钮单窗格时已禁用，纯防御
        }
        let uniform = PaneLayout::uniform(tab.panes.len());
        if tab.maximized.is_some() {
            tab.maximized = None;
            // 与 toggle_maximize_pane 的还原分支同款：隐藏窗格的纹理
            // 还是旧画面，强制下一帧渲染。
            for s in &mut tab.panes {
                s.term_frame_due_since = Some(
                    Instant::now()
                        .checked_sub(REDRAW_ABS_CAP)
                        .unwrap_or_else(Instant::now),
                );
            }
        } else if tab.layout == uniform {
            return; // 已是均分且非最大化：无变化不写盘
        }
        tab.layout = uniform;
        self.window.request_redraw();
        // 复位后写盘（P15；与拖动结束/双击复位同语义）。
        self.persist_sessions();
    }

    /// 循环切换激活 tab：dir 为 1（下一个）或 -1（上一个）。
    fn cycle_tab(&mut self, dir: isize) {
        let n = self.tabs.len() as isize;
        if n <= 1 {
            return;
        }
        let idx = (self.active_tab as isize + dir).rem_euclid(n) as usize;
        self.activate(idx);
    }

    /// 窗口标题跟随激活 tab（与侧栏条目同源 display_title：自定义名 >
    /// 焦点窗格 cwd > OSC 标题 > 「会话 N」，恒非空）。
    fn update_window_title(&self) {
        let title = self.tabs[self.active_tab].display_title();
        self.window.set_title(&format!("Lumen — {title}"));
    }

    /// 构造当前 tab 列表的持久化快照（F4/F5 嵌套结构：每 tab 的
    /// 自定义名 + 各窗格 cwd + 焦点下标）。窗格 cwd 取 OSC 9;9 上报
    /// 值，尚未上报（恢复后首个提示符还没到）时回退该窗格启动时的
    /// 初始目录——防止恢复后立即触发的写盘把保存的 cwd 冲成 None。
    fn sessions_snapshot(&self) -> sessions_store::SessionsFile {
        sessions_store::SessionsFile::new(
            self.tabs
                .iter()
                .map(|t| sessions_store::TabEntry {
                    custom_title: t.custom_title.clone(),
                    panes: t
                        .panes
                        .iter()
                        .map(|p| sessions_store::PaneEntry {
                            cwd: p
                                .term
                                .cwd()
                                .map(std::path::Path::to_path_buf)
                                .or_else(|| p.initial_cwd.clone()),
                        })
                        .collect(),
                    focused: t.focused,
                    // 布局比例（F7③）：构造路径保证归一化权重，写盘
                    // 原值（拖动结束/双击复位时由调用时机触发落盘）。
                    row_weights: t.layout.row_weights().to_vec(),
                    col_weights: t.layout.col_weights().to_vec(),
                    // 最大化态（P14）：toggle 即落盘，重启保持。
                    maximized: t.maximized,
                })
                .collect(),
            self.active_tab,
        )
    }

    /// 会话列表持久化（F4）：结构性变更（新建/关闭/重命名/切换激活）
    /// 与 cwd 上报变化时调用；快照与上次写盘一致则跳过，实际写频
    /// ≈ 用户开关 tab / cd 的频率。失败只记日志（save 内部），不
    /// 打扰终端使用。
    fn persist_sessions(&mut self) {
        let snap = self.sessions_snapshot();
        if self.last_sessions_snapshot.as_ref() == Some(&snap) {
            return;
        }
        log::debug!(
            "会话快照落盘：{} 个 tab，权重 {:?}",
            snap.tabs.len(),
            snap.tabs
                .iter()
                .map(|t| (&t.row_weights, &t.col_weights))
                .collect::<Vec<_>>()
        );
        snap.save();
        self.last_sessions_snapshot = Some(snap);
    }
}

impl App {
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<AppState> {
        // M3.8 自绘标题栏：无边框窗口 + DWM 阴影/Win11 圆角。
        // with_decorations(false) 在 Windows 上保留 WS_THICKFRAME（拖边 resize
        // 可用），WM_NCCALCSIZE 铺满客户区（无系统标题栏）。
        // with_undecorated_shadow(true) 启用 DWM 阴影并允许 Win11 圆角识别；
        // 副作用：顶部 1px 黑线（顶栏背景色覆盖消除）。
        // 非 Windows 平台降级保留系统装饰（with_decorations 有 #[cfg(windows)] 处理）。
        // 第二十二轮：运行时窗口图标（窗口左上角/Alt-Tab/任务栏运行态）。
        // with_window_icon 设 32px 图标（符合 Windows ICON 推荐小图标尺寸）；
        // with_taskbar_icon (Windows 专属扩展) 设 64px 大图标（任务栏/Alt-Tab 高 DPI）。
        // 解码失败降级为 None（warn 已在 load_icon 内打印）。
        let window_icon = load_icon(include_bytes!("../../../icons/lumen-icon-32.png"));
        #[cfg(target_os = "windows")]
        let taskbar_icon = load_icon(include_bytes!("../../../icons/lumen-icon-64.png"));
        #[cfg(target_os = "windows")]
        let attrs = {
            Window::default_attributes()
                .with_title("Lumen")
                .with_inner_size(winit::dpi::LogicalSize::new(1000.0, 640.0))
                .with_maximized(true)
                .with_decorations(false)
                .with_undecorated_shadow(true)
                .with_window_icon(window_icon)
                .with_taskbar_icon(taskbar_icon)
        };
        #[cfg(not(target_os = "windows"))]
        let attrs = Window::default_attributes()
            .with_title("Lumen")
            .with_inner_size(winit::dpi::LogicalSize::new(1000.0, 640.0))
            .with_maximized(true)
            .with_window_icon(window_icon);
        // 启动默认最大化（P17）：inner_size 保留为「取消最大化」后的还原尺寸。
        let window = Arc::new(event_loop.create_window(attrs).context("创建窗口失败")?);
        // workaround winit #4186：with_decorations(false) + with_resizable(true) 下
        // 拖边 resize 可能失效（WS_THICKFRAME 添加时序 bug，PR #4188 修复未合入 0.30.9）。
        // init 后显式调 set_resizable(true) 可触发 WS_THICKFRAME 重新施加，绕过该 bug。
        window.set_resizable(true);
        window.set_ime_allowed(true);
        // 告知输入法处于终端语境（egui-winit 内部有同等映射）。
        window.set_ime_purpose(winit::window::ImePurpose::Terminal);

        let size = window.inner_size();
        let scale = window.scale_factor() as f32;
        let mut renderer = Renderer::new(window.clone(), size.width, size.height, scale)
            .context("初始化渲染器失败")?;

        // —— 设置加载与应用（settings.json；缺失/损坏降级默认值）——
        let app_settings = settings::Settings::load();
        // F6 多语言：启动后立即将全局语言设为设置中存储的语言。
        i18n::set_language(app_settings.language);
        // 系统深浅模式（P12 Sync with OS）：winit 报不出来（None）按
        // 深色处理——默认主题即深色；后续变化经 ThemeChanged 事件维护。
        let os_dark = !matches!(window.theme(), Some(winit::window::Theme::Light));
        let ap = &app_settings.appearance;
        let actual_family = renderer.reconfigure_font(&ap.font_family, ap.font_size);
        let theme_info = settings::theme_info(app_settings.effective_theme_id(os_dark));
        renderer.set_theme(theme_info.theme());
        // 问题5：启动时同步 panel_outline 描边色到 renderer。
        {
            let pal = shell::theme::shell_palette(theme_info);
            let [r, g, b, _] = pal.panel_outline.to_array();
            renderer.set_footer_border_color(r, g, b);
        }
        info!(
            "设置加载：主题 {}（id {}，sync_with_os={}）字号 {} 侧栏宽 {}/{} 字体「{}」→ 实际生效「{actual_family}」",
            theme_info.name,
            theme_info.id,
            ap.sync_with_os,
            ap.font_size,
            app_settings.layout.sidebar_width,
            app_settings.layout.filetree_width,
            if ap.font_family.is_empty() {
                "自动"
            } else {
                &ap.font_family
            }
        );
        // 字体回退提示（设置页 Appearance 展示）。
        // F6：启动时语言已由第 1069 行 i18n::set_language 设置完毕，
        // 此处必须走 i18n 表而非硬编码简体中文。
        let font_hint = (!ap.font_family.is_empty()
            && !actual_family.eq_ignore_ascii_case(&ap.font_family))
        .then(|| {
            i18n::fmt2(
                i18n::strings().toast_font_fallback_fmt,
                &ap.font_family,
                &actual_family,
            )
        });
        // 首次启动（无设置文件）落盘默认值，方便用户直接手改；
        // 文件存在但损坏时不在此覆盖（保留现场，变更时才写）。
        // 此刻 UI 尚未建立，失败只记日志（save 内部已记）不弹 toast。
        if settings::Settings::path().is_some_and(|p| !p.exists()) {
            let _ = app_settings.save();
        }

        // —— 登录态加载（profile.json；缺失=未登录、损坏=未登录+警告）——
        let user_profile = profile::Profile::load();
        match &user_profile {
            Some(p) => info!("登录态加载：{} <{}>（本地 mock）", p.display_name, p.email),
            None => info!("登录态：未登录"),
        }

        // —— egui 三件套 ——
        let egui_ctx = egui::Context::default();
        shell::theme::apply_style(&egui_ctx, &shell::theme::shell_palette(theme_info));
        shell::theme::install_cjk_fonts(&egui_ctx);
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(scale),
            None,
            Some(renderer.device().limits().max_texture_dimension_2d as usize),
        );
        let egui_renderer = egui_wgpu::Renderer::new(
            renderer.device(),
            renderer.surface_format(),
            egui_wgpu::RendererOptions::default(),
        );

        // 终端区初值：窗口减去侧栏宽度与顶栏高度（首帧 egui 布局后
        // 按实际窗格矩形校正，文件树栏宽度即在首帧补扣）。窗格离屏
        // 纹理在首帧 RedrawRequested 懒创建（布局前不知道各窗格尺寸）。
        let sidebar_px = (app_settings.layout.sidebar_width * scale).round();
        let topbar_px = (shell::topbar::HEIGHT * scale).round();
        let term_w = ((size.width as f32 - sidebar_px).max(1.0)) as u32;
        let term_h = ((size.height as f32 - topbar_px).max(1.0)) as u32;

        // 单会话兜底的行列数按整个终端区估算；多窗格恢复时**逐窗格**
        // 按还原布局预切矩形估算（见 estimate_restored_pane_px——B2
        // 修复：旧实现全员按整区 spawn，首帧腰斩级缩行 resize 与首个
        // 提示符打印撞车，是症状①②的共同触发器）。
        // M4.1 批C 注：初始化时 term 尚未 spawn（无 block 数据）→ Fallback →
        // footer_px=0；此处 grid_size_for 等价 footer_px=0，正确。
        // 首帧 RedrawRequested 按真实 footer 高度精确校正。
        let (rows, cols) = renderer.grid_size_for(term_w, term_h);
        info!("终端尺寸: {rows} 行 x {cols} 列（初始化估算，footer 待首帧校正）");
        // 估算用终端工作区（逻辑点）：再扣文件树栏（启动时默认展开，
        // 宽度来自设置）。与首帧实际布局的残差只剩面板边距像素级出入。
        let filetree_px = (app_settings.layout.filetree_width * scale).round();
        let est_area = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(
                (term_w as f32 - filetree_px).max(1.0) / scale,
                term_h as f32 / scale,
            ),
        );

        // PTY 事件走 per-session 有界通道（Session 自持接收端），唤醒
        // 走全局去重的 PtyWake（见 session.rs 模块文档）。
        let wake_pending = Arc::new(AtomicBool::new(false));

        // —— 会话恢复（F4/F5）：sessions.json 有效时按嵌套结构逐 tab
        // 逐窗格重开 shell（初始目录用保存的 cwd，失效回退默认并提示；
        // 屏幕内容不恢复，是新 shell）；缺失/损坏/全部 spawn 失败回退
        // 单默认会话。旧平铺格式由 sessions_store 读侧自动迁移。 ——
        let stored = sessions_store::SessionsFile::load();
        let mut tabs: Vec<Tab> = Vec::new();
        let mut next_session_id: SessionId = 0;
        let mut next_tab_id: TabId = 0;
        let mut active_idx = 0usize;
        // 保存的 cwd 已失效（目录被删/网络盘离线）的窗格数（toast 一次）。
        let mut stale_cwd = 0usize;
        // 成功还原布局比例的 tab 数（F7 持久化；恢复日志用）。
        let mut restored_layouts = 0usize;
        if let Some(stored) = &stored {
            for tab_entry in &stored.tabs {
                // 逐窗格估算 spawn 尺寸（B2 修复）：布局/最大化的取值
                // 规则与下方实际还原同源。spawn 失败跳窗格时实际布局
                // 会回退均分、估算随之偏差，但那是罕见降级路径，只
                // 影响首帧 resize 幅度，不影响正确性。
                let n = tab_entry.panes.len();
                let est_layout =
                    PaneLayout::from_weights(n, &tab_entry.row_weights, &tab_entry.col_weights)
                        .unwrap_or_else(|| PaneLayout::uniform(n));
                let est_max = tab_entry.maximized.filter(|&m| m < n && n > 1);
                let est_px = estimate_restored_pane_px(est_area, &est_layout, n, est_max, scale);
                let mut panes: Vec<Session> = Vec::new();
                for (pi, pane_entry) in tab_entry.panes.iter().enumerate() {
                    let cwd = pane_entry.usable_cwd();
                    if let Some(saved) = pane_entry.cwd.as_deref() {
                        if cwd.is_none() {
                            stale_cwd += 1;
                            log::warn!(
                                "会话恢复：保存的工作目录已失效，回退默认目录: {}",
                                saved.display()
                            );
                        }
                    }
                    // 估算不可用（防御，不应发生）回退整区行列。
                    // M4.1 批C 注：恢复路径 term 尚未 spawn → Fallback → footer_px=0，
                    // grid_size_for 等价 footer_px=0。首帧 RedrawRequested 校正。
                    let (est_rows, est_cols) = est_px
                        .get(pi)
                        .map(|&(w, h)| renderer.grid_size_for(w, h))
                        .unwrap_or((rows, cols));
                    match Session::spawn(
                        next_session_id,
                        est_rows,
                        est_cols,
                        SCROLLBACK,
                        wake_pending.clone(),
                        self.proxy.clone(),
                        cwd,
                    ) {
                        Ok(s) => {
                            next_session_id += 1;
                            panes.push(s);
                        }
                        // 单窗格 spawn 失败（shell 缺失等极端情况）跳过
                        // 该窗格，不连坐其余。
                        Err(e) => error!("恢复窗格失败（跳过该窗格）: {e:#}"),
                    }
                }
                if panes.is_empty() {
                    // 整个 tab 的窗格都没起来：跳过该 tab。
                    continue;
                }
                // 最大化态还原（P14）：读侧已夹紧，这里再按实际起来的
                // 窗格数防御（spawn 失败跳窗格会改变数量）；单格无最大
                // 化语义。最大化期间焦点强制为最大化格。
                let maximized = tab_entry
                    .maximized
                    .filter(|&m| m < panes.len() && panes.len() > 1);
                let focused = maximized.unwrap_or(tab_entry.focused.min(panes.len() - 1));
                // 布局比例还原（F7 持久化）：保存的权重形状须与实际
                // 起来的窗格数一致（spawn 失败跳窗格会改变数量）且
                // 数值合法，否则回退均分（旧 v2 无字段也走这条路）。
                let layout = match PaneLayout::from_weights(
                    panes.len(),
                    &tab_entry.row_weights,
                    &tab_entry.col_weights,
                ) {
                    Some(l) => {
                        restored_layouts += 1;
                        l
                    }
                    None => PaneLayout::uniform(panes.len()),
                };
                tabs.push(Tab {
                    id: next_tab_id,
                    custom_title: tab_entry.custom_title.clone(),
                    panes,
                    focused,
                    layout,
                    maximized,
                });
                next_tab_id += 1;
            }
            if !tabs.is_empty() {
                active_idx = stored.active_tab.min(tabs.len() - 1);
                let pane_total: usize = tabs.iter().map(|t| t.panes.len()).sum();
                info!(
                    "会话恢复：{} 个 tab / {pane_total} 个窗格，激活 #{active_idx}（cwd 失效 {stale_cwd} 个，布局比例还原 {restored_layouts} 个 tab）",
                    tabs.len()
                );
            }
        }
        if tabs.is_empty() {
            tabs.push(Tab {
                id: next_tab_id,
                custom_title: None,
                panes: vec![Session::spawn(
                    next_session_id,
                    rows,
                    cols,
                    SCROLLBACK,
                    wake_pending.clone(),
                    self.proxy.clone(),
                    None,
                )?],
                focused: 0,
                layout: PaneLayout::uniform(1),
                maximized: None,
            });
            next_session_id += 1;
            next_tab_id += 1;
        }

        let clipboard = match arboard::Clipboard::new() {
            Ok(c) => Some(c),
            Err(e) => {
                error!("剪贴板不可用: {e}");
                None
            }
        };

        let perf = std::env::var("LUMEN_PERF")
            .ok()
            .and_then(|p| std::fs::File::create(p).ok());

        // —— 命令历史库（M4.1 批D2）——
        // 启动时加载磁盘历史，顺序：磁盘 JSONL → PSReadLine 种子（首次）。
        // 加载失败降级空库，记 warn 日志，不阻断启动。
        #[cfg(feature = "input-editor")]
        let history_store = history::HistoryStore::load();

        // 在 app_settings 被 move 进 AppState 之前读出 classic_mode（第十八轮）。
        let init_force_fallback = app_settings.classic_mode;

        let mut state = AppState {
            perf,
            perf_t0: Instant::now(),
            last_render_at: None,
            last_term_render_at: None,
            window,
            renderer,
            tabs,
            active_tab: active_idx,
            next_session_id,
            next_tab_id,
            wake_pending,
            proxy: self.proxy.clone(),
            settings: app_settings,
            os_dark,
            last_sessions_snapshot: None,
            layout_dirty: false,
            layout_apply_logged: false,
            profile: user_profile,
            modifiers: ModifiersState::default(),
            clipboard,
            last_key_at: None,
            mouse_pos: (0.0, 0.0),
            egui_ctx,
            egui_state,
            egui_renderer,
            pane_textures: HashMap::new(),
            pending_tex_free: Vec::new(),
            pane_rects_px: Vec::new(),
            pane_close_rects_px: Vec::new(),
            divider_rects_px: Vec::new(),
            panel_resize_rects_px: Vec::new(),
            terminal_focused: true,
            egui_repaint_at: None,
            was_popup_open: false,
            shell_state: shell::ShellState::default(),
            window_just_resized: false,
            bg_texture: None,
            // 从持久化设置恢复经典直通状态（第十八轮）。
            // settings.classic_mode 在 ToggleFallback 路径同步写盘，重启后还原。
            force_fallback: init_force_fallback,
            #[cfg(feature = "input-editor")]
            history: history_store,
            #[cfg(feature = "input-editor")]
            ghost_cache: (0, None),
            #[cfg(feature = "input-editor")]
            completion_candidates: Vec::new(),
            #[cfg(feature = "input-editor")]
            completion_sidecar: completion_sidecar::CompletionSidecar::new(self.proxy.clone()),
            #[cfg(feature = "input-editor")]
            completion_req_id: 0,
            #[cfg(feature = "input-editor")]
            footer_dragging: false,
            #[cfg(feature = "input-editor")]
            footer_drag_anchor: lumen_editor::Position::default(),
            #[cfg(feature = "input-editor")]
            footer_click_state: footer_mouse::ClickState::default(),
            #[cfg(feature = "input-editor")]
            footer_context_menu_at: None,
        };
        state.shell_state.settings.font_hint = font_hint;
        // 第十九轮：从持久化设置恢复文件树可见性初值。
        // sidebar_visible 直接驱动 if app_settings.layout.sidebar_visible { } 渲染分支，
        // 无需额外映射；filetree.visible 存于 ShellState（Default 硬编码 true），
        // 必须在此显式从 settings 读出。两入口（顶栏②按钮 + Ctrl+B）切换时均同步
        // 写盘（见 shell_out 处理段与 ToggleFiletree 分支），重启即可还原。
        state.shell_state.filetree.visible = state.settings.layout.filetree_visible;
        // 恢复条目中保存的 cwd 已失效：回退默认目录并提示一次（F4）。
        if stale_cwd > 0 {
            state.shell_state.toast.push(
                shell::toast::ToastKind::Warn,
                i18n::fmt1(i18n::strings().toast_stale_cwd_fmt, stale_cwd),
            );
        }
        // 启动时加载背景图（P13）：enabled 且有 path 时解码上传 GPU。
        if state.settings.appearance.background.enabled
            && state.settings.appearance.background.path.is_some()
        {
            state.apply_background_image();
        }
        // 窗口标题对齐激活会话（恢复多会话时 active 可能非 0）。
        state.update_window_title();

        // M3.8 批2 Snap Layouts 子类化：窗口创建后安装子类过程。
        // 失败时记 warn 日志并继续（Snap 是增强功能，不影响应用主体逻辑）。
        // 取 HWND：winit 使用 rwh_06，HasWindowHandle trait 提供 window_handle()。
        // raw-window-handle 0.6 中 Win32WindowHandle.hwnd 字段类型为 NonZeroIsize，
        // 调用 .get() 取出 isize 值传入 install。
        #[cfg(target_os = "windows")]
        {
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match state.window.window_handle() {
                Ok(handle) => {
                    if let RawWindowHandle::Win32(wh) = handle.as_raw() {
                        // SAFETY: hwnd 来自 winit 刚创建的有效窗口（本函数内），
                        // 在 init 返回前窗口不会被销毁，时序成立。
                        let hwnd = wh.hwnd.get(); // NonZeroIsize::get() → isize，即 Win32 HWND 值
                        if unsafe { snap_layouts::install(hwnd) } {
                            log::info!("Snap Layouts 子类过程安装成功（hwnd={hwnd:#x}）");
                        } else {
                            log::warn!(
                                "Snap Layouts 子类过程安装失败，SetWindowSubclass 返回 FALSE"
                            );
                        }
                    } else {
                        log::warn!("Snap Layouts 子类化跳过：非 Win32 窗口句柄");
                    }
                }
                Err(e) => {
                    log::warn!("Snap Layouts 子类化跳过：获取 window_handle 失败（{e}）");
                }
            }
        }

        Ok(state)
    }
}

impl ApplicationHandler<PtyWake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_none() {
            match self.init(event_loop) {
                Ok(state) => self.state = Some(state),
                Err(e) => {
                    error!("初始化失败: {e:#}");
                    event_loop.exit();
                }
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: PtyWake) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        // F8 前台化：第二实例被单实例锁拒掉前发来的请求——恢复最小
        // 化并请求焦点（Windows 限制跨进程抢前台，focus_window 可能
        // 只闪任务栏，request_user_attention 兜底向用户示意）。
        if single_instance::take_foreground_request() {
            info!("处理前台化请求：set_minimized(false) + focus_window + request_user_attention");
            state.window.set_minimized(false);
            state.window.focus_window();
            state
                .window
                .request_user_attention(Some(winit::window::UserAttentionType::Informational));
        }
        if state.tabs.is_empty() {
            return; // 退出流程中（exit 后仍可能有滞后事件）
        }
        // 先清挂起标志再 drain：drain 期间新到的数据会触发下一个 wake，不丢。
        state.wake_pending.store(false, Ordering::Release);

        let drain_t0 = Instant::now();
        let active_tab = state.active_tab;
        let focused = state.tabs[active_tab].focused;
        let pane_counts: Vec<usize> = state.tabs.iter().map(|t| t.panes.len()).collect();
        // per-session 通道按「焦点优先」轮询（需求池 P5 + F5 分屏）：
        // 先清焦点窗格的通道——其量受前台回显/输出规模天然限制且
        // 消化快于产出；其余窗格（含激活 tab 的可见兄弟窗格）按
        // BG_DRAIN_CAP 配额逐个消化，超限事件留在各自通道里由有界
        // 容量反压各自的读线程，互不连坐。可见窗格本就按合帧节拍上
        // 屏，配额只是把单轮消化切片，洪泛不抢占焦点窗格的打字。旧
        // 的全局单通道下前台回显最坏要排在 ~2MB 洪泛之后（队头阻塞，
        // 延迟尖峰 10~30ms）。
        let order = drain_order(&pane_counts, active_tab, focused);
        // 每窗格本轮已消化字节数 / 是否有新数据（按 order 下标）。
        let mut consumed = vec![0usize; order.len()];
        let mut got_data = vec![false; order.len()];
        let mut exited: Vec<SessionId> = Vec::new();
        // 非焦点窗格超出本轮配额提前停手（需补发 wake 续处理）。
        let mut backlog = false;
        for (k, &(ti, pi)) in order.iter().enumerate() {
            let is_focused = ti == active_tab && pi == focused;
            // Receiver 克隆一份（Arc 浅拷贝）避免循环内长借用 state。
            let rx = state.tabs[ti].panes[pi].rx.clone();
            loop {
                if !is_focused && consumed[k] >= BG_DRAIN_CAP {
                    // 本轮配额用尽：剩余留到补发的下一个 wake 再消化，
                    // 前台打字不被 yes 级输出抢占主线程。
                    backlog = true;
                    break;
                }
                let Ok(ev) = rx.try_recv() else {
                    break;
                };
                match ev {
                    PtyEvent::Data(bytes) => {
                        consumed[k] += bytes.len();
                        // 调试辅助：LUMEN_VT_LOG=<路径> 时把 PTY 原始字节追加到文件。
                        if let Ok(path) = std::env::var("LUMEN_VT_LOG") {
                            use std::io::Write;
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(path)
                            {
                                let _ = f.write_all(&bytes);
                            }
                        }
                        // 取证设施（B3）：LUMEN_DUMP_PTY=<dir> 时按会话 id
                        // 把原始字节流追加写入 <dir>/pane-<id>.bin，同时把
                        // 可读的转义序列表示追加写入 <dir>/pane-<id>.txt。
                        // 环境变量门控，零开销（仅读一次 env，实际写盘在
                        // 条件分支内），长期保留供现场取证用。
                        if let Ok(dir) = std::env::var("LUMEN_DUMP_PTY") {
                            let sid = state.tabs[ti].panes[pi].id;
                            use std::io::Write;
                            let bin_path = format!("{dir}/pane-{sid}.bin");
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&bin_path)
                            {
                                let _ = f.write_all(&bytes);
                            }
                            let txt_path = format!("{dir}/pane-{sid}.txt");
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&txt_path)
                            {
                                // 人类可读格式：控制字符/转义序列以 <XX> 或
                                // <ESC[...X> 表示，普通可打印字符原样输出。
                                let _ = f.write_all(dump_pty_readable(&bytes).as_bytes());
                            }
                        }
                        state.tabs[ti].panes[pi].term.advance(&bytes);
                        got_data[k] = true;

                        // —— 块闭合探针（M4.1 批D2）——
                        // advance() 处理 OSC 133;D 后会新增已闭合块；
                        // 探针用已见闭合块数与当前闭合块数比对。
                        #[cfg(feature = "input-editor")]
                        {
                            // 先收集所有需要处理的数据（不持有不可变借用）。
                            let (closed_now, new_block_data): (usize, Vec<(u64, Option<i32>)>) = {
                                let pane = &state.tabs[ti].panes[pi];
                                let blocks = pane.term.blocks();
                                let closed_blocks: Vec<_> =
                                    blocks.iter().filter(|b| b.is_closed()).collect();
                                let closed_now = closed_blocks.len();
                                let last = pane.last_seen_closed_blocks;
                                let new_data: Vec<(u64, Option<i32>)> = if closed_now > last {
                                    closed_blocks[last..]
                                        .iter()
                                        .map(|b| (b.id, b.exit_code))
                                        .collect()
                                } else {
                                    Vec::new()
                                };
                                (closed_now, new_data)
                            };

                            if !new_block_data.is_empty() {
                                // 耗时：从 pending_submit 的提交时刻到现在（在写 pane 前取）。
                                let duration_ms = state.tabs[ti].panes[pi]
                                    .pending_submit
                                    .as_ref()
                                    .map(|(_, t, _)| t.elapsed().as_millis() as u64)
                                    .unwrap_or(0);
                                // 取 pending_submit 中的文本和 history_idx（clone 脱离借用）。
                                let pending = state.tabs[ti].panes[pi].pending_submit.clone();

                                for (block_id, exit_code) in &new_block_data {
                                    // 设置退出码角标（仅 Compose 态会显示，Running 态下也存，
                                    // 下一次进入 Compose 态时 badge 仍在，按任意键清除）。
                                    if let Some(code) = exit_code {
                                        state.tabs[ti].panes[pi].exit_badge =
                                            Some(lumen_renderer::composer_view::ExitBadge {
                                                exit_code: *code,
                                                duration_ms,
                                            });
                                    }
                                    // 历史库回填 exit_code + duration_ms
                                    if let Some((ref submitted_text, _, history_idx)) = pending {
                                        // 取当前库中该条目的 ts（用于 text+ts 匹配校验）
                                        let ts = state
                                            .history
                                            .entries()
                                            .get(history_idx)
                                            .map(|e| e.ts)
                                            .unwrap_or(0);
                                        state.history.backfill(
                                            history_idx,
                                            submitted_text,
                                            ts,
                                            exit_code.unwrap_or(-1),
                                            duration_ms,
                                        );
                                    }
                                    log::debug!(
                                        "[BlockClosed] block_id={block_id} exit_code={exit_code:?} duration_ms={duration_ms}"
                                    );
                                }
                                // 回填完成后清 pending_submit（仅清一次，即使多块闭合）。
                                if pending.is_some() {
                                    state.tabs[ti].panes[pi].pending_submit = None;
                                }
                                state.tabs[ti].panes[pi].last_seen_closed_blocks = closed_now;
                            }
                        }
                    }
                    PtyEvent::Exited => exited.push(state.tabs[ti].panes[pi].id),
                }
            }
        }

        // —— 每窗格的批后处理：应答回写对所有窗格照常执行（后台不回
        // 写 DSR/DA 会卡死对端程序）；渲染调度对激活 tab 的**全部可见
        // 窗格**生效（与后台 tab 的本质区别：可见窗格都要上屏），后台
        // tab 的窗格只更新 ESU 标记并标未读点。
        let mut focused_stats = None;
        // 后台窗格未读点 false→true 翻转：侧栏需要一次重绘（仅翻转
        // 那次请求，已置位后的后续批次不再重复，保持「后台 drain 不
        // 打扰前台渲染节拍」的原设计）。
        let mut needs_shell_redraw = false;
        // 循环内长借用 state.tabs：限频基准先拷出、重绘请求收集为标志。
        let last_term_render_at = state.last_term_render_at;
        let mut want_redraw = false;
        for (k, &(ti, pi)) in order.iter().enumerate() {
            if !got_data[k] {
                continue;
            }
            let visible = ti == active_tab;
            // 最大化期间被隐藏的激活 tab 窗格（P14）：照常消化与回写
            // （下方），但不参与渲染调度（同「后台 tab 不渲染」闸门）；
            // 也不标未读点——所属 tab 本身可见，激活态下挂未读点既
            // 矛盾也无法被 activate() 清除。
            let hidden_by_max = visible && state.tabs[ti].maximized.is_some_and(|m| m != pi);
            let s = &mut state.tabs[ti].panes[pi];
            // 终端应答（DSR/DA/DECRQM 等）回写给 shell。
            let resp = s.term.take_responses();
            if !resp.is_empty() {
                let _ = s.pty.write(&resp);
            }
            // 进入备用屏幕（vim/codex 全屏）时块交互无意义且不可见，
            // 清掉选中态，避免 Ctrl+C 被残留选中块吞成「复制」。
            if s.term.is_alt_screen() && s.selected_block.is_some() {
                s.selected_block = None;
            }
            let sync = s.term.is_synchronized();
            let esu_mark = s.term.esu_mark();
            let frame_completed = esu_mark != s.last_esu_mark && !sync;
            s.last_esu_mark = esu_mark;

            if !visible {
                if !s.has_unseen_output {
                    s.has_unseen_output = true;
                    needs_shell_redraw = true;
                }
                continue;
            }
            if hidden_by_max {
                continue;
            }
            if pi == focused {
                focused_stats = Some((sync, frame_completed, s.term.cursor_unsettled()));
            }
            if frame_completed {
                // 本批完成了 DEC 2026 同步帧：协议语义就是「立即原子
                // 呈现」，零等待直接渲染（codex 打字回显走这条快路）。
                // 但渲染频率以 ~8ms 为下限：极速输入（百帧每秒级回显）
                // 时把积压帧合并，避免渲染请求超出显示能力拖垮主线程。
                // 限频基准用 last_term_render_at（终端帧时间戳，整帧
                // 粒度——多窗格同帧渲染共享一个基准）：鼠标驱动的纯
                // UI 重绘不该反向推迟完成帧的上屏。
                let now = Instant::now();
                let recent = last_term_render_at
                    .filter(|t| now.duration_since(*t) < Duration::from_millis(8));
                if let Some(last) = recent {
                    let at = last + Duration::from_millis(8);
                    s.redraw_at = Some(at);
                    s.redraw_hard_at = None;
                    s.redraw_abs_at = Some(at + Duration::from_millis(50));
                } else {
                    s.redraw_at = None;
                    s.redraw_hard_at = None;
                    s.redraw_abs_at = None;
                    // 直渲请求是「欠帧」：到 RedrawRequested 执行前若有
                    // 新 BSU 批到达重新拉起同步区间，门控可暂缓这帧、
                    // 交给重新武装的渲染计划在 ESU 后补画完整帧（直接
                    // 放行会把半成品画上屏——蓝条闪烁，需求池 P1）；
                    // 暂缓以欠帧起点 + REDRAW_ABS_CAP 为限，见门控注释。
                    s.term_frame_due_since.get_or_insert(now);
                    want_redraw = true;
                }
            } else {
                // 无同步协议的流（普通 shell/claude）：静默合帧，每批
                // 数据推后渲染时刻，流停了才画（见 about_to_wait）；
                // 硬上限自首批起算，保障刷新率。
                let now = Instant::now();
                s.redraw_at = Some(now + REDRAW_DEBOUNCE);
                if s.redraw_hard_at.is_none() {
                    s.redraw_hard_at = Some(now + REDRAW_HARD_CAP);
                    s.redraw_abs_at = Some(now + REDRAW_ABS_CAP);
                }
            }
        }
        // 窗口标题跟随焦点窗格（OSC 标题/cwd 可能随本批数据更新）。
        if focused_stats.is_some() {
            state.update_window_title();
        }
        // 会话快照持久化（F4）：任一窗格的 cwd（OSC 9;9）可能随本批
        // 数据更新，与上次写盘快照比对后按需落盘（实际写频≈用户 cd
        // 频率，比对不同才写）。
        if got_data.iter().any(|&b| b) {
            state.persist_sessions();
        }
        if want_redraw || needs_shell_redraw {
            state.window.request_redraw();
        }
        let total: usize = consumed.iter().sum();
        if total > 0 {
            let (sync, fc, unsettled) = focused_stats.unwrap_or_default();
            state.perf_log(format_args!(
                "drain {total}B 耗时 {:?} sync={sync} esu帧={fc} unsettled={unsettled} 后台积压={backlog}",
                drain_t0.elapsed()
            ));
        }

        // —— 生命周期：shell 退出（海风哥 2026-06-13 体验优化）——
        // 多窗格：关闭退出的那格（F5：剩余窗格继续）；
        // 单窗格：原位重启一个新 shell，不关应用（单窗口 `exit` 后立即
        // 换一个干净 PowerShell 继续用，省去重开 app）。
        for sid in exited {
            let Some((ti, pi)) = state.find_pane(sid) else {
                continue;
            };
            if state.tabs[ti].panes.len() > 1 {
                info!("会话 id={sid} 的 shell 已退出（多窗格），关闭该窗格");
                // 多窗格时 close_pane 必返回 false（不会退出应用）。
                state.close_pane(ti, pi);
            } else {
                info!("会话 id={sid} 的 shell 已退出（单窗格），原位重启新 shell");
                if state.respawn_pane(ti, pi) {
                    info!("重启失败、最后会话已关闭，退出应用");
                    event_loop.exit();
                    return;
                }
            }
        }

        // 后台数据滞留：补发一个 wake 接着消化（与转发线程同一套去重）。
        if backlog
            && !state.wake_pending.swap(true, Ordering::AcqRel)
            && state.proxy.send_event(PtyWake).is_err()
        {
            error!("补发 PtyWake 失败：事件循环已关闭");
        }

        // M4.4 批2：drain sidecar 命令补全响应，合并进候选列表。
        #[cfg(feature = "input-editor")]
        {
            let responses = state.completion_sidecar.poll();
            let mut sidecar_merged = false;
            for resp in responses {
                // 丢弃过期响应（id 不匹配当前在途请求）。
                if resp.id != state.completion_req_id || state.completion_req_id == 0 {
                    continue;
                }
                if resp.items.is_empty() {
                    continue;
                }
                // 取当前行文本（用于 char→byte 换算）。
                let line_text = {
                    let ti = state.active_tab;
                    let pi = state.tabs[ti].focused;
                    let view = state.tabs[ti].panes[pi].editor.view();
                    let cur = view.cursor();
                    view.line(cur.line).to_owned()
                };
                // 把 sidecar 候选转成 Completion，按 display 去重后追加。
                // 先收集已有 display 字符串（owned），释放借用后再 push。
                let existing_displays: std::collections::HashSet<String> = state
                    .completion_candidates
                    .iter()
                    .map(|c| c.display.clone())
                    .collect();
                // char→byte 区间只算一次（resp 内所有候选共享同一 ri/rl）。
                let replace_range = Some(completion_sidecar::char_range_to_bytes(
                    &line_text, resp.ri, resp.rl,
                ));
                let mut new_items: Vec<completion::Completion> = Vec::new();
                for item in &resp.items {
                    if item.text.is_empty() {
                        continue;
                    }
                    // ProviderContainer = 目录。
                    let is_dir = item.kind == "ProviderContainer";
                    // display 与 replacement 统一使用 item.text。
                    let display = if is_dir && !item.text.ends_with('/') {
                        format!("{}/", item.text)
                    } else {
                        item.text.clone()
                    };
                    if existing_displays.contains(&display) {
                        continue; // 去重：与文件路径候选同名的跳过。
                    }
                    new_items.push(completion::Completion {
                        display,
                        replacement: item.text.clone(),
                        is_dir,
                        replace_range,
                    });
                }
                state.completion_candidates.extend(new_items);
                sidecar_merged = true;
            }
            if sidecar_merged && !state.completion_candidates.is_empty() {
                // 若弹窗尚未打开（文件路径候选为空、等 sidecar），现在打开。
                if !state.shell_state.completion.open {
                    state.shell_state.completion.open = true;
                    state.shell_state.completion.selected = 0;
                    state.terminal_focused = false;
                }
                state.window.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.tabs.is_empty() {
            return; // 退出流程中
        }
        // 渲染调度看激活 tab **全部窗格**的计划（后台 tab 的窗格不设
        // 计划、不打扰渲染）。逐窗格判定：
        // - 计划未到点 → 计入下次唤醒时刻（取最早）；
        // - 到点但正处于同步区间且 abs 兜底未到、欠帧未超龄 → 顺延
        //   （小步 2ms 等 ESU，原单会话语义）；
        // - 到点且可渲染 → 清计划、记欠帧起点，本轮立即请求重绘
        //   （其余仍在同步区间的窗格由 RedrawRequested 的逐窗格门控
        //   各自跳过，保留上一完整帧）。
        let now = Instant::now();
        // 未到点计划中的最早时刻（含 egui 计划）。
        let mut wake: Option<Instant> = None;
        // 任一窗格到点且可渲染 → 立即重绘。
        let mut fire = false;
        // 有到点但被同步区间顺延的窗格。
        let mut deferred = false;
        for s in &mut state.tabs[state.active_tab].panes {
            // 终端渲染时刻 = 静默窗口与强制刷新中先到者。
            let Some(t) = s
                .redraw_at
                .map(|soft| s.redraw_hard_at.map_or(soft, |h| soft.min(h)))
            else {
                continue;
            };
            if now < t {
                wake = Some(wake.map_or(t, |w| w.min(t)));
                continue;
            }
            // 到点但正处于同步区间：小步顺延等帧完成（ESU 通常随下一
            // 批数据立刻到达），但不超过绝对兜底时刻；欠帧已超龄（上
            // 轮被门控暂缓、又熬过了一个 REDRAW_ABS_CAP）则不再顺延。
            if s.term.is_synchronized()
                && s.redraw_abs_at.is_some_and(|a| now < a)
                && s.term_frame_due_since
                    .is_none_or(|d| now.duration_since(d) < REDRAW_ABS_CAP)
            {
                deferred = true;
                continue;
            }
            // 到点且可渲染：清计划（只清自己的，不连带其他窗格）。
            s.redraw_at = None;
            s.redraw_hard_at = None;
            s.redraw_abs_at = None;
            // 计划到点 = 欠一帧终端渲染。计划已清空，若执行重绘前新
            // 数据又拉起同步区间（abs 重新武装到未来），同步门控可暂
            // 缓这帧等 ESU 补画，但暂缓以本起点 + REDRAW_ABS_CAP 为限
            // （绝对兜底到点的强制渲染不许被无限顺延吃掉）。
            s.term_frame_due_since.get_or_insert(now);
            fire = true;
        }
        // egui 重绘计划（动画等）独立成项：到点即清并请求重绘——
        // 例外是「终端窗格全部顺延中且无其他到点窗格」时跟着顺延
        // （2ms 粒度，对 UI 动画无感），避免把半成品终端帧画上屏
        // （原单会话语义）。
        match state.egui_repaint_at {
            Some(e) if now >= e => {
                if fire || !deferred {
                    state.egui_repaint_at = None;
                    fire = true;
                }
            }
            Some(e) => wake = Some(wake.map_or(e, |w| w.min(e))),
            None => {}
        }

        if fire {
            // 重绘在途；ControlFlow 显式回 Wait（粘性的 WaitUntil(过去
            // 时刻) 会让事件循环全速空转，历史事故见 git log）。
            event_loop.set_control_flow(ControlFlow::Wait);
            state.window.request_redraw();
            return;
        }
        if deferred {
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_millis(2)));
            return;
        }
        match wake {
            Some(t) => event_loop.set_control_flow(ControlFlow::WaitUntil(t)),
            // 没有任何待渲染计划时必须显式回到 Wait：ControlFlow 是粘
            // 性的，残留的 WaitUntil(过去时刻) 会让事件循环全速空转
            // （曾导致 ESU 直渲后单核拉满、键盘处理抖动、conhost 被抢
            // CPU）。
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.tabs.is_empty() {
            return; // 退出流程中（exit 后仍可能有滞后事件）
        }

        // —— egui 先行消化事件 ——
        // 终端聚焦时键盘与 IME 整体绕过 egui：Tab/方向键不被 egui 的
        // 焦点导航偷走、IME 提交不被双投。其余事件先喂 egui（面板悬停
        // 高亮、按钮交互都靠它），Resized/CloseRequested 等窗口级事件
        // egui 看过后仍由我们自行处理。
        // RedrawRequested 绝不喂 egui：egui-winit 对它一律返回
        // repaint:true，照做 request_redraw 会形成「重绘请求自循环」，
        // 事件循环全速空转单核拉满（实测踩过，性质同 main.rs 历史上的
        // ControlFlow 粘性空转事故）。
        // 注：resp.consumed 在本布局下对鼠标无判别力（终端区被
        // CentralPanel 覆盖，悬停即视为「在 egui 区域上」）——鼠标按
        // 终端区矩形路由（mouse_in_term），键盘/IME 按 terminal_focused
        // 路由，不依赖 consumed。
        let bypass_egui = matches!(event, WindowEvent::RedrawRequested)
            || (state.terminal_focused
                && matches!(
                    event,
                    WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_)
                ))
            // 诊断开关（B1）：无交互桌面的自动化环境里物理光标不在窗口
            // 内，每个注入的 WM_MOUSEMOVE 都伴随系统补发的 WM_MOUSELEAVE
            // （TrackMouseEvent 语义），egui 的指针态被清空导致注入的
            // 按下被丢弃。设 LUMEN_DIAG_IGNORE_CURSOR_LEFT=1 时不把
            // CursorLeft 喂给 egui（仅自动化拖动测试用，正常使用不设）。
            || (matches!(event, WindowEvent::CursorLeft { .. })
                && std::env::var_os("LUMEN_DIAG_IGNORE_CURSOR_LEFT").is_some());
        if !bypass_egui {
            let resp = state.egui_state.on_window_event(&state.window, &event);
            if resp.repaint {
                // 事件驱动重绘的 8ms 合帧下限：egui-winit 对几乎一切
                // 输入事件（含 CursorMoved）都返回 repaint:true，高回报
                // 率鼠标（1000Hz）划过窗口时无脑 request_redraw 会让每
                // 个事件循环迭代渲染一帧（Mailbox 非阻塞呈现不被垂直
                // 同步限速），主线程被渲染占满、打字处理被挤——与 ESU
                // 直渲同款的退化。距上帧不足 8ms 时合入 egui_repaint_at
                // 计划，由 about_to_wait 统一调度（复用同步区间顺延与
                // ControlFlow 复位逻辑，不会空转）。
                let now = Instant::now();
                let recent = state
                    .last_render_at
                    .filter(|t| now.duration_since(*t) < Duration::from_millis(8));
                if let Some(last) = recent {
                    let at = last + Duration::from_millis(8);
                    state.egui_repaint_at = Some(state.egui_repaint_at.map_or(at, |e| e.min(at)));
                } else {
                    state.window.request_redraw();
                }
            }
        }

        match event {
            WindowEvent::CloseRequested => {
                // 退出前以此刻的会话列表落盘（F4）：正常运行中每次
                // 变更已即时写盘，这里兜底拿住「最后一次变更与关窗
                // 之间」的状态（快照一致时内部自动跳过）。
                state.persist_sessions();
                // 命令历史库：原子重写磁盘（去重 + 截断到 MAX_ENTRIES）。
                // 失败只记 warn，不阻断退出。（M4.1 批D2）
                #[cfg(feature = "input-editor")]
                state.history.flush_on_exit();
                event_loop.exit();
            }
            WindowEvent::ModifiersChanged(mods) => state.modifiers = mods.state(),
            WindowEvent::ThemeChanged(t) => {
                // 系统深浅模式切换（P12 Sync with OS）：记录新状态；
                // 开启跟随时即时切到对应槽位主题。不写盘——设置本身
                // 没变，变的是系统侧。
                let dark = t == winit::window::Theme::Dark;
                if state.os_dark != dark {
                    state.os_dark = dark;
                    info!("系统主题切换：{}", if dark { "深色" } else { "浅色" });
                    if state.settings.appearance.sync_with_os {
                        state.apply_theme();
                        state.window.request_redraw();
                    }
                }
            }
            WindowEvent::Resized(size) => {
                state.renderer.resize_surface(size.width, size.height);
                // B3-8：整窗 resize 标志——通知下一帧 RedrawRequested
                // 穿透 divider_resize_held 门控，确保 term/PTY resize
                // 必达。整窗 resize 是 OS 级事件，与分隔条拖动无关。
                state.window_just_resized = true;
                // 终端行列数跟随 egui 布局出的终端区矩形，统一在
                // RedrawRequested 里检测变化并 resize（离屏纹理同步重建）。
                state.window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // DPI 迁移（跨显示器拖动/改系统缩放）：egui-winit 已在
                // 上方消化此事件更新 pixels_per_point，渲染器侧的缩放
                // 与字体度量必须同步更新，否则终端文字物理字号永久停
                // 在启动时的 DPI（且设置页改字号也按错误 DPI 生效）。
                // 行列数重算、全会话 resize、离屏重建由下一帧
                // RedrawRequested 的矩形/网格对照检查自动完成（与设置
                // 页改字号同链路）；伴随的 Resized 事件已有分支处理
                // surface 重配。
                // B3-8：DPI 变更也是 OS 级 resize，同样需要穿透
                // divider_resize_held 门控（伴随的 Resized 一般也会置
                // 此标志，双保险无妨）。
                state.window_just_resized = true;
                state.renderer.set_scale_factor(scale_factor as f32);
                let ap = &state.settings.appearance;
                state
                    .renderer
                    .reconfigure_font(&ap.font_family, ap.font_size);
                state.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // —— M4.1 批B：事件 → keymap 查表 → Action → dispatch ——
                //
                // 原八层 if-else 拦截链已全部平移进 keymap 静态表
                // （crates/lumen-app/src/keymap.rs）。此处为「瘦身后」
                // 的入口：组装 GuardState、查表、执行结果。
                //
                // 无法入表的特例（说明为什么不入表）：
                // 1. IME：Ime::Commit / Ime::Preedit 事件走 WindowEvent::Ime
                //    分支（下方），不经过 KeyboardInput，故不在此表内。
                // 2. 重命名文本输入：重命名编辑中键盘归 egui 输入框，
                //    terminal_focused=false 的闸已拦住，keymap 中 renaming
                //    守卫只影响外壳快捷键层，无需单独入表。
                // 3. login.open 期间外壳快捷键全部静默：由 overlay_open
                //    守卫 + terminal_focused=false 联合处理，符合设计稿。

                let pressed = event.state == ElementState::Pressed;
                let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);

                // 组装守卫状态（从 AppState 采样，不缓存）。
                let guard = keymap::GuardState {
                    has_selection: state.tabs[ti].panes[pi]
                        .selection
                        .as_ref()
                        .is_some_and(|s| !s.is_empty()),
                    has_selected_block: state.tabs[ti].panes[pi].selected_block.is_some(),
                    is_alt_screen: state.tabs[ti].panes[pi].term.is_alt_screen(),
                    overlay_open: state.shell_state.settings.open
                        || state.shell_state.login.open
                        || state.shell_state.history_search.open
                        || state.shell_state.completion.open,
                    renaming: state.shell_state.renaming.is_some(),
                    filetree_dialog_open: state.shell_state.filetree.dialog_open(),
                    terminal_focused: state.terminal_focused,
                    win32_input: state.tabs[ti].panes[pi].term.win32_input()
                        && std::env::var_os("LUMEN_WIN32_INPUT").is_some(),
                    // M4.1 批D1：Compose 态编辑器缓冲是否为空（影响 Ctrl+C / Ctrl+D）
                    #[cfg(feature = "input-editor")]
                    compose_buf_empty: state.tabs[ti].panes[pi].editor.view().text().is_empty(),
                    #[cfg(not(feature = "input-editor"))]
                    compose_buf_empty: true,
                    // M4.1 批D2：光标所在行位置（影响 ↑/↓ 历史导航 vs 多行移动分流）
                    #[cfg(feature = "input-editor")]
                    compose_cursor_at_first_line: {
                        let view = state.tabs[ti].panes[pi].editor.view();
                        view.cursor().line == 0
                    },
                    #[cfg(not(feature = "input-editor"))]
                    compose_cursor_at_first_line: true,
                    #[cfg(feature = "input-editor")]
                    compose_cursor_at_last_line: {
                        let view = state.tabs[ti].panes[pi].editor.view();
                        let lc = view.line_count();
                        view.cursor().line == lc.saturating_sub(1)
                    },
                    #[cfg(not(feature = "input-editor"))]
                    compose_cursor_at_last_line: true,
                    // M4.1 批3：光标在文档末尾（末行字节偏移 = 末行长度）
                    #[cfg(feature = "input-editor")]
                    compose_cursor_at_doc_end: {
                        let view = state.tabs[ti].panes[pi].editor.view();
                        let cur = view.cursor();
                        let lc = view.line_count();
                        let at_last = cur.line == lc.saturating_sub(1);
                        if at_last {
                            // 末行最后字节偏移 = 末行字节长度
                            let last_line_len = view
                                .lines()
                                .nth(lc.saturating_sub(1))
                                .map(|l| l.len())
                                .unwrap_or(0);
                            cur.byte == last_line_len
                        } else {
                            false
                        }
                    },
                    // M4.1 批3：ghost 是否非空（缓存命中时复用，否则重算）
                    #[cfg(feature = "input-editor")]
                    ghost_exists: {
                        let rev = state.tabs[ti].panes[pi].editor.revision();
                        if state.ghost_cache.0 != rev {
                            let text = state.tabs[ti].panes[pi].editor.view().text();
                            let ghost = if text.contains('\n') || text.is_empty() {
                                None
                            } else {
                                state.history.find_ghost_prefix(&text)
                            };
                            state.ghost_cache = (rev, ghost);
                        }
                        state.ghost_cache.1.is_some()
                    },
                    // 第十一轮：编辑器选区非空（Ctrl+C 第一级 / Ctrl+X 判断）
                    #[cfg(feature = "input-editor")]
                    has_editor_selection: state.tabs[ti].panes[pi].editor.view().has_selection(),
                };

                // 求值当前有效输入模式（纯推导，不缓存）。
                let mode =
                    mode::effective_mode(&state.tabs[ti].panes[pi].term, state.force_fallback);

                // 查表。
                let result = keymap::lookup(&event, state.modifiers, mode, pressed, &guard);

                // 任意按键命中 → 清退出码角标（设计稿 §3.2 第⑥步，M4.1 批D2）。
                // 仅 Compose 态有 exit_badge；result=None（keymap 拦截）时也清，
                // 防止角标因未命中的修饰键抬起而留住。
                #[cfg(feature = "input-editor")]
                if result.is_some() {
                    state.tabs[ti].panes[pi].exit_badge = None;
                }

                match result {
                    None => {
                        // keymap 未命中（通常是 terminal_focused=false 的闸），
                        // 不写 PTY。
                    }

                    Some(keymap::LookupResult::ShellAction(shell_action)) => {
                        // 外壳级动作：不走 dispatch，直接执行外壳逻辑。
                        use keymap::ShellAction;
                        match shell_action {
                            ShellAction::NewPane => {
                                state.new_pane();
                            }
                            ShellAction::ClosePane => {
                                if state.close_pane(ti, pi) {
                                    info!("最后一个会话已关闭，退出应用");
                                    event_loop.exit();
                                }
                            }
                            ShellAction::ToggleMaximizePane => {
                                let focused = state.tabs[state.active_tab].focused;
                                state.toggle_maximize_pane(focused);
                            }
                            ShellAction::ToggleSettings => {
                                // 登录覆盖层打开时不响应（键盘归 egui）。
                                if !state.shell_state.login.open {
                                    if state.shell_state.settings.open {
                                        state.shell_state.settings.open = false;
                                        state.terminal_focused = true;
                                    } else {
                                        state.shell_state.settings.open_with(&state.settings);
                                        state.terminal_focused = false;
                                    }
                                    state.window.request_redraw();
                                }
                            }
                            ShellAction::NewTab => {
                                // 设置页打开时不响应（避免在覆盖层背后偷偷增删）。
                                if !state.shell_state.settings.open {
                                    state.new_tab();
                                }
                            }
                            ShellAction::CloseTab => {
                                if !state.shell_state.settings.open
                                    && state.close_tab(state.active_tab)
                                {
                                    info!("最后一个会话已关闭，退出应用");
                                    event_loop.exit();
                                }
                            }
                            ShellAction::ToggleFiletree => {
                                if !state.shell_state.settings.open {
                                    // 文件树开合：终端区宽度随之变化，下一帧
                                    // egui 布局产出新矩形并触发离屏重建+resize。
                                    let new_visible = !state.shell_state.filetree.visible;
                                    state.shell_state.filetree.visible = new_visible;
                                    // 第十九轮持久化：Ctrl+B 路径写盘，重启还原。
                                    // 与顶栏②按钮路径共用同一 settings 字段，两入口
                                    // 保持状态源一致（ShellState::filetree.visible）。
                                    state.settings.layout.filetree_visible = new_visible;
                                    if let Some(err) = state.settings.save() {
                                        state.shell_state.toast.push(
                                            shell::toast::ToastKind::Error,
                                            i18n::fmt1(
                                                i18n::strings().toast_settings_save_failed_fmt,
                                                &err,
                                            ),
                                        );
                                    }
                                    state.window.request_redraw();
                                }
                            }
                            ShellAction::CycleTab(dir) => {
                                if !state.shell_state.settings.open {
                                    state.cycle_tab(dir);
                                }
                            }
                        }
                    }

                    Some(keymap::LookupResult::Win32KeyUp) => {
                        // win32-input-mode 抬起事件：encode_key_win32(Kd=0) 写 PTY。
                        if let Some(bytes) = input::encode_key_win32(&event, state.modifiers, false)
                        {
                            if let Err(e) = state.tabs[ti].panes[pi].write_user_input(&bytes) {
                                error!("写入 PTY 失败（win32 key-up）: {e:#}");
                            }
                        }
                    }

                    Some(keymap::LookupResult::Consumed) => {
                        // 按键已消费（如 Ctrl+Shift+C 无选区），不写 PTY。
                    }

                    Some(keymap::LookupResult::ComposeTab) => {
                        // Compose 态 Tab：M4.4 批1 文件路径补全 + 批2 命令补全。
                        #[cfg(feature = "input-editor")]
                        {
                            let ti = state.active_tab;
                            let pi = state.tabs[ti].focused;
                            // 取当前行文本与光标字节偏移。
                            let (line_text, cursor_byte) = {
                                let view = state.tabs[ti].panes[pi].editor.view();
                                let cur = view.cursor();
                                let line = view.line(cur.line).to_owned();
                                (line, cur.byte)
                            };
                            let cwd = state.tabs[ti].panes[pi]
                                .term
                                .cwd()
                                .map(|p| p.to_path_buf())
                                .unwrap_or_else(|| std::path::PathBuf::from("."));
                            let (_, token) = completion::current_token(&line_text, cursor_byte);
                            let candidates = completion::complete_path(token, &cwd);

                            // 批2：计算光标的 char 偏移，发送 sidecar 命令补全请求。
                            // char 偏移 = line_text[..cursor_byte] 的 Unicode char 数。
                            let cursor_char = line_text[..cursor_byte.min(line_text.len())]
                                .chars()
                                .count();
                            let cwd_str = cwd.to_string_lossy();
                            let req_id =
                                state
                                    .completion_sidecar
                                    .request(&line_text, cursor_char, &cwd_str);
                            state.completion_req_id = req_id;

                            if candidates.is_empty() {
                                // 无文件路径候选，但命令补全可能异步到达：
                                // 先清候选列表、打开弹窗（空状态）等待 sidecar 响应；
                                // 若 sidecar 也无候选才降级提示。
                                // 此处先只清旧候选，弹窗在 sidecar 响应到达后开。
                                state.completion_candidates.clear();
                                // 无文件路径候选时先不开弹窗（等 sidecar），但不推 toast。
                            } else {
                                state.completion_candidates = candidates;
                                let comp = &mut state.shell_state.completion;
                                comp.open = true;
                                comp.selected = 0;
                                state.terminal_focused = false;
                                state.window.request_redraw();
                            }
                        }
                        // 无 input-editor feature 时沿用占位提示。
                        #[cfg(not(feature = "input-editor"))]
                        {
                            let s = i18n::strings();
                            state
                                .shell_state
                                .toast
                                .push(shell::toast::ToastKind::Info, s.toast_compose_tab_hint);
                        }
                    }

                    Some(keymap::LookupResult::ComposeHistorySearch) => {
                        // Compose 态 Ctrl+R：打开历史搜索面板（M4.3）。
                        let hs = &mut state.shell_state.history_search;
                        hs.open = true;
                        hs.query.clear();
                        hs.selected = 0;
                        hs.focus_query = true;
                        // 面板打开期间键盘归 egui，不进终端。
                        state.terminal_focused = false;
                        state.window.request_redraw();
                    }

                    Some(keymap::LookupResult::ComposeEsc) => {
                        // Compose 态 Esc：关浮层 / 清选区（D1 内不清编辑器文本）。
                        // 批D1：仅清选区；浮层（历史面板等）D2 接入。
                        state.tabs[ti].panes[pi].selection = None;
                        state.window.request_redraw();
                    }

                    // M4.1 批3：接受 ghost text（→/End 在文末 + ghost 非空）
                    // 把 ghost 后缀 InsertText 进编辑器，ghost_cache 顺带失效（revision 变）。
                    #[cfg(feature = "input-editor")]
                    Some(keymap::LookupResult::AcceptGhost) => {
                        if let Some(ghost) = state.ghost_cache.1.take() {
                            state.ghost_cache.0 = 0; // 让缓存在下帧重算
                            state.dispatch(
                                action::Action::Edit(action::EditAction::InsertText(ghost)),
                                ti,
                                pi,
                            );
                            state.last_key_at = Some(Instant::now());
                        }
                    }

                    Some(keymap::LookupResult::TerminalAction(action)) => {
                        // 经由 dispatch 执行终端 Action。
                        state.dispatch(action, ti, pi);
                        // 按键记录（端到端延迟埋点）。
                        state.last_key_at = Some(Instant::now());
                    }

                    Some(keymap::LookupResult::PassThrough) => {
                        // 兜底直通：encode_key / encode_key_win32 编码后写 PTY。
                        let use_win32 = state.tabs[ti].panes[pi].term.win32_input()
                            && std::env::var_os("LUMEN_WIN32_INPUT").is_some();
                        let bytes = if use_win32 {
                            input::encode_key_win32(&event, state.modifiers, true)
                        } else {
                            input::encode_key(&event, state.modifiers)
                        };
                        if let Some(bytes) = bytes {
                            state.tabs[ti].panes[pi].term.grid_mut().scroll_to_bottom();
                            let write_t0 = Instant::now();
                            if let Err(e) = state.tabs[ti].panes[pi].write_user_input(&bytes) {
                                error!("写入 PTY 失败: {e:#}");
                            }
                            state.last_key_at = Some(write_t0);
                            state.perf_log(format_args!("key 写入耗时 {:?}", write_t0.elapsed()));
                        }
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                state.mouse_pos = (position.x, position.y);

                // ── footer 拖选跟踪（第十一轮，input-editor feature）────
                #[cfg(feature = "input-editor")]
                if state.footer_dragging {
                    if let Some((rel_x, rel_y, cell_w, cell_h, fp, lines)) =
                        state.mouse_footer_relative()
                    {
                        let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
                        let cursor_pos = footer_mouse::clamped_position(
                            rel_x, rel_y, cell_w, cell_h, fp, &line_refs,
                        );
                        let anchor = state.footer_drag_anchor;
                        let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);
                        let old_sel = state.tabs[ti].panes[pi].editor.view().selection();
                        let new_sel = lumen_editor::Selection {
                            anchor,
                            cursor: cursor_pos,
                        };
                        if old_sel != new_sel {
                            state.dispatch(
                                action::Action::Edit(action::EditAction::SetSelection(
                                    action::Selection {
                                        anchor: action::Position {
                                            line: anchor.line,
                                            byte: anchor.byte,
                                        },
                                        head: action::Position {
                                            line: cursor_pos.line,
                                            byte: cursor_pos.byte,
                                        },
                                    },
                                )),
                                ti,
                                pi,
                            );
                        }
                    }
                    // 不再走终端拖选
                } else if state.focused_pane().selecting {
                    // 终端区拖选跟随焦点窗格：端点按窗格矩形换算（cell_at 已
                    // 夹紧，拖出窗格边界即收在边缘行列）。
                    if let Some(head) = state.sel_point_at_mouse() {
                        let mut moved = false;
                        if let Some(sel) = state.focused_pane_mut().selection.as_mut() {
                            if sel.head != head {
                                sel.head = head;
                                moved = true;
                            }
                        }
                        if moved {
                            state.window.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::MouseInput {
                state: btn_state,
                button,
                ..
            } => match (button, btn_state) {
                (MouseButton::Left, ElementState::Pressed) => {
                    // 点的是窗格关闭按钮：动作由 egui 侧处理（✕ →
                    // pane_close），这里不聚焦不建选区，也不视作
                    // 「点击面板交出焦点」——关完接着打字不该断流。
                    if state.mouse_on_pane_close() {
                        return;
                    }
                    // 按在分隔条上：拖动调比例由 egui 侧处理（F7③，
                    // divider_drag），这里不聚焦/不建选区，也不交出
                    // 终端焦点——调完比例接着打字不该断流。
                    if state.mouse_on_pane_divider() {
                        return;
                    }
                    // 按在侧栏/文件树栏的拖宽手柄上（P10）：拖宽由
                    // egui 面板处理，这里同样不聚焦/不建选区/不交出
                    // 终端焦点——调完宽度接着打字不该断流。
                    if state.mouse_on_panel_resize() {
                        return;
                    }
                    // 焦点仲裁（F5）：点击窗格聚焦该窗格 + 终端拿键盘/
                    // IME 焦点；点击 egui 面板交出焦点（路由随之切换）。
                    let Some(pi) = state.pane_under_mouse() else {
                        state.terminal_focused = false;
                        return;
                    };
                    state.terminal_focused = true;
                    state.focus_pane(pi);

                    // ── footer 区域分流（第十一轮，input-editor feature）─
                    // Compose/可见态下点击 footer 区域时，不建终端选区，
                    // 转入编辑器鼠标处理路径。键盘续走编辑器（terminal_focused=true 保持）。
                    #[cfg(feature = "input-editor")]
                    if state.mouse_on_footer() {
                        if let Some((rel_x, rel_y, cell_w, cell_h, fp, lines)) =
                            state.mouse_footer_relative()
                        {
                            let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
                            // 像素 → 编辑器位置
                            let pos = footer_mouse::pixel_to_position(
                                rel_x, rel_y, cell_w, cell_h, fp, &line_refs,
                            );
                            // 显示列（用于 click-count 位移检测）
                            let display_col = (rel_x / cell_w.max(1.0)).floor() as usize;
                            let row = pos.line;
                            let kind = state.footer_click_state.record_click(
                                row,
                                display_col,
                                std::time::Instant::now(),
                            );

                            let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);

                            let action = match kind {
                                footer_mouse::ClickKind::Single => {
                                    let shift = state.modifiers.shift_key();
                                    let cur_anchor =
                                        state.tabs[ti].panes[pi].editor.view().selection().anchor;
                                    footer_mouse::single_click_action(pos, shift, cur_anchor)
                                }
                                footer_mouse::ClickKind::Double => {
                                    let line_text =
                                        lines.get(pos.line).map(|s| s.as_str()).unwrap_or("");
                                    let sel = footer_mouse::word_selection(pos, line_text);
                                    lumen_editor::EditAction::SetSelection(sel)
                                }
                                footer_mouse::ClickKind::Triple => {
                                    let line_text =
                                        lines.get(pos.line).map(|s| s.as_str()).unwrap_or("");
                                    let sel = footer_mouse::line_selection(pos, line_text);
                                    lumen_editor::EditAction::SetSelection(sel)
                                }
                            };

                            // 将 lumen_editor::EditAction 包装为 app 层 Action
                            // 单击时记录锚点（拖选用）
                            let app_action = lumen_editor_action_to_app_action(action);
                            state.dispatch(app_action, ti, pi);

                            // 记录拖选锚点（单击/双击/三击都可能继续拖）
                            let new_anchor =
                                state.tabs[ti].panes[pi].editor.view().selection().anchor;
                            state.footer_drag_anchor = new_anchor;
                            state.footer_dragging = true;
                        }
                        return;
                    }

                    // 选区在点中的窗格（即新焦点窗格）建立。
                    let Some(p) = state.sel_point_at_mouse() else {
                        return;
                    };
                    let s = state.focused_pane_mut();
                    s.selecting = true;
                    // 单击先建立空选区（不高亮），拖动后才有内容。
                    s.selection = Some(Selection { anchor: p, head: p });
                    state.window.request_redraw();
                }
                (MouseButton::Left, ElementState::Released) => {
                    // footer 拖选结束（input-editor feature）。
                    #[cfg(feature = "input-editor")]
                    if state.footer_dragging {
                        state.footer_dragging = false;
                        return;
                    }

                    // 本次按下不在窗格上（点的是 egui 面板）则与终端无关。
                    if !state.focused_pane().selecting {
                        return;
                    }
                    state.focused_pane_mut().selecting = false;
                    if state.focused_pane().selection.is_some_and(|s| s.is_empty()) {
                        // 单击（未拖动）：选中/清除所在命令块。
                        // 备用屏幕下块行号坐标系不可用，不做块选中。
                        let p = state.sel_point_at_mouse();
                        let s = state.focused_pane_mut();
                        s.selection = None;
                        if let Some(p) = p {
                            if !s.term.is_alt_screen() {
                                let hit = s.term.block_at_line(p.line).map(|b| b.id);
                                s.selected_block = if hit == s.selected_block { None } else { hit };
                            }
                        }
                        state.window.request_redraw();
                    }
                }
                (MouseButton::Right, ElementState::Pressed) => {
                    // 右键也按「点击窗格聚焦」仲裁（F5）：复制/粘贴作用
                    // 于点中的窗格。
                    let Some(pidx) = state.pane_under_mouse() else {
                        return;
                    };
                    state.focus_pane(pidx);
                    state.terminal_focused = true;

                    // ── footer 区域右键：弹出编辑器上下文菜单（第十一轮）─
                    #[cfg(feature = "input-editor")]
                    if state.mouse_on_footer() {
                        // 记录弹出位置，egui 帧内渲染菜单（见 RedrawRequested 处理）
                        state.footer_context_menu_at = Some(state.mouse_pos);
                        state.window.request_redraw();
                        return;
                    }

                    // 右键（终端区）：有选区则复制，否则粘贴（Windows Terminal 惯例）。
                    // 字段级下标：clipboard 需要同时可变借用。
                    let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);
                    if state.tabs[ti].panes[pi].copy_selection(&mut state.clipboard) {
                        state.tabs[ti].panes[pi].selection = None;
                        state.window.request_redraw();
                    } else {
                        state.tabs[ti].panes[pi].paste_clipboard(&mut state.clipboard);
                    }
                }
                _ => {}
            },
            // IME 预编辑（M4.1 批D2，设计稿 §7.3）：
            // Compose 态：更新 session.preedit（不进编辑器文档，不参与 undo）。
            // text 为空或 cursor_range 为 None + 空串 → 清空预编辑（预编辑取消）。
            // 其余态：事件本身已由 egui-winit 处理（路由已交 egui），此处忽略。
            WindowEvent::Ime(Ime::Preedit(text, cursor)) => {
                if !state.terminal_focused {
                    return;
                }
                let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);
                #[cfg(feature = "input-editor")]
                {
                    let mode =
                        mode::effective_mode(&state.tabs[ti].panes[pi].term, state.force_fallback);
                    if mode == mode::InputMode::Compose {
                        if text.is_empty() {
                            // 空串 = 预编辑结束/取消
                            state.tabs[ti].panes[pi].preedit = None;
                        } else {
                            state.tabs[ti].panes[pi].preedit =
                                Some(lumen_renderer::composer_view::PreeditState {
                                    text,
                                    cursor_range: cursor,
                                });
                        }
                        state.window.request_redraw();
                        return;
                    }
                }
                // 非 Compose 态或 feature 未开启：丢弃（PTY 终端自行处理 IME）。
                let _ = (text, cursor);
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                // 仅终端聚焦时把 IME 提交文本写入 shell（焦点窗格）；
                // egui 输入框聚焦时事件已喂给 egui 消化，再写 PTY 就是
                // 双投。
                if !state.terminal_focused {
                    return;
                }
                // M4.1 批D1：IME 分流——设计稿 §7.3
                // Compose 态：提交文本进编辑器（InsertText），不写 PTY。
                // 其余态：与按键路径一致，回滚到底部后写 PTY。
                let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);
                #[cfg(feature = "input-editor")]
                {
                    let mode =
                        mode::effective_mode(&state.tabs[ti].panes[pi].term, state.force_fallback);
                    if mode == mode::InputMode::Compose {
                        // 提交时清空 preedit（M4.1 批D2）
                        state.tabs[ti].panes[pi].preedit = None;
                        // IME 提交进编辑器（走 dispatch 确保门控逻辑一致）
                        state.dispatch(
                            action::Action::Edit(action::EditAction::InsertText(text)),
                            ti,
                            pi,
                        );
                        return;
                    }
                }
                // 非 Compose 态（含 feature 未开启）：直通 PTY
                // 与按键路径一致：输入即回滚到底部——翻看历史时提交
                // 中文，视图不跳回底部会看不到自己的回显。
                let s = state.tabs[ti].panes[pi].term.grid_mut();
                s.scroll_to_bottom();
                let s = state.focused_pane_mut();
                if let Err(e) = s.write_user_input(text.as_bytes()) {
                    error!("写入 PTY 失败: {e:#}");
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // 终端窗格区内滚轮归终端，区外（侧栏等）归 egui；滚动
                // 作用于**焦点窗格**（F5 拍板：键盘/IME/滚轮/选区全部
                // 跟焦点，悬停别的窗格不抢路由——要滚哪个先点哪个）。
                if state.pane_under_mouse().is_none() {
                    return;
                }
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y * 3.0) as isize,
                    MouseScrollDelta::PixelDelta(p) => {
                        (p.y / state.renderer.cell_size().1 as f64) as isize
                    }
                };
                if lines != 0 {
                    state
                        .focused_pane_mut()
                        .term
                        .grid_mut()
                        .scroll_display(lines);
                    state.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                // 问题4（B4 修复）：无边框窗口最小化时 winit inner_size
                // 缩为约 160×28 小条（非 0×0，绕过原有 0 尺寸守卫），
                // egui 布局与 PTY resize 会以此极小尺寸执行，导致
                // layout.rs 的 clamp 产生 max < min panic，进而在
                // wgpu swapchain 释放时触发次生 panic。
                // 守卫：最小化态 (is_minimized == true) 或宽/高 < 120
                // 物理像素（160×28 小条实测值）时跳过整帧渲染与布局。
                {
                    let sz = state.window.inner_size();
                    const MIN_RENDERABLE: u32 = 120;
                    let too_small = sz.width < MIN_RENDERABLE || sz.height < MIN_RENDERABLE;
                    let minimized = state.window.is_minimized().unwrap_or(false);
                    if minimized || too_small {
                        return;
                    }
                }
                // surface 帧先行取得：失败（Lost/Outdated 已就地重配）则
                // 本帧整体跳过——egui 输入与 textures_delta 都未消费，
                // 状态不丢，等下一次重绘。
                let Some(frame) = state.renderer.acquire_frame() else {
                    return;
                };
                let render_t0 = Instant::now();

                // —— DEC 2026 同步区间门控（事件驱动重绘的保护层，F5
                // 起**逐窗格**判定）——
                // M3 起鼠标划过窗口等任意 egui repaint 都会触发本处理器，
                // 而 BSU..ESU 之间 grid 是边收边改的半成品（光标游走、
                // 未画完的行）——静默合帧/小步顺延只管**定时调度**路径，
                // 管不住事件驱动的 request_redraw。此处兜底：同步区间内
                // 且渲染计划在途（abs 兜底未到点）的窗格，跳过其终端离
                // 屏渲染——egui 照常布局合成（悬停高亮不受影响），该
                // 窗格纹理保留上一完整帧，ESU 到达后由快路/计划补画；
                // 其余窗格照常渲染（逐窗格门控互不连坐）。跳过时也不动
                // take_dirty 与光标冻结状态（属于「真渲染」的配套动作，
                // 提前执行会吃掉 damage、错推冻结时间轴）。
                // 欠帧（term_frame_due_since）不再无条件放行：ESU 快路
                // 的 request_redraw 与 WM_PAINT 之间若有新 BSU 批被
                // drain（流式输出下的常见竞态），旧逻辑会把半成品 grid
                // 画上屏——蓝条随未归位的光标行伸缩、内容闪烁（需求池
                // P1 的来源之一）。改为：同步区间内欠帧也暂缓，交给该
                // 批 drain 时重新武装的渲染计划（abs 在途是跳过的前提，
                // 且 abs 必伴随 redraw_at，补画一定会被调度）在 ESU 后
                // 补画完整帧；但暂缓以欠帧起点 + REDRAW_ABS_CAP 为限，
                // 超龄后无论是否同步一律放行——保住「应用不会卡死在
                // BSU 画面冻结」的绝对兜底语义（worst case 与原 abs
                // 兜底同量级，普通流量根本到不了）。
                let mut skip_pane: Vec<bool> = state.tabs[state.active_tab]
                    .panes
                    .iter()
                    .map(|s| {
                        s.term.is_synchronized()
                            && s.redraw_abs_at.is_some_and(|a| render_t0 < a)
                            && s.term_frame_due_since
                                .is_none_or(|d| render_t0.duration_since(d) < REDRAW_ABS_CAP)
                    })
                    .collect();
                // 最大化期间其余窗格无条件跳过渲染（P14：不可见，纹理
                // 不上屏；后台照常消化输出，还原/重启时强制补帧）。
                if let Some(m) = state.tabs[state.active_tab].maximized {
                    for (i, skip) in skip_pane.iter_mut().enumerate() {
                        if i != m {
                            *skip = true;
                        }
                    }
                }

                for (i, s) in state.tabs[state.active_tab].panes.iter_mut().enumerate() {
                    if skip_pane[i] {
                        continue;
                    }
                    s.term.grid_mut().take_dirty();
                    // 光标跟随策略（逐窗格）：正常情况下零延迟跟随终端
                    // 光标；处于「帧尾未归位」窗口（ESU 后还没重新显示
                    // 光标）时冻结旧位置，等归位序列或超时，避免画出
                    // 重绘残留位。
                    let now = Instant::now();
                    let g = s.term.grid();
                    let seen = (g.cursor.row, g.cursor.col, g.cursor.visible);
                    // 同行近距移动是打字/退格的特征，即时跟随不冻结；
                    // 动画残留位的特征是跨行大跳，才需要等归位确认。
                    let typing_move = seen.2
                        && s.cursor_displayed.2
                        && seen.0 == s.cursor_displayed.0
                        && seen.1.abs_diff(s.cursor_displayed.1) <= 4;
                    if !s.term.cursor_unsettled() || typing_move {
                        s.cursor_frozen_at = None;
                        s.cursor_displayed = seen;
                    } else {
                        let frozen = *s.cursor_frozen_at.get_or_insert(now);
                        if now.duration_since(frozen) >= CURSOR_FREEZE_CAP {
                            s.cursor_displayed = seen;
                            s.cursor_frozen_at = None;
                        } else if s.cursor_displayed != seen {
                            // 安排超时时刻补画一帧，防止光标停滞在旧位。
                            let at = frozen + CURSOR_FREEZE_CAP;
                            s.redraw_at = Some(s.redraw_at.map_or(at, |x| x.min(at)));
                        }
                    }
                }

                // —— 窗格离屏纹理懒创建（新窗格/恢复后的首帧）——
                // 先按 1x1 占位注册到 egui 拿稳定 TextureId（run_ui 录
                // 制 Image 需要它）；本帧布局后的矩形对照会立即按实际
                // 尺寸重建并原地换绑——egui 在 pass 录制时才解析纹理，
                // 占位尺寸不会真的被采样上屏。
                for i in 0..state.tabs[state.active_tab].panes.len() {
                    let sid = state.tabs[state.active_tab].panes[i].id;
                    if state.pane_textures.contains_key(&sid) {
                        continue;
                    }
                    state.renderer.ensure_offscreen(sid, 1, 1);
                    let Some(view) = state.renderer.offscreen_view(sid) else {
                        continue; // 刚 ensure 过必在；防御分支
                    };
                    let tex = state.egui_renderer.register_native_texture(
                        state.renderer.device(),
                        view,
                        wgpu::FilterMode::Nearest,
                    );
                    state.pane_textures.insert(sid, tex);
                }

                // —— egui 帧：跑 UI 布局，产出本帧各窗格矩形 ——
                let raw_input = state.egui_state.take_egui_input(&state.window);

                // —— 最大化越界修复（第十轮问题1）——
                // 无边框 + WS_THICKFRAME 最大化时，Windows 将窗口推至约
                // (-8,-8)，尺寸比工作区大 ~16px（隐藏不可见粗边框）。egui
                // 按完整 inner_size 布局，右/下贴边内容画在屏幕外被裁剪。
                //
                // 修复：用 MonitorFromWindow + GetMonitorInfoW 实算四边越界量，
                // shrink raw_input.screen_rect.max（只改 max，保持 min=(0,0)），
                // 使 egui 内容区等于实际可见区域。
                //
                // 坐标链路验证：
                //   鼠标事件坐标 = 客户区坐标（原点 (0,0)），screen_rect.min
                //   仍为 (0,0)，两者坐标系一致，无需平移。
                //   snap_layouts 按钮换算：egui rect × ppp + inner_position；
                //   shrink 后按钮 egui 坐标贴 shrunk max，× ppp + (-8) = 工作区
                //   右边界，正确（不再超出屏幕）。
                // —— 最大化越界修复（第十一轮根因分析：无需 shrink screen_rect）——
                //
                // 第十轮曾尝试：GetWindowRect 检测到 8px overflow → shrink raw_input.screen_rect。
                // 但第十一轮诊断证明该思路错误，原因：
                //   1. winit 的 window.inner_size() 调用 GetClientRect（非 GetWindowRect），
                //      返回的是客户区物理像素（2560px on 2560px monitor），已排除 8px 不可见
                //      阴影边框。
                //   2. GetWindowRect 返回的 8px overflow 是系统管理的不可见 THICKFRAME 阴影，
                //      不在客户区内，不影响内容布局。
                //   3. shrink screen_rect 反而使 egui 布局比可见区域窄 8px，造成右侧 8px 空白。
                //
                // 真正原因（第十一轮定位）：
                //   footer label "[ 编辑模式 ]" 用 `label_char_count * cw` 估算宽度，
                //   但 CJK 汉字在等宽终端字体中渲染为 2×cw（全角），导致文字实际宽度约为
                //   估算值的 1.5×，label_x 偏右，文本溢出纹理右边界被裁剪。
                //   修复已落 lumen-renderer/src/lib.rs（改用 layout_runs().line_w 实测宽度）。
                //   statusbar 按钮同样受 CJK 宽度估算影响，修复已落 shell/statusbar.rs。
                //
                // 此处不再 shrink screen_rect。query_maximized_overflow / maximized_overflow
                // 纯函数已有单测保留（算法正确，只是本场景不需要应用它）。

                let entries: Vec<shell::TabItem> = state
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        // 标题取值（自定义名 > 焦点窗格 cwd > OSC 标题 >
                        // 会话 N）见 Tab::display_title，恒非空；默认名
                        // 为 cwd 时挂全路径悬停提示（截断时可看全，F4）。
                        let title = t.display_title();
                        shell::TabItem {
                            id: t.id,
                            hover_path: t.title_is_cwd().then(|| title.clone()),
                            title,
                            active: i == state.active_tab,
                            unseen: t.has_unseen(),
                            pane_count: t.panes.len(),
                        }
                    })
                    .collect();
                let tab = &state.tabs[state.active_tab];
                // 本帧布局对应的窗格 id 快照：下方动作（关 tab/增删窗
                // 格）可能改变结构，矩形与窗格的对应关系以此校验。
                let layout_pane_ids: Vec<SessionId> = tab.panes.iter().map(|p| p.id).collect();
                let panes_view: Vec<shell::PaneView> = tab
                    .panes
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        // 窗格标题（F7①）：与侧栏 display_title 同源
                        // 取值（cwd > OSC 标题），但标题栏空间窄，cwd
                        // 取尾目录名（盘根等无尾名时回完整路径）；悬停
                        // 提示完整 cwd。两者皆无回退「窗格 N」。
                        let cwd = p.term.cwd();
                        let title = cwd
                            .map(|c| {
                                c.file_name().map_or_else(
                                    || c.display().to_string(),
                                    |t| t.to_string_lossy().into_owned(),
                                )
                            })
                            .or_else(|| {
                                let t = p.term.title();
                                (!t.is_empty()).then(|| t.to_owned())
                            })
                            .unwrap_or_else(|| {
                                i18n::fmt1(i18n::strings().pane_default_name_fmt, i + 1)
                            });
                        shell::PaneView {
                            tex: state.pane_textures.get(&p.id).copied(),
                            focused: i == tab.focused,
                            title,
                            title_hover: cwd.map(|c| c.display().to_string()),
                        }
                    })
                    .collect();
                let was_renaming = state.shell_state.renaming.is_some();
                // 文件树输入：焦点窗格的 cwd（OSC 9;9 上报）与空闲态
                // （cd 注入闸门，见 Terminal::shell_waiting_input）。
                let active_cwd = tab
                    .focused_pane()
                    .term
                    .cwd()
                    .map(std::path::Path::to_path_buf);
                let shell_idle = tab.focused_pane().term.shell_waiting_input();
                // 背景图参数（P13）：仅当纹理已加载且 settings 启用时传入。
                // 同时检查 enabled：用户本帧拨动开关关闭后，bg_texture 清空在
                // apply_background_image（run_ui 之后）才执行；提前在此过滤
                // 可保证关闭语义对 egui 层当帧即时生效，避免一帧闪烁。
                let bg_image = state
                    .bg_texture
                    .as_ref()
                    .filter(|_| state.settings.appearance.background.enabled)
                    .map(|tex| shell::BgImageInput {
                        texture_id: tex.texture_id,
                        width: tex.width,
                        height: tex.height,
                        opacity: state.settings.appearance.background.opacity,
                        dim: state.settings.appearance.background.dim,
                    });
                // 历史搜索面板行数据（M4.3）：仅面板打开时计算（最多取 50 条）。
                // 面板关闭时传空 Vec，不做 fuzzy_search 开销。
                // 历史搜索面板行数据（M4.3）：仅面板打开时计算（取前 20 条，由 fuzzy_search 内部截断）。
                // 面板关闭时传空 Vec，不做 fuzzy_search 开销。
                let history_rows_owned: Vec<shell::history_search_ui::HistoryRow> =
                    if state.shell_state.history_search.open {
                        let query = &state.shell_state.history_search.query;
                        state
                            .history
                            .fuzzy_search(query)
                            .into_iter()
                            .map(|hit| {
                                let entry = &state.history.entries()[hit.entry_idx];
                                shell::history_search_ui::HistoryRow {
                                    text: entry.text.clone(),
                                    exit_code: entry.exit_code,
                                    match_spans: hit.match_spans,
                                }
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                // 补全弹窗候选视图（M4.4 批1）：仅 input-editor feature 下，
                // completion.open 时构造 CompletionView 传入 shell；否则传 None。
                #[cfg(feature = "input-editor")]
                let completion_candidate_rows: Vec<
                    shell::completion_ui::CandidateRow,
                > = if state.shell_state.completion.open {
                    state
                        .completion_candidates
                        .iter()
                        .map(|c| shell::completion_ui::CandidateRow {
                            display: c.display.clone(),
                            is_dir: c.is_dir,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                // 锚点：footer 上方合理位置（底部 = 窗口高 - statusbar - footer 估算高度）。
                // 取 egui 逻辑坐标：statusbar 高度 + 一行 footer 高度约 40px 之上。
                // 首版不要求精确跟随光标列，x 取终端区左缘附近固定值即可。
                #[cfg(feature = "input-editor")]
                let completion_view_owned: Option<
                    shell::completion_ui::CompletionView<'_>,
                > = if state.shell_state.completion.open && !completion_candidate_rows.is_empty() {
                    let scale = state.egui_ctx.pixels_per_point();
                    let win_h = state.window.inner_size().height as f32 / scale;
                    // statusbar 高度 + footer 约 1 行高度（cell_h 估约 20px）
                    let anchor_y = win_h
                            - shell::statusbar::HEIGHT
                            - 20.0  // footer 单行高度估算
                            - 4.0; // 小间距
                    let anchor_x = state.settings.layout.sidebar_width + 12.0;
                    Some(shell::completion_ui::CompletionView {
                        candidates: &completion_candidate_rows,
                        anchor: egui::pos2(anchor_x, anchor_y),
                    })
                } else {
                    None
                };
                let shell_input = shell::ShellInput {
                    panes: &panes_view,
                    layout: tab.layout.clone(),
                    maximized: tab.maximized,
                    tabs: &entries,
                    profile: state.profile.as_ref(),
                    cwd: active_cwd.as_deref(),
                    shell_idle,
                    os_dark: state.os_dark,
                    bg_image,
                    // 底部状态栏所需：当前有效输入模式 + 经典直通开关（M4.1 批E）
                    #[cfg(feature = "input-editor")]
                    input_mode: mode::effective_mode(
                        &tab.focused_pane().term,
                        state.force_fallback,
                    ),
                    #[cfg(feature = "input-editor")]
                    force_fallback: state.force_fallback,
                    history_rows: &history_rows_owned,
                    #[cfg(feature = "input-editor")]
                    completion_view: completion_view_owned,
                    #[cfg(not(feature = "input-editor"))]
                    completion_view: None,
                };
                let shell_state = &mut state.shell_state;
                let app_settings = &mut state.settings;
                // M3.8：传入当前窗口最大化态，顶栏据此切换最大化/还原图标。
                let is_maximized = state.window.is_maximized();
                let mut shell_out = None;
                // footer 右键菜单请求（第十一轮）：egui Area 在帧内弹出。
                #[cfg(feature = "input-editor")]
                let footer_ctx_menu_req = state.footer_context_menu_at.take();
                #[cfg(feature = "input-editor")]
                let mut footer_ctx_action: Option<action::Action> = None;
                let full_output = state.egui_ctx.run_ui(raw_input, |ui| {
                    shell_out = Some(shell::show(
                        ui,
                        &shell_input,
                        shell_state,
                        app_settings,
                        is_maximized,
                    ));

                    // ── footer 右键菜单（第十一轮，input-editor feature）──
                    #[cfg(feature = "input-editor")]
                    if let Some((mx, my)) = footer_ctx_menu_req {
                        let scale = ui.ctx().pixels_per_point();
                        // 物理像素 → egui 逻辑点
                        let lx = mx as f32 / scale;
                        let ly = my as f32 / scale;
                        let s = crate::i18n::strings();
                        // 查询编辑器选区（用于灰显判断）
                        let has_sel = {
                            let ti = state.active_tab;
                            let pi = state.tabs[ti].focused;
                            state.tabs[ti].panes[pi].editor.view().has_selection()
                        };

                        let area_resp = egui::Area::new(egui::Id::new("footer_ctx_menu"))
                            .fixed_pos(egui::pos2(lx, ly))
                            .order(egui::Order::Foreground)
                            .show(ui.ctx(), |ui| {
                                egui::Frame::popup(ui.style()).show(ui, |ui| {
                                    // 复制（有选区时可用）
                                    let copy_btn =
                                        ui.add_enabled(has_sel, egui::Button::new(s.ctx_menu_copy));
                                    if copy_btn.clicked() {
                                        // 复制编辑器选区（dispatch 内处理）
                                        footer_ctx_action = Some(action::Action::Term(
                                            action::TermAction::CopyEditorSelection,
                                        ));
                                    }
                                    // 剪切（有选区时可用）
                                    let cut_btn =
                                        ui.add_enabled(has_sel, egui::Button::new(s.ctx_menu_cut));
                                    if cut_btn.clicked() {
                                        footer_ctx_action = Some(action::Action::Term(
                                            action::TermAction::CutEditorSelection,
                                        ));
                                    }
                                    // 粘贴（始终可用）
                                    if ui.button(s.ctx_menu_paste).clicked() {
                                        footer_ctx_action = Some(action::Action::Term(
                                            action::TermAction::PasteClipboard,
                                        ));
                                    }
                                    // 全选
                                    if ui.button(s.ctx_menu_select_all).clicked() {
                                        footer_ctx_action = Some(action::Action::Edit(
                                            action::EditAction::SelectAll,
                                        ));
                                    }
                                });
                            });
                        // Esc 或点击菜单外：关闭（Area 自然消失，不处理关闭信号）
                        let _ = area_resp;
                    }
                });
                let Some(shell_out) = shell_out else {
                    return; // run_ui 必然执行闭包，防御分支
                };
                if shell_out.term_clicked {
                    state.terminal_focused = true;
                }

                // ── footer 右键菜单动作 dispatch（第十一轮）───────────────
                #[cfg(feature = "input-editor")]
                if let Some(ctx_action) = footer_ctx_action {
                    let ti = state.active_tab;
                    let pi = state.tabs[ti].focused;
                    state.dispatch(ctx_action, ti, pi);
                }

                // 底部状态栏「经典模式」按钮：复用 ToggleFallback 同路径（M4.1 批E）。
                #[cfg(feature = "input-editor")]
                if shell_out.toggle_fallback {
                    let ti = state.active_tab;
                    let pi = state.tabs[ti].focused;
                    state.dispatch(
                        action::Action::Term(action::TermAction::ToggleFallback),
                        ti,
                        pi,
                    );
                }
                // 点击窗格（egui interact 侧的命中，与 window_event 的
                // 原始鼠标路由互为冗余、同语义）：切焦点窗格。
                if let Some(pi) = shell_out.pane_clicked {
                    state.focus_pane(pi);
                }
                // 重命名编辑期间键盘/IME 归 egui 的输入框（右键打开
                // 菜单不经过左键焦点仲裁，必须在此强制让出）；编辑以
                // **键盘**结束（Enter/Esc）才把焦点还给终端——点击别处
                // 取消时那次点击已按鼠标仲裁决定焦点归属（点头像/面板
                // 为 false），无条件翻回 true 会让头像菜单开着时键盘
                // 直通 PTY（Esc 关不掉菜单、打字进 shell）。
                if state.shell_state.renaming.is_some() {
                    state.terminal_focused = false;
                } else if was_renaming && shell_out.rename_ended_by_key {
                    state.terminal_focused = true;
                }
                // —— egui 弹层（右键菜单/头像菜单等 Popup）焦点路由 ——
                // 打开期间键盘恒归 egui：右键打开菜单不经过左键焦点
                // 仲裁，没有这层时 terminal_focused 仍为 true，Esc 想关
                // 菜单却把 \x1b 写进 PTY（PSReadLine 清掉输入中的命令
                // 行），打字也漏进 shell。关闭那帧的焦点归属按关闭方式
                // 仲裁：键盘（Esc）关闭还给终端（关完直接继续敲命令）；
                // 点击关闭尊重该次点击的鼠标仲裁结果（点终端区已置
                // true、点面板保持 false），不强行翻转。
                let popup_open = egui::Popup::is_any_open(&state.egui_ctx);
                if popup_open {
                    state.terminal_focused = false;
                } else if state.was_popup_open
                    && state.shell_state.renaming.is_none()
                    && !state.shell_state.settings.open
                    && !state.shell_state.login.open
                    && !state.shell_state.history_search.open
                    && !state.shell_state.completion.open
                    && !state.egui_ctx.input(|i| i.pointer.any_click())
                {
                    state.terminal_focused = true;
                }
                state.was_popup_open = popup_open;

                // —— 侧栏动作：切换 / 重命名 / 新建 / 关闭（tab 级）——
                if let Some(id) = shell_out.activate {
                    if let Some(idx) = state.tabs.iter().position(|t| t.id == id) {
                        if idx != state.active_tab {
                            state.activate(idx);
                        }
                    }
                }
                if let Some((id, name)) = shell_out.rename {
                    if let Some(t) = state.tabs.iter_mut().find(|t| t.id == id) {
                        // 空名 = 清除自定义名，恢复跟随默认标题（焦点
                        // 窗格 cwd > OSC 标题）。
                        t.custom_title = (!name.is_empty()).then_some(name);
                    }
                    state.update_window_title();
                    // 重命名是结构性变更：落盘（F4）。
                    state.persist_sessions();
                }
                if shell_out.new_session {
                    state.new_tab();
                }
                if let Some(id) = shell_out.close {
                    if let Some(idx) = state.tabs.iter().position(|t| t.id == id) {
                        if state.close_tab(idx) {
                            info!("最后一个会话已关闭，退出应用");
                            event_loop.exit();
                            return; // 不再呈现本帧（应用退出中）
                        }
                    }
                }

                // —— M3.8 自绘标题栏：窗口控制动作处理 ——
                // drag_window / set_minimized / set_maximized 须在 shell::show
                // 同帧（RedrawRequested 内）执行，时序成立（调研 §3 已证）。
                if shell_out.drag_title_bar {
                    // drag_window 内部发 WM_NCLBUTTONDOWN + HTCAPTION 启动系统拖动。
                    // 失败（如最大化态下操作）静默忽略——不影响应用逻辑。
                    if let Err(e) = state.window.drag_window() {
                        log::debug!("drag_window 失败（忽略）：{e}");
                    }
                }
                if shell_out.minimize_window {
                    state.window.set_minimized(true);
                }
                if shell_out.toggle_maximize_window {
                    state.window.set_maximized(!state.window.is_maximized());
                }
                if shell_out.close_window {
                    // 关闭窗口：走与 CloseRequested 同路径——落盘后退出。
                    state.persist_sessions();
                    info!("自绘标题栏关闭按钮：落盘后退出");
                    event_loop.exit();
                    return; // 本帧不再继续呈现
                }
                if let Some((lx, ly)) = shell_out.show_window_menu_at {
                    // 逻辑点换算为物理像素，传给 show_window_menu。
                    let scale = state.window.scale_factor();
                    let px = winit::dpi::PhysicalPosition::new(
                        (lx as f64 * scale).round() as i32,
                        (ly as f64 * scale).round() as i32,
                    );
                    state.window.show_window_menu(px);
                }

                // —— M3.8 批2 Snap Layouts：最大化按钮矩形换算为屏幕物理像素 ——
                // egui 逻辑坐标矩形 × pixels_per_point + 窗口客户区屏幕原点
                // = 屏幕物理像素矩形，写入 snap_layouts 原子供子类过程使用。
                //
                // 坐标系说明：
                //   - egui 坐标原点 = 窗口客户区左上角（逻辑像素）。
                //   - 屏幕坐标原点 = 主显示器左上角（物理像素，可为负值）。
                //   - inner_position() 返回客户区左上角的屏幕物理坐标（PhysicalPosition）。
                //   - egui 坐标原点 = 客户区屏幕左上角 = inner_position()（无边框下
                //     NC offset 为 0；最大化时系统将窗口推至约 (-8,-8) 隐藏粗边框，
                //     inner_position 如实反映该值，换算仍正确）。
                //     选用 inner_position 而非 outer_position，是因为 egui 坐标
                //     原点确实对应客户区——无论有无边框、是否最大化都正确。
                #[cfg(target_os = "windows")]
                if let Some(rect) = shell_out.maximize_btn_rect {
                    // inner_position 可能在 Resumed 前失败，用 ok() 静默跳过。
                    if let Ok(origin) = state.window.inner_position() {
                        let ppp = full_output.pixels_per_point;
                        let l = (rect.min.x * ppp).round() as i32 + origin.x;
                        let t = (rect.min.y * ppp).round() as i32 + origin.y;
                        let r = (rect.max.x * ppp).round() as i32 + origin.x;
                        let b = (rect.max.y * ppp).round() as i32 + origin.y;
                        snap_layouts::update_button_rect(l, t, r, b);
                    }
                }

                // —— 窗格级动作（F5 批2）：顶栏「＋」新增 / 窗格 ✕ 关闭
                // （语义同 Ctrl+Shift+D / Ctrl+Shift+W）。结构变更由下方
                // layout_pane_ids 对照检测，本帧跳过矩形应用与终端渲染。
                if shell_out.new_pane {
                    state.new_pane();
                }
                if let Some(pi) = shell_out.pane_close {
                    let ti = state.active_tab;
                    // ✕ 仅多窗格时出现，越界/单窗格为防御（关最后一格
                    // 即关 tab，与快捷键同语义）。
                    if pi < state.tabs[ti].panes.len() && state.close_pane(ti, pi) {
                        info!("最后一个会话已关闭，退出应用");
                        event_loop.exit();
                        return; // 不再呈现本帧（应用退出中）
                    }
                }
                // —— 窗格最大化/还原（P14）：标题栏按钮（与
                // Ctrl+Shift+Enter 同语义，toggle 内部含下标防御）。
                if let Some(pi) = shell_out.pane_maximize {
                    state.toggle_maximize_pane(pi);
                }
                // —— 一键恢复默认布局（P15）：顶栏「▦」——全部比例
                // 均分 + 最大化态先退出，复位后落盘。
                if shell_out.layout_reset {
                    state.reset_pane_layout();
                }

                // —— 拖动标题栏换位（F7②）：交换两窗格在 panes 中的
                // 下标——交换的是格子里的「内容」（Session），格子的
                // 几何（布局权重）不动；焦点跟随被拖窗格落位，被换走
                // 的格子若持有焦点则跟去对侧（其余窗格焦点不动）。
                // 下标对应 run_ui 时的布局，结构同帧变更（防御）越界
                // 即跳过；交换后 layout_pane_ids 对照不再一致，本帧
                // 跳过矩形应用、下一帧按新顺序重建（与增删窗格同款
                // 瞬态）。
                if let Some((src, dst)) = shell_out.pane_swap {
                    let tab = &mut state.tabs[state.active_tab];
                    // 最大化期间换位禁用（P14；UI 侧已不发拖动，纯防御）。
                    if src != dst
                        && src < tab.panes.len()
                        && dst < tab.panes.len()
                        && tab.maximized.is_none()
                    {
                        tab.panes.swap(src, dst);
                        if tab.focused == src {
                            tab.focused = dst;
                        } else if tab.focused == dst {
                            tab.focused = src;
                        }
                        // 窗格顺序即持久化顺序：换位是结构性变更，立即
                        // 落盘（沿用既有时机；快照一致时内部自动跳过）。
                        state.update_window_title();
                        state.window.request_redraw();
                        state.persist_sessions();
                    }
                }

                // —— 分隔条调比例（F7③）：拖动把边界拖到指针处（实时
                // 生效——比例变化下一帧产出新矩形，沿用「矩形变化 →
                // 离屏重建 + term/pty resize」既有链路；拖动重绘已被
                // 事件驱动的 8ms 合帧下限节流）；双击恢复该方向均分。
                // 下标对应 run_ui 时的布局，结构同帧变更时由 layout
                // 侧的越界检查兜底（不施加、不 panic）。
                if let Some(kind) = shell_out.divider_reset {
                    let tab = &mut state.tabs[state.active_tab];
                    let changed = match kind {
                        DividerKind::Row(_) => tab.layout.reset_rows(),
                        DividerKind::Col { row, .. } => tab.layout.reset_cols(row),
                    };
                    if changed {
                        state.window.request_redraw();
                        // 双击复位与拖动结束同义：比例落盘（F7 持久化）。
                        state.persist_sessions();
                    }
                } else if let Some((kind, pos)) = shell_out.divider_drag {
                    let area = shell_out.term_rect;
                    let tab = &mut state.tabs[state.active_tab];
                    let changed = match kind {
                        DividerKind::Row(idx) => tab.layout.drag_row_to(idx, pos.y, area),
                        DividerKind::Col { row, idx } => {
                            tab.layout.drag_col_to(row, idx, pos.x, area)
                        }
                    };
                    log::debug!("分隔条拖动: {kind:?} pos={pos:?} changed={changed}");
                    if changed {
                        state.layout_dirty = true;
                        state.window.request_redraw();
                    }
                }
                if shell_out.divider_drag_ended {
                    // 拖动结束才落盘（拖动中不写）；快照一致时内部
                    // 自动跳过。
                    log::debug!("分隔条拖动结束：落盘比例");
                    state.layout_dirty = false;
                    state.persist_sessions();
                }

                // —— 覆盖层（设置页/登录页）焦点路由：先处理关闭再处理
                // 打开——登录页关闭时设置页可能仍开着（Account 入口的
                // 叠层场景），后判打开保证焦点不被错误交还终端 ——
                if shell_out.settings_closed || shell_out.login_closed {
                    // 关闭后焦点交还终端（IME 强制复位链路每帧照旧执行）。
                    state.terminal_focused = true;
                }

                // —— 历史搜索面板（M4.3）输出处理 ——
                // history_accept：按当前输入模式分流。
                // - Compose 态：填入编辑器（SetText + 光标移末）。
                // - 非 Compose 态（Running / Fallback / AltScreen）：直接写入 PTY，
                //   不带回车，让用户确认后自己回车（验收①）。
                if let Some(text) = shell_out.history_accept {
                    let ti = state.active_tab;
                    let pi = state.tabs[ti].focused;
                    let cur_mode =
                        mode::effective_mode(&state.tabs[ti].panes[pi].term, state.force_fallback);
                    #[cfg(feature = "input-editor")]
                    if cur_mode == mode::InputMode::Compose {
                        state.tabs[ti].panes[pi]
                            .editor
                            .apply(&lumen_editor::EditAction::SetText(text));
                        // 光标移到行末（视觉跟手，与历史导航同款）。
                        state.tabs[ti].panes[pi]
                            .editor
                            .apply(&lumen_editor::EditAction::Move {
                                motion: lumen_editor::Motion::DocEnd,
                                extend: false,
                            });
                    } else {
                        // 非 Compose 态：把命令文本写入 PTY（不含 \r，让用户自己确认）。
                        if let Err(e) = state.tabs[ti].panes[pi].write_user_input(text.as_bytes()) {
                            log::error!("历史搜索填入 PTY 失败: {e:#}");
                        }
                    }
                    #[cfg(not(feature = "input-editor"))]
                    {
                        // 无 input-editor feature 时（理论上不会到此分支，防御性兜底）
                        let _ = cur_mode;
                        if let Err(e) = state.tabs[ti].panes[pi].write_user_input(text.as_bytes()) {
                            log::error!("历史搜索填入 PTY 失败: {e:#}");
                        }
                    }
                    state.shell_state.history_search.open = false;
                    state.terminal_focused = true;
                    state.window.request_redraw();
                }
                // history_closed：关闭面板，焦点还给终端。
                if shell_out.history_closed {
                    state.shell_state.history_search.open = false;
                    state.terminal_focused = true;
                    state.window.request_redraw();
                }
                // history_query_changed：query 变化，下帧重算结果（fuzzy_search 在 run_ui 前算）。
                if shell_out.history_query_changed {
                    state.window.request_redraw();
                }
                // 面板打开期间键盘恒归 egui（终端不收键盘）。
                if state.shell_state.history_search.open {
                    state.terminal_focused = false;
                }

                // —— 补全弹窗（M4.4 批1）输出处理 ——
                // completion_accept：用选定候选的 replacement 替换当前 token。
                // 批2：若候选含 replace_range（命令补全），按其字节区间替换；
                //       否则（文件路径补全）沿用批1 的 current_token 区间逻辑。
                #[cfg(feature = "input-editor")]
                if let Some(idx) = shell_out.completion_accept {
                    if let Some(cand) = state.completion_candidates.get(idx) {
                        let replacement = cand.replacement.clone();
                        let replace_range = cand.replace_range;
                        let ti = state.active_tab;
                        let pi = state.tabs[ti].focused;
                        let cur_line_idx = state.tabs[ti].panes[pi].editor.view().cursor().line;

                        let (sel_start_byte, sel_end_byte) = if let Some((rs, re)) = replace_range {
                            // 命令补全：使用 sidecar 给出的字节区间（已在合并时换算好）。
                            (rs, re)
                        } else {
                            // 文件路径补全：重新计算 current_token 区间（与编辑器当前状态一致）。
                            let view = state.tabs[ti].panes[pi].editor.view();
                            let cur = view.cursor();
                            let line = view.line(cur.line).to_owned();
                            let (start, _) = completion::current_token(&line, cur.byte);
                            (start, cur.byte)
                        };

                        let sel_start_pos = lumen_editor::Position {
                            line: cur_line_idx,
                            byte: sel_start_byte,
                        };
                        let sel_end_pos = lumen_editor::Position {
                            line: cur_line_idx,
                            byte: sel_end_byte,
                        };
                        // 选中替换区间，然后 InsertText 覆盖写入候选文本。
                        state.tabs[ti].panes[pi].editor.apply(
                            &lumen_editor::EditAction::SetSelection(lumen_editor::Selection {
                                anchor: sel_start_pos,
                                cursor: sel_end_pos,
                            }),
                        );
                        state.tabs[ti].panes[pi]
                            .editor
                            .apply(&lumen_editor::EditAction::InsertText(replacement));
                        state.shell_state.completion.open = false;
                        state.completion_candidates.clear();
                        state.completion_req_id = 0; // 取消 sidecar 在途请求（若还有）。
                        state.terminal_focused = true;
                        state.window.request_redraw();
                    }
                }
                // completion_closed：关闭弹窗，焦点还给终端。
                if shell_out.completion_closed {
                    state.shell_state.completion.open = false;
                    state.completion_candidates.clear();
                    #[cfg(feature = "input-editor")]
                    {
                        state.completion_req_id = 0; // 丢弃后续 sidecar 响应。
                    }
                    state.terminal_focused = true;
                    state.window.request_redraw();
                }
                // 弹窗打开期间键盘归 egui（终端不收键盘）。
                #[cfg(feature = "input-editor")]
                if state.shell_state.completion.open {
                    state.terminal_focused = false;
                }

                // 文件树对话框（新建输入/删除确认）：打开期间键盘/IME
                // 归 egui 的输入框（与重命名编辑同款仲裁）；关闭后交还
                // 终端（与设置页关闭同款，点「取消」也交还）。
                if state.shell_state.filetree.dialog_open() {
                    state.terminal_focused = false;
                } else if shell_out.filetree_dialog_closed {
                    state.terminal_focused = true;
                }
                if shell_out.settings_opened
                    || state.shell_state.settings.open
                    || shell_out.login_opened
                    || state.shell_state.login.open
                {
                    // 打开期间键盘/IME 恒归 egui（覆盖层之下终端的 PTY
                    // 消化与渲染照常进行，只是不收键盘）。
                    state.terminal_focused = false;
                }
                if shell_out.settings_font_changed {
                    // 字体/字号即时生效：重建字体度量（行排版缓存随之
                    // 失效）；cell 尺寸变化引发的行列数重算与全会话
                    // resize 在下方矩形检查统一处理（同一帧内完成）。
                    let ap = &state.settings.appearance;
                    let actual = state
                        .renderer
                        .reconfigure_font(&ap.font_family, ap.font_size);
                    state.shell_state.settings.font_hint = (!ap.font_family.is_empty()
                        && !actual.eq_ignore_ascii_case(&ap.font_family))
                    .then(|| {
                        i18n::fmt2(
                            i18n::strings().toast_font_fallback_fmt,
                            &ap.font_family,
                            &actual,
                        )
                    });
                }
                if shell_out.settings_theme_changed {
                    // 主题即时生效（P12 画廊点选/槽位变更/Sync 开关
                    // 共用）：按生效主题 id 切终端配色 + 外壳样式。
                    state.apply_theme();
                }
                if shell_out.settings_background_image_changed {
                    // 路径变更/清除/开关：重载纹理，renderer 透明状态同步。
                    state.apply_background_image();
                }
                if shell_out.settings_background_params_changed {
                    // 仅 opacity/dim 变更：不需重载纹理，直接更新透明状态。
                    let enabled =
                        state.settings.appearance.background.enabled && state.bg_texture.is_some();
                    state.renderer.set_transparent_background(enabled);
                }
                // 问题7：顶栏① 会话栏显隐——写入 settings 并触发存盘。
                let sidebar_changed = if let Some(v) = shell_out.toggle_sidebar {
                    state.settings.layout.sidebar_visible = v;
                    true
                } else {
                    false
                };
                // 第十九轮：顶栏② 文件树显隐——写入 settings 并触发存盘。
                // shell/mod.rs 已在 toggle_filetree 信号路径同步更新
                // ShellState::filetree.visible（两入口共享同一状态源）；
                // 此处只需同步 settings 字段并将 filetree_changed 并入
                // need_save，Ctrl+B 路径自行落盘不走此分支。
                let filetree_changed = if let Some(v) = shell_out.toggle_filetree {
                    state.settings.layout.filetree_visible = v;
                    true
                } else {
                    false
                };
                let need_save = shell_out.settings_font_changed
                    || shell_out.settings_theme_changed
                    || shell_out.settings_background_image_changed
                    || shell_out.settings_background_params_changed
                    || shell_out.settings_language_changed
                    || sidebar_changed
                    || filetree_changed;
                if need_save {
                    // 变更即写盘（写临时文件后改名，防半写损坏）。失败
                    // 弹 toast：用户以为改完即存，静默丢失重启才发现。
                    if let Some(err) = state.settings.save() {
                        state.shell_state.toast.push(
                            shell::toast::ToastKind::Error,
                            i18n::fmt1(i18n::strings().toast_settings_save_failed_fmt, &err),
                        );
                        // push 发生在本帧 egui 布局之后：请求下一帧立即显示。
                        state.window.request_redraw();
                    }
                }

                // —— 登录/登出动作：state.profile 是唯一数据源，更新后
                // 顶栏头像、头像菜单、设置页 Account 三处下一帧即联动 ——
                if let Some(p) = shell_out.logged_in {
                    // mock 登录成功：原子写盘（重启保持登录态）+ 更新内存态。
                    p.save();
                    info!("登录成功（mock）：{} <{}>", p.display_name, p.email);
                    state.shell_state.toast.push(
                        shell::toast::ToastKind::Info,
                        i18n::fmt1(i18n::strings().toast_logged_in_fmt, &p.display_name),
                    );
                    // push 发生在本帧 egui 布局之后：请求下一帧立即显示。
                    state.window.request_redraw();
                    state.profile = Some(p);
                }
                if shell_out.logged_out {
                    // 登出：删 profile.json，三处 UI 即时回未登录态。
                    profile::Profile::delete();
                    info!("已登出（profile.json 已删除）");
                    state.profile = None;
                }

                // —— 文件树动作：双击目录 cd / 双击文件系统默认程序
                // 打开（注入目标 = 焦点窗格）——
                if let Some(dir) = shell_out.cd_dir {
                    // UI 已按 shell 空闲闸门过滤，这里直接注入。
                    let cmd = shell::filetree::cd_command(&dir);
                    let s = state.focused_pane_mut();
                    s.term.grid_mut().scroll_to_bottom();
                    if let Err(e) = s.write_user_input(&cmd) {
                        error!("写入 PTY 失败: {e:#}");
                    }
                    // cd 后把键盘/IME 焦点交还终端，用户可直接继续敲命令。
                    state.terminal_focused = true;
                }
                if let Some(file) = shell_out.open_file {
                    shell::filetree::open_with_default(&file);
                }
                // —— 文件树拖放：把路径文本插入**落点所在窗格**的命令
                // 行（不带回车；F5 批2 拍板：拖放目标 = 鼠标落点窗格，
                // 落点不在任何窗格时 shell 侧已过滤为 None）。先聚焦落
                // 点窗格——插入后接着编辑命令行的就是它。转义与 cd 注
                // 入同一套设施（弯引号同形字/控制字符防御见
                // filetree::path_insert_text；空字节串 = 路径被拒绝）。
                //
                // 第二十一轮分流（与 d9444c6 Ctrl+V 分流同构）：
                // Compose 态 → dispatch Edit(InsertText) 进 footer 编辑器；
                // Running / AltScreen / Fallback → 原写 PTY 路径不变。
                // dispatch 内实时查 effective_mode，防落点窗格聚焦瞬间
                // 与执行时刻的模式漂移。
                if let Some((pi, path)) = shell_out.insert_path {
                    // 下标对应 run_ui 时的布局；本帧结构若已被上方动作
                    // 改变（增删窗格）则跳过本次插入（防御，拖放与增删
                    // 同帧发生的概率可忽略）。
                    if pi < state.tabs[state.active_tab].panes.len() {
                        // 先聚焦落点窗格（落点若非焦点窗格，先切焦点再插入，
                        // 与原行为一致）。
                        state.focus_pane(pi);
                        let (ti, pi_focused) =
                            (state.active_tab, state.tabs[state.active_tab].focused);

                        // Compose 态分流：进编辑器；其余态直写 PTY。
                        #[cfg(feature = "input-editor")]
                        {
                            let mode = mode::effective_mode(
                                &state.tabs[ti].panes[pi_focused].term,
                                state.force_fallback,
                            );
                            if mode == mode::InputMode::Compose {
                                // path_insert_text_str 与 path_insert_text 同一引号规则；
                                // 控制字符路径返回 None，静默跳过（纵深防御）。
                                if let Some(text) = shell::filetree::path_insert_text_str(&path) {
                                    // 尾随空格：路径后方便光标继续编辑（与 PTY 路径行为对称）。
                                    let text_with_space = format!("{text} ");
                                    state.dispatch(
                                        action::Action::Edit(action::EditAction::InsertText(
                                            text_with_space,
                                        )),
                                        ti,
                                        pi_focused,
                                    );
                                    state.terminal_focused = true;
                                }
                                // Compose 路径处理完毕，不走下方 PTY 路径。
                                // （continue 不可用——在 if-let 块而非循环内；
                                //  显式跳过：下方 PTY 块受 feature-gate else 保护。）
                            } else {
                                // 非 Compose 态：原写 PTY 路径。
                                let bytes = shell::filetree::path_insert_text(&path);
                                if !bytes.is_empty() {
                                    let s = state.focused_pane_mut();
                                    s.term.grid_mut().scroll_to_bottom();
                                    if let Err(e) = s.write_user_input(&bytes) {
                                        error!("写入 PTY 失败: {e:#}");
                                    }
                                    state.terminal_focused = true;
                                }
                            }
                        }
                        // feature = "input-editor" 未开启时：全量走原 PTY 路径。
                        #[cfg(not(feature = "input-editor"))]
                        {
                            let bytes = shell::filetree::path_insert_text(&path);
                            if !bytes.is_empty() {
                                let s = state.focused_pane_mut();
                                s.term.grid_mut().scroll_to_bottom();
                                if let Err(e) = s.write_user_input(&bytes) {
                                    error!("写入 PTY 失败: {e:#}");
                                }
                                state.terminal_focused = true;
                            }
                        }
                    }
                }
                // —— 文件树右键菜单：复制绝对/相对路径到剪贴板 ——
                if let Some(text) = shell_out.copy_text {
                    let ok = matches!(
                        state.clipboard.as_mut().map(|c| c.set_text(text.clone())),
                        Some(Ok(()))
                    );
                    if ok {
                        state.shell_state.toast.push(
                            shell::toast::ToastKind::Info,
                            i18n::fmt1(i18n::strings().toast_copied_fmt, &text),
                        );
                    } else {
                        error!("写剪贴板失败（复制路径）");
                        state.shell_state.toast.push(
                            shell::toast::ToastKind::Error,
                            i18n::strings().toast_copy_failed,
                        );
                    }
                    // push 发生在本帧 egui 布局之后：请求下一帧立即显示。
                    state.window.request_redraw();
                }

                // —— 窗格矩形（物理像素）变化 → 逐窗格重建离屏 + resize ——
                // 对各边按 epaint 同款语义取整后求宽高：分数 DPI（如
                // 125%）下布局矩形的物理尺寸可为分数，纹理尺寸若单独
                // round 会与呈现 quad 差出 0.5px——Nearest 采样在区中部
                // 复制/丢一行 texel（1px 接缝游走）。shell 侧已把矩形
                // round_to_pixels（见 shell/mod.rs），三者同源后纹理与
                // 屏上 quad 像素数严格相等（1:1 映射），pane_rects_px
                // （鼠标/IME 映射）的 ±0.5px 系统偏差也一并消除。
                //
                // 本帧布局的矩形对应 run_ui 时的窗格列表；上方动作（关
                // tab/增删窗格/切 tab）可能已改变结构——结构变了就跳过
                // 矩形应用与终端渲染（egui 呈现旧画面一帧，与 activate
                // 的「先切再补帧」同款瞬态），请求下一帧按新结构重来。
                let ppp = full_output.pixels_per_point;
                // 面板拖宽手柄命中区（P10）：raw 鼠标让位判定用
                // （mouse_on_panel_resize）。与窗格结构无关，无条件
                // 按本帧布局更新（文件树收起时本帧为空 = 不让位）。
                state.panel_resize_rects_px.clear();
                for r in &shell_out.panel_resize_rects {
                    state.panel_resize_rects_px.push((
                        r.min.x * ppp,
                        r.min.y * ppp,
                        r.width() * ppp,
                        r.height() * ppp,
                    ));
                }
                // —— 侧栏宽度持久化（P10）：egui 面板自管宽度（本帧
                // 实际值经 shell_out 报回），这里只负责落盘——指针
                // 松开（拖动结束）且与已存值差 ≥1px 才写（判定抽
                // width_worth_persisting，B1 单测覆盖）；窗口过窄被
                // 临时压缩到范围之外的瞬态宽度不写（重启还原用户最后
                // 一次主动调整的值）。
                if !state.egui_ctx.input(|i| i.pointer.any_down()) {
                    let lay = &mut state.settings.layout;
                    let mut width_changed = false;
                    let sw = shell_out.sidebar_width;
                    if width_worth_persisting(
                        sw,
                        lay.sidebar_width,
                        settings::SIDEBAR_WIDTH_MIN,
                        settings::SIDEBAR_WIDTH_MAX,
                    ) {
                        log::debug!("侧栏宽度落盘：{} → {sw}", lay.sidebar_width);
                        lay.sidebar_width = sw;
                        width_changed = true;
                    }
                    if let Some(fw) = shell_out.filetree_width {
                        if width_worth_persisting(
                            fw,
                            lay.filetree_width,
                            settings::FILETREE_WIDTH_MIN,
                            settings::FILETREE_WIDTH_MAX,
                        ) {
                            log::debug!("文件树宽度落盘：{} → {fw}", lay.filetree_width);
                            lay.filetree_width = fw;
                            width_changed = true;
                        }
                    }
                    if width_changed {
                        // 失败弹 toast（与字体/主题写盘同款）：用户以为
                        // 拖完即存，静默丢失重启才发现。
                        if let Some(err) = state.settings.save() {
                            state.shell_state.toast.push(
                                shell::toast::ToastKind::Error,
                                // F6：与 L2636 字体/主题/语言写盘失败路径保持一致。
                                i18n::fmt1(i18n::strings().toast_settings_save_failed_fmt, &err),
                            );
                            state.window.request_redraw();
                        }
                    }
                    // 比例写盘兜底（B1 加固）：drag_stopped 在边角场景
                    // 可能错失（拖动中窗口失焦等），拖动改过比例且指针
                    // 已松开就补一次落盘（快照一致时内部自动跳过，
                    // 正常路径无重复写）。
                    if state.layout_dirty {
                        state.layout_dirty = false;
                        log::debug!("比例写盘兜底：指针已松开且布局有未落盘变更");
                        state.persist_sessions();
                    }
                }
                // —— 启动首帧的布局应用值日志（B1 恢复面验收）：加载
                // 日志只证明文件读到了值，这里输出 UI 实际用上的值
                // （egui 面板实际宽度 + 激活 tab 实际权重），一次性。
                if !state.layout_apply_logged {
                    state.layout_apply_logged = true;
                    let t = &state.tabs[state.active_tab];
                    info!(
                        "外壳布局应用：侧栏宽 {:.1}（设置 {:.1}）文件树宽 {:?}（设置 {:.1}）窗格权重 rows={:?} cols={:?} 最大化={:?}",
                        shell_out.sidebar_width,
                        state.settings.layout.sidebar_width,
                        shell_out.filetree_width,
                        state.settings.layout.filetree_width,
                        t.layout.row_weights(),
                        t.layout.col_weights(),
                        t.maximized,
                    );
                }
                let structure_unchanged = state.tabs.get(state.active_tab).is_some_and(|t| {
                    t.panes.len() == layout_pane_ids.len()
                        && t.panes
                            .iter()
                            .zip(&layout_pane_ids)
                            .all(|(p, id)| p.id == *id)
                });
                // 分隔条拖动期间暂缓 term/PTY resize（B2 修复）：旧行为
                // 逐帧 resize 是对 ConPTY 的整批重绘风暴，PSReadLine 的
                // 差量渲染跨 resize 即坐标失步——提示符丢字、回显错位
                // 混叠（症状②）的直接温床，且逐帧触发缩行。拖动中纹理
                // 照常随矩形重建（边界视觉跟手，内容暂按旧行列呈现），
                // 松手（drag_stopped）那一帧本判定即为 false，下方矩形
                // 对照一次性提交 resize。
                //
                // B3-8 修正：整窗 resize（WindowEvent::Resized）必须穿透
                // 此门控——整窗 resize 是 OS 级行为，与分隔条拖动完全
                // 独立；若 egui 指针/拖动状态因窗口失焦或系统接管未被
                // 正常清除，divider_drag 可能持续为 Some 但无法靠
                // drag_stopped 清零，导致整窗 resize 的 term/PTY resize
                // 被永久阻断（海风哥 B3-8 现象：拖过分隔条后放大整窗，
                // 文字仍按旧窄宽折行）。window_just_resized 标志在
                // WindowEvent::Resized 置位，本帧用后清零（单次消耗）。
                let window_resized_this_frame = state.window_just_resized;
                state.window_just_resized = false;
                let divider_resize_held = !window_resized_this_frame
                    && shell_out.divider_drag.is_some()
                    && !shell_out.divider_drag_ended;
                if window_resized_this_frame && shell_out.divider_drag.is_some() {
                    log::debug!(
                        "B3-8：整窗 resize 帧检测到 divider_drag.is_some()（拖动状态滞留），\
                         强制穿透 held 门控，确保 term/PTY resize 提交"
                    );
                }
                if structure_unchanged {
                    // 窗格关闭按钮命中区（F5 批2）：raw 鼠标路由的让位
                    // 判定用（mouse_on_pane_close）。
                    state.pane_close_rects_px.clear();
                    for r in &shell_out.pane_close_rects {
                        state.pane_close_rects_px.push((
                            r.min.x * ppp,
                            r.min.y * ppp,
                            r.width() * ppp,
                            r.height() * ppp,
                        ));
                    }
                    // 分隔条命中区（F7③）：raw 鼠标路由的让位判定用
                    // （mouse_on_pane_divider）。
                    state.divider_rects_px.clear();
                    for r in &shell_out.divider_rects {
                        state.divider_rects_px.push((
                            r.min.x * ppp,
                            r.min.y * ppp,
                            r.width() * ppp,
                            r.height() * ppp,
                        ));
                    }
                    state.pane_rects_px.clear();
                    for (i, r) in shell_out.pane_rects.iter().enumerate() {
                        // 最大化期间的隐藏窗格（P14）：shell 侧矩形为
                        // NOTHING 占位——不重建离屏/不 resize（保持隐藏
                        // 前的网格，后台输出按原尺寸消化）、不进鼠标/IME
                        // 路由表。
                        if state.tabs[state.active_tab]
                            .maximized
                            .is_some_and(|m| m != i)
                        {
                            continue;
                        }
                        let x0 = (r.min.x * ppp).round();
                        let y0 = (r.min.y * ppp).round();
                        let x1 = (r.max.x * ppp).round();
                        let y1 = (r.max.y * ppp).round();
                        let sid = state.tabs[state.active_tab].panes[i].id;
                        state.pane_rects_px.push((sid, (x0, y0, x1 - x0, y1 - y0)));
                        let tw = (x1 - x0).max(1.0) as u32;
                        let th = (y1 - y0).max(1.0) as u32;
                        if state.renderer.ensure_offscreen(sid, tw, th) {
                            // 原地换绑：TextureId 不变，本帧 egui pass 即
                            // 采样新视图。
                            if let (Some(view), Some(tex)) = (
                                state.renderer.offscreen_view(sid),
                                state.pane_textures.get(&sid),
                            ) {
                                state.egui_renderer.update_egui_texture_from_wgpu_texture(
                                    state.renderer.device(),
                                    view,
                                    wgpu::FilterMode::Nearest,
                                    *tex,
                                );
                            }
                            // 新建的纹理是空的：本帧必须渲染该窗格，否则
                            // egui 采样到全黑（即使正处同步区间，半成品也
                            // 好过黑屏闪烁）。
                            skip_pane[i] = false;
                        }
                        // 行列数同时受窗格矩形与 cell 尺寸（设置页字体/
                        // 字号）影响，每帧对照网格检测（廉价的整数比较）。
                        // 分屏后各窗格尺寸不同：逐窗格 resize（term +
                        // PTY）。后台 tab 的窗格不在布局里、不在此 resize
                        // ——切换激活的首帧先走到这里 resize 再渲染，旧
                        // 行列的画面不会上屏。设置页改字号即时生效走的就
                        // 是这条链路：cell 尺寸变 → 行列数变 → resize。
                        //
                        // M4.1 批C：footer 扣高（feature = "input-editor"）。
                        // 聚焦窗格按当前模式计算 footer 高度；非聚焦窗格无 footer。
                        // AltScreen / Fallback 隐藏 footer（footer_px=0）→ 一进一出
                        // 各一次 resize，与整窗 resize 走同一路径（window_just_resized
                        // 豁免已覆盖，见 B3-8 注释），不额外处理。
                        // 常驻等高铁律：Compose↔Running footer_px 相同 → 不触发 resize。
                        #[cfg(feature = "input-editor")]
                        let footer_px_for_resize = {
                            let pane_idx = i;
                            let is_focused = state.tabs[state.active_tab].focused == pane_idx;
                            if is_focused {
                                let pane = &state.tabs[state.active_tab].panes[i];
                                let mode = mode::effective_mode(&pane.term, state.force_fallback);
                                let cv = composer::compose_view_for_mode(
                                    mode,
                                    pane.editor.view(),
                                    pane.preedit.clone(),
                                    pane.exit_badge.clone(),
                                    None, // ghost 仅用于渲染，resize 高度计算不需要
                                );
                                let (_, cell_h) = state.renderer.cell_size();
                                let fp = state.renderer.padding() * 0.4;
                                let max_h = th as f32 / 3.0;
                                let target_h = lumen_renderer::composer_view::footer_height_px(
                                    Some(&cv),
                                    cell_h,
                                    fp,
                                    max_h,
                                );
                                // M4.1 批D2：增高防抖（100ms）。
                                // 目标高度变化时更新 footer_target_h 和 changed_at。
                                let s = &mut state.tabs[state.active_tab].panes[i];
                                if (target_h - s.footer_target_h).abs() >= 0.5 {
                                    s.footer_target_h = target_h;
                                    s.footer_h_changed_at = Instant::now();
                                }
                                // 纯函数判定：是否允许提交给 renderer/resize。
                                let should_commit = history::footer_height_debounce(
                                    s.footer_committed_h,
                                    s.footer_target_h,
                                    s.footer_h_changed_at,
                                    Instant::now(),
                                );
                                if should_commit {
                                    s.footer_committed_h = s.footer_target_h;
                                }
                                s.footer_committed_h
                            } else {
                                0.0_f32
                            }
                        };
                        #[cfg(not(feature = "input-editor"))]
                        let footer_px_for_resize: f32 = 0.0;
                        let (rows, cols) =
                            state
                                .renderer
                                .grid_size_for_with_footer(tw, th, footer_px_for_resize);
                        // M4.1 批C 冒烟观测点：首帧可见 footer 占高生效。
                        // 日志示例：「footer 占高 32px，网格 {rows}x{cols}
                        //            （无 footer 时多 1-2 行）」
                        if footer_px_for_resize > 0.0 {
                            log::debug!(
                                "M4.1 批C：窗格 id={} footer 占高 {:.0}px \
                                 → 网格 {rows}x{cols}（窗格 {tw}x{th}）",
                                state.tabs[state.active_tab].panes[i].id,
                                footer_px_for_resize
                            );
                        }
                        let s = &mut state.tabs[state.active_tab].panes[i];
                        let (old_rows, old_cols) = {
                            let g = s.term.grid();
                            (g.rows(), g.cols())
                        };
                        if divider_resize_held && (rows, cols) != (old_rows, old_cols) {
                            // B3-8 诊断：分隔条拖动中暂缓 resize，记录被挡
                            // 的尺寸变化，帮助取证 held 是否误触。
                            log::debug!(
                                "B3-8 诊断：窗格 id={} 网格变化 {old_rows}x{old_cols} → \
                                 {rows}x{cols} 因 divider_resize_held=true 暂缓",
                                s.id
                            );
                        }
                        if !divider_resize_held && (rows, cols) != (old_rows, old_cols) {
                            // 观测点（B2）：幅度可核对恢复路径估算的精度
                            // ——估算到位时首帧 resize 应为 ±1 级微调。
                            log::debug!(
                                "窗格 id={} 网格 {old_rows}x{old_cols} → {rows}x{cols}",
                                s.id
                            );
                            s.term.resize(rows, cols);
                            // resize 失败 = term 与 ConPTY 几何失步（丢字
                            // /错位的温床），必须可观测（B2 修复：不再
                            // `let _ =` 静默吞掉）。
                            if let Err(e) = s.pty.resize(rows as u16, cols as u16) {
                                log::warn!(
                                    "窗格 id={} 的 PTY resize 到 {rows}x{cols} 失败: {e:#}",
                                    s.id
                                );
                            }
                            // B3-7：已知限制——窄窗格提示符折行后经历宽度变化，
                            // 当前提示符行打字会错位至用户回车自愈。根因为
                            // PSReadLine 上游缺陷（锚点不随解折行重测，WT #2432/#15042
                            // 同款），终端侧无非侵入手段，接受现状。
                            // resize 后注入 \r 的方案（B3-5/B3-5b/B3-6）经海风哥实测
                            // 否决：会产生多余提示符行，已全部拆除。
                            // 尺寸变化会夹紧光标位置，立即同步绘制态。
                            let g = s.term.grid();
                            s.cursor_displayed = (g.cursor.row, g.cursor.col, g.cursor.visible);
                            // 网格已重排（字号变更等可不伴随纹理重建）：
                            // 旧帧内容与新行列数不匹配，本帧必须渲染。
                            skip_pane[i] = false;
                        }
                    }
                } else {
                    state.window.request_redraw();
                }

                // —— 终端管线渲染到各窗格离屏纹理（damage/行缓存机制
                // 原样，行缓存按会话 id 隔离）——同步区间门控跳过的窗
                // 格不渲染：其纹理保留上一完整帧，egui pass 照常采样
                // 合成（渲染计划在途，ESU 后补画）。
                let mut rendered = 0usize;
                if structure_unchanged {
                    // M4.1 批C：按当前有效模式组装 ComposerView（feature = "input-editor"）。
                    // 节拍纪律（设计稿 §7.4）：编辑器重绘直接 request_redraw，
                    // 不挂 PTY debounce。此处仅按模式组装视图数据，无副作用。
                    #[cfg(feature = "input-editor")]
                    let footer_view = {
                        // M4.1 批3：ghost text 缓存（revision 变化时重算）。
                        // 先独立更新缓存（不持有 focused 借用），再组装视图。
                        {
                            let ti2 = state.active_tab;
                            let pi2 = state.tabs[ti2].focused;
                            let rev = state.tabs[ti2].panes[pi2].editor.revision();
                            if state.ghost_cache.0 != rev {
                                let text =
                                    state.tabs[ti2].panes[pi2].editor.view().text().to_owned();
                                let ghost = if text.contains('\n') || text.is_empty() {
                                    log::debug!(
                                        "[ghost_cache] 跳过：text 为空或多行 len={} has_nl={}",
                                        text.len(),
                                        text.contains('\n')
                                    );
                                    None
                                } else {
                                    let g = state.history.find_ghost_prefix(&text);
                                    log::debug!(
                                        "[ghost_cache] rev={rev} text={:?} ghost={:?}",
                                        text,
                                        g
                                    );
                                    g
                                };
                                state.ghost_cache = (rev, ghost);
                            }
                        }
                        let ghost = state.ghost_cache.1.clone();
                        let focused = state.focused_pane();
                        let mode = mode::effective_mode(&focused.term, state.force_fallback);
                        composer::compose_view_for_mode(
                            mode,
                            focused.editor.view(),
                            focused.preedit.clone(),
                            focused.exit_badge.clone(),
                            ghost,
                        )
                    };

                    for (i, skip) in skip_pane.iter().enumerate() {
                        if *skip {
                            continue;
                        }
                        let s = &mut state.tabs[state.active_tab].panes[i];
                        s.term_frame_due_since = None;
                        let s = &state.tabs[state.active_tab].panes[i];
                        // 防抖光标态整组传入：不可见时行号仍是运行中块
                        // 状态条的下边界（与光标同源防抖，块条几何帧间
                        // 连续）。
                        // M4.1 批C：feature = "input-editor" 开启时用
                        // render_with_composer 传入 footer 视图；flag 剔除时用 render。
                        // 只有聚焦窗格显示 footer；非聚焦窗格传 None = 无 footer。
                        // 批D 起按各窗格独立模式组装（多窗格各自有 footer）。
                        #[cfg(feature = "input-editor")]
                        let render_result = {
                            let composer_view = if state.tabs[state.active_tab].focused == i {
                                Some(&footer_view)
                            } else {
                                None
                            };
                            state.renderer.render_with_composer(
                                s.id,
                                &s.term,
                                s.selection.as_ref(),
                                s.cursor_displayed,
                                s.selected_block,
                                composer_view,
                            )
                        };
                        #[cfg(not(feature = "input-editor"))]
                        let render_result = state.renderer.render(
                            s.id,
                            &s.term,
                            s.selection.as_ref(),
                            s.cursor_displayed,
                            s.selected_block,
                        );
                        if let Err(e) = render_result {
                            error!("渲染失败: {e:#}");
                        }
                        rendered += 1;
                    }
                }
                if rendered > 0 {
                    // ESU 直渲限频基准（整帧粒度，多窗格共享）。
                    state.last_term_render_at = Some(render_t0);
                }

                // —— egui 平台输出 + IME 强制复位（IME 最大坑对策）——
                // egui 会按自己的文本焦点开关整窗 IME / 挪动候选框；终端
                // 聚焦时必须在 handle_platform_output **之后**强制复位，
                // 并把候选框钉在**焦点窗格**光标所在格子（窗格原点 +
                // cell×行列；首帧矩形未知时跳过本帧定位，允许位仍复位）。
                state
                    .egui_state
                    .handle_platform_output(&state.window, full_output.platform_output);
                if state.terminal_focused {
                    state.window.set_ime_allowed(true);
                    if let Some((px, py, pw, ph)) = state.focused_pane_rect_px() {
                        let s = state.focused_pane();
                        let (cw, ch) = state.renderer.cell_size();

                        // M4.1 批D2：Compose 态时 IME 候选框跟随 footer 内编辑器光标。
                        // 其余态：跟随终端光标所在格子（原行为）。
                        #[cfg(feature = "input-editor")]
                        let (ime_x, ime_y) = {
                            let mode = mode::effective_mode(&s.term, state.force_fallback);
                            if mode == crate::mode::InputMode::Compose {
                                // footer 光标位置：footer 在窗格底部
                                // cursor = (行, 字节偏移)，此处近似为字符列 = cursor.1 / cell_w
                                // 精确列需要 ComposerView 里的字节转 glyph 列，
                                // 此处用字节偏移粗估（常用 ASCII 场景 1:1，CJK 偏差 1 格内）。
                                let cv_cursor = s.editor.view().cursor();
                                let footer_top_y = py + ph
                                    - ch * (s.editor.view().line_count().max(1) as f32)
                                    - state.renderer.padding() * 0.8;
                                let col_approx = cv_cursor.byte.min(200) as f32;
                                let footer_x = px + col_approx * cw;
                                let footer_y = footer_top_y + cv_cursor.line as f32 * ch;
                                (footer_x, footer_y)
                            } else {
                                let g = s.term.grid();
                                let view_row = (g.display_offset() + s.cursor_displayed.0)
                                    .min(g.rows().saturating_sub(1));
                                let (cx, cy) =
                                    state.renderer.cell_origin(view_row, s.cursor_displayed.1);
                                (px + cx, py + cy)
                            }
                        };
                        #[cfg(not(feature = "input-editor"))]
                        let (ime_x, ime_y) = {
                            let g = s.term.grid();
                            let view_row = (g.display_offset() + s.cursor_displayed.0)
                                .min(g.rows().saturating_sub(1));
                            let (cx, cy) =
                                state.renderer.cell_origin(view_row, s.cursor_displayed.1);
                            (px + cx, py + cy)
                        };
                        let _ = pw; // pw 仅防未使用 warning（IME 候选框宽度用 cw）
                        state.window.set_ime_cursor_area(
                            winit::dpi::PhysicalPosition::new(ime_x as f64, ime_y as f64),
                            winit::dpi::PhysicalSize::new(cw as f64, ch as f64),
                        );
                    }
                }

                // —— egui 渲染到 surface（单 pass，Clear 装载）——
                let clipped = state.egui_ctx.tessellate(full_output.shapes, ppp);
                let (sw, sh) = state.renderer.surface_size();
                let screen = egui_wgpu::ScreenDescriptor {
                    size_in_pixels: [sw, sh],
                    pixels_per_point: ppp,
                };
                let device = state.renderer.device();
                let queue = state.renderer.queue();
                for (id, delta) in &full_output.textures_delta.set {
                    state
                        .egui_renderer
                        .update_texture(device, queue, *id, delta);
                }
                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("lumen egui frame"),
                });
                let user_cmds = state.egui_renderer.update_buffers(
                    device,
                    queue,
                    &mut encoder,
                    &clipped,
                    &screen,
                );
                let surface_view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                {
                    let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("lumen egui pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &surface_view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(
                                    state.renderer.theme().background.to_wgpu(),
                                ),
                                store: wgpu::StoreOp::Store,
                            },
                            depth_slice: None,
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    // egui 的 render() 要求 'static 生命周期 pass；
                    // forget_lifetime 之后不得再操作父 encoder。
                    let mut pass = pass.forget_lifetime();
                    state.egui_renderer.render(&mut pass, &clipped, &screen);
                }
                queue.submit(user_cmds.into_iter().chain([encoder.finish()]));
                frame.present();
                for id in &full_output.textures_delta.free {
                    state.egui_renderer.free_texture(id);
                }
                // 已关闭窗格的纹理注销（呈现后才安全：关闭动作发生在
                // run_ui 之后时，本帧 shape 仍引用该纹理 id）。
                for id in state.pending_tex_free.drain(..) {
                    state.egui_renderer.free_texture(&id);
                }

                // —— egui 重绘计划：与终端节拍在 about_to_wait 合流 ——
                let repaint_delay = full_output
                    .viewport_output
                    .get(&egui::ViewportId::ROOT)
                    .map_or(Duration::MAX, |v| v.repaint_delay);
                // 仅记录异常值（动画/立即重绘请求），用于空转监控。
                if repaint_delay < Duration::from_secs(3600) {
                    state.perf_log(format_args!("egui repaint_delay {repaint_delay:?}"));
                }
                state.egui_repaint_at = if repaint_delay == Duration::ZERO {
                    // 动画进行中要求立即重绘——但同样受 8ms 合帧下限
                    // 约束：帧尾直接 request_redraw 会形成「画完即请求
                    // 下一帧」的紧循环（实测启动动画期间每 ~0.4ms 一帧、
                    // 千帧每秒级白占主线程）。改排计划由 about_to_wait
                    // 统一调度，动画以 ~125fps 推进（视觉无差异）。
                    Some(render_t0 + Duration::from_millis(8))
                } else if repaint_delay < Duration::from_secs(3600) {
                    Some(render_t0 + repaint_delay)
                } else {
                    None // 「无限远」：无需主动重绘
                };

                // —— 埋点（沿用 M2 字段，便于打字延迟基线对比）——
                let gap = state
                    .last_render_at
                    .map(|t| render_t0.duration_since(t))
                    .unwrap_or_default();
                state.last_render_at = Some(render_t0);
                let key_to_screen = state
                    .last_key_at
                    .take()
                    .map(|t| format!(" 键→上屏 {:?}", t.elapsed()))
                    .unwrap_or_default();
                let skipped = skip_pane.iter().filter(|s| **s).count();
                let term_mark = if !structure_unchanged {
                    " 终端=跳过(结构变更)".to_owned()
                } else if skipped > 0 {
                    format!(" 终端跳过 {skipped}/{} 窗格(同步区间)", skip_pane.len())
                } else {
                    String::new()
                };
                state.perf_log(format_args!(
                    "render 耗时 {:?} 距上帧 {gap:?}{key_to_screen}{term_mark}",
                    render_t0.elapsed()
                ));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        drain_order, estimate_restored_pane_px, load_icon, maximized_overflow,
        width_worth_persisting, PaneLayout,
    };

    // ── 第二十二轮：运行时图标加载单元测试 ─────────────────────────────

    #[test]
    fn 图标加载_32px_解码成功() {
        let bytes = include_bytes!("../../../icons/lumen-icon-32.png");
        let icon = load_icon(bytes);
        assert!(icon.is_some(), "lumen-icon-32.png 应解码成功");
    }

    #[test]
    fn 图标加载_64px_解码成功() {
        let bytes = include_bytes!("../../../icons/lumen-icon-64.png");
        let icon = load_icon(bytes);
        assert!(icon.is_some(), "lumen-icon-64.png 应解码成功");
    }

    #[test]
    fn 图标加载_损坏字节_返回_none() {
        // 非法字节流：load_icon 应返回 None 而非 panic。
        let icon = load_icon(b"\x00\x01\x02\x03_not_a_png");
        assert!(icon.is_none(), "损坏字节流应返回 None");
    }

    /// 估算测试区域：与 layout.rs 测试同款 304x202（宽对 3 列、高对
    /// 2 排整除：上3下2 时上排格 100x100、下排格 151x100）。
    fn est_area() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(304.0, 202.0))
    }

    #[test]
    fn 恢复估算_五格上3下2_扣标题栏() {
        // B2 修复断言：估算必须与 shell 首帧同源——布局矩形扣
        // 24px 窗格标题栏，再乘 DPI 缩放取整。
        let px = estimate_restored_pane_px(est_area(), &PaneLayout::uniform(5), 5, None, 2.0);
        assert_eq!(px.len(), 5);
        // 上排格 100x100 逻辑 → 内容 100x76 → 物理 ×2。
        assert_eq!(px[0], (200, 152));
        assert_eq!(px[2], (200, 152));
        // 下排格 151x100 逻辑。
        assert_eq!(px[3], (302, 152));
        assert_eq!(px[4], (302, 152));
    }

    #[test]
    fn 恢复估算_最大化格按整区其余按布局() {
        let px = estimate_restored_pane_px(est_area(), &PaneLayout::uniform(2), 2, Some(0), 1.0);
        // 最大化格独占整区（304x202 − 24 标题栏）。
        assert_eq!(px[0], (304, 178));
        // 隐藏格按布局矩形（两格左右分 151x202；还原最大化时回到它，
        // 届时 resize 近似无损）。
        assert_eq!(px[1], (151, 178));
    }

    #[test]
    fn 恢复估算_布局形状不符回退均分() {
        // 布局是 3 格形状、实际 2 格（防御路径）：按 2 格均分计算，
        // 不 panic、数量对位。
        let px = estimate_restored_pane_px(est_area(), &PaneLayout::uniform(3), 2, None, 1.0);
        assert_eq!(px.len(), 2);
        assert_eq!(px[0], (151, 178));
    }

    #[test]
    fn 焦点窗格最先_激活tab次之() {
        // 3 个 tab 窗格数 2/3/1，激活 tab=1、焦点窗格=2：焦点最先，
        // 激活 tab 其余窗格次之（可见），后台 tab 按下标序殿后。
        assert_eq!(
            drain_order(&[2, 3, 1], 1, 2),
            vec![(1, 2), (1, 0), (1, 1), (0, 0), (0, 1), (2, 0)]
        );
        assert_eq!(drain_order(&[3], 0, 0), vec![(0, 0), (0, 1), (0, 2)]);
    }

    #[test]
    fn 单窗格与空列表() {
        assert_eq!(drain_order(&[1], 0, 0), vec![(0, 0)]);
        assert!(drain_order(&[], 0, 0).is_empty());
    }

    #[test]
    fn 下标越界时退化为顺序遍历() {
        // 激活 tab 越界：全部按下标序。
        assert_eq!(drain_order(&[2], 7, 0), vec![(0, 0), (0, 1)]);
        // 焦点窗格越界：激活 tab 仍领先，但无焦点优先项。
        assert_eq!(drain_order(&[1, 2], 1, 9), vec![(1, 0), (1, 1), (0, 0)]);
    }

    #[test]
    fn 宽度写盘判定_正常变化才写() {
        // 范围内且差 ≥1px：写。
        assert!(width_worth_persisting(240.0, 180.0, 140.0, 320.0));
        assert!(width_worth_persisting(180.0, 240.0, 140.0, 320.0));
        // 差 <1px（亚像素抖动/无变化）：不写。
        assert!(!width_worth_persisting(180.5, 180.0, 140.0, 320.0));
        assert!(!width_worth_persisting(180.0, 180.0, 140.0, 320.0));
        // 端点 ±1 容差内：写（面板钳到 min/max 是用户主动拖到头）。
        assert!(width_worth_persisting(139.5, 180.0, 140.0, 320.0));
        assert!(width_worth_persisting(320.8, 180.0, 140.0, 320.0));
    }

    #[test]
    fn 宽度写盘判定_瞬态与非法不写() {
        // 窗口过窄被临时压缩到范围之外：不写（重启还原用户值）。
        assert!(!width_worth_persisting(80.0, 180.0, 140.0, 320.0));
        assert!(!width_worth_persisting(500.0, 180.0, 140.0, 320.0));
        // NaN/Inf 防御：不写。
        assert!(!width_worth_persisting(f32::NAN, 180.0, 140.0, 320.0));
        assert!(!width_worth_persisting(f32::INFINITY, 180.0, 140.0, 320.0));
    }

    // ── 第十轮问题1：最大化越界纯函数测试 ──────────────────────────────

    #[test]
    fn 最大化越界_标准8px() {
        // 2560×1440 屏幕（工作区），窗口 rect (-8,-8)~(2568,1400)
        // 实测典型值：四边各 8px 越界
        let win = (-8, -8, 2568, 1400);
        let work = (0, 0, 2560, 1440);
        let (l, t, r, b) = maximized_overflow(win, work);
        assert_eq!(l, 8, "左越界应为 8px");
        assert_eq!(t, 8, "顶越界应为 8px");
        assert_eq!(r, 8, "右越界应为 8px");
        assert_eq!(b, 0, "底未越界应为 0（工作区底在任务栏上方）");
    }

    #[test]
    fn 最大化越界_非最大化时全零() {
        // 正常非最大化窗口在工作区内：所有越界量为 0
        let win = (100, 100, 1100, 740);
        let work = (0, 0, 2560, 1440);
        let (l, t, r, b) = maximized_overflow(win, work);
        assert_eq!((l, t, r, b), (0, 0, 0, 0), "非最大化时无越界");
    }

    #[test]
    fn 最大化越界_跨显示器负坐标() {
        // 副显示器在主屏左侧（工作区 x=-1920..0，y=0..1080）
        // 最大化时窗口 rect (-1928,-8)~(8,1072)
        let win = (-1928, -8, 8, 1072);
        let work = (-1920, 0, 0, 1080);
        let (l, t, r, b) = maximized_overflow(win, work);
        assert_eq!(l, 8, "副显示器左越界应为 8px");
        assert_eq!(t, 8, "副显示器顶越界应为 8px");
        assert_eq!(r, 8, "副显示器右越界应为 8px");
        assert_eq!(b, 0, "副显示器底未越界");
    }

    #[test]
    fn 最大化越界_底部也有越界() {
        // 部分配置下底部也越界（任务栏很高时）
        let win = (-8, -8, 2568, 1448);
        let work = (0, 0, 2560, 1400);
        let (l, t, r, b) = maximized_overflow(win, work);
        assert_eq!(l, 8);
        assert_eq!(t, 8);
        assert_eq!(r, 8);
        assert_eq!(b, 48, "底部越界量应正确计算");
    }

    // ── M4.1 批D1：提交编码纯函数测试（设计稿 §3.2 步骤 2）─────────

    #[cfg(feature = "input-editor")]
    mod submit_encoding {
        use super::super::encode_submit;

        #[test]
        fn 单行文本_末尾加_cr() {
            let payload = encode_submit("echo hello");
            assert_eq!(payload, b"echo hello\r", "单行提交应为 text + CR");
        }

        #[test]
        fn 空文本_仍加_cr() {
            let payload = encode_submit("");
            assert_eq!(payload, b"\r", "空文本提交应为单个 CR");
        }

        #[test]
        fn 多行文本_括号粘贴协议包裹() {
            let text = "line1\nline2";
            let payload = encode_submit(text);
            assert!(
                payload.starts_with(b"\x1b[200~"),
                "多行提交应以 ESC[200~ 开头"
            );
            assert!(
                payload.ends_with(b"\x1b[201~\r"),
                "多行提交应以 ESC[201~CR 结尾"
            );
            let inner = &payload[6..payload.len() - 7];
            assert_eq!(inner, text.as_bytes(), "多行提交括号内容应为原始文本");
        }

        #[test]
        fn 两行判定阈值_仅两行走括号粘贴() {
            // 恰好两行（含一个 \n）→ 括号粘贴
            let payload = encode_submit("a\nb");
            assert!(
                payload.starts_with(b"\x1b[200~"),
                "两行文本应走括号粘贴协议"
            );
        }
    }
}
