//! Lumen 主程序：winit 事件循环，组装 PTY → 终端状态机 → 渲染器 → egui 外壳。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod input;
mod profile;
mod session;
mod sessions_store;
mod settings;
mod shell;

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
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

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

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let event_loop = EventLoop::<PtyWake>::with_user_event()
        .build()
        .context("创建事件循环失败")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App { proxy, state: None };
    event_loop.run_app(&mut app).context("事件循环异常退出")?;
    Ok(())
}

struct App {
    proxy: EventLoopProxy<PtyWake>,
    state: Option<AppState>,
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
    /// 最近一次写盘的会话列表快照（F4 持久化去重：cwd 上报/结构
    /// 变更都先与它比对，无变化不重复写盘）。None = 本次运行尚未写。
    last_sessions_snapshot: Option<sessions_store::SessionsFile>,
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
    fn sel_point_at_mouse(&self) -> Option<SelPoint> {
        let (x, y, w, h) = self.focused_pane_rect_px()?;
        let (row, col) = self.renderer.cell_at(
            self.mouse_pos.0 - x as f64,
            self.mouse_pos.1 - y as f64,
            w.max(1.0) as u32,
            h.max(1.0) as u32,
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
    fn new_pane(&mut self) {
        if self.tabs[self.active_tab].panes.len() >= MAX_PANES {
            self.shell_state.toast.push(
                shell::toast::ToastKind::Warn,
                format!("每个会话最多 {MAX_PANES} 个窗格"),
            );
            // push 不在 egui 帧内：请求一帧立即显示。
            self.window.request_redraw();
            return;
        }
        let focused = self.focused_pane();
        let g = focused.term.grid();
        let (rows, cols) = (g.rows(), g.cols());
        let cwd = focused
            .term
            .cwd()
            .map(std::path::Path::to_path_buf)
            .or_else(|| focused.initial_cwd.clone())
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
                tab.panes.push(s);
                tab.focused = tab.panes.len() - 1;
                // 布局变化：下一帧 egui 产出新窗格矩形并触发逐窗格
                // 离屏重建 + term/pty resize。
                self.update_window_title();
                self.window.request_redraw();
                // 增窗格是结构性变更：落盘（F5）。
                self.persist_sessions();
            }
            Err(e) => {
                error!("新建窗格失败: {e:#}");
                self.shell_state
                    .toast
                    .push(shell::toast::ToastKind::Error, format!("新建窗格失败：{e}"));
                self.window.request_redraw();
            }
        }
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
        snap.save();
        self.last_sessions_snapshot = Some(snap);
    }
}

impl App {
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<AppState> {
        let attrs = Window::default_attributes()
            .with_title("Lumen")
            .with_inner_size(winit::dpi::LogicalSize::new(1000.0, 640.0));
        let window = Arc::new(event_loop.create_window(attrs).context("创建窗口失败")?);
        window.set_ime_allowed(true);
        // 告知输入法处于终端语境（egui-winit 内部有同等映射）。
        window.set_ime_purpose(winit::window::ImePurpose::Terminal);

        let size = window.inner_size();
        let scale = window.scale_factor() as f32;
        let mut renderer = Renderer::new(window.clone(), size.width, size.height, scale)
            .context("初始化渲染器失败")?;

        // —— 设置加载与应用（settings.json；缺失/损坏降级默认值）——
        let app_settings = settings::Settings::load();
        let ap = &app_settings.appearance;
        let actual_family = renderer.reconfigure_font(&ap.font_family, ap.font_size);
        renderer.set_theme(ap.theme.terminal_theme());
        info!(
            "设置加载：主题 {} 字号 {} 字体「{}」→ 实际生效「{actual_family}」",
            ap.theme.display_name(),
            ap.font_size,
            if ap.font_family.is_empty() {
                "自动"
            } else {
                &ap.font_family
            }
        );
        // 字体回退提示（设置页 Appearance 展示）。
        let font_hint = (!ap.font_family.is_empty()
            && !actual_family.eq_ignore_ascii_case(&ap.font_family))
        .then(|| {
            format!(
                "系统中未找到「{}」，已回退「{actual_family}」",
                ap.font_family
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
        shell::theme::apply_style(
            &egui_ctx,
            shell::theme::palette(app_settings.appearance.theme.is_light()),
        );
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
        let sidebar_px = (shell::SIDEBAR_WIDTH * scale).round();
        let topbar_px = (shell::topbar::HEIGHT * scale).round();
        let term_w = ((size.width as f32 - sidebar_px).max(1.0)) as u32;
        let term_h = ((size.height as f32 - topbar_px).max(1.0)) as u32;

        // 行列数先按整个终端区估算（分屏恢复时各窗格首帧会按实际
        // 矩形 resize，spawn 值只影响 shell 启动瞬间的报告尺寸）。
        let (rows, cols) = renderer.grid_size_for(term_w, term_h);
        info!("终端尺寸: {rows} 行 x {cols} 列");

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
        if let Some(stored) = &stored {
            for tab_entry in &stored.tabs {
                let mut panes: Vec<Session> = Vec::new();
                for pane_entry in &tab_entry.panes {
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
                    match Session::spawn(
                        next_session_id,
                        rows,
                        cols,
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
                let focused = tab_entry.focused.min(panes.len() - 1);
                tabs.push(Tab {
                    id: next_tab_id,
                    custom_title: tab_entry.custom_title.clone(),
                    panes,
                    focused,
                });
                next_tab_id += 1;
            }
            if !tabs.is_empty() {
                active_idx = stored.active.min(tabs.len() - 1);
                let pane_total: usize = tabs.iter().map(|t| t.panes.len()).sum();
                info!(
                    "会话恢复：{} 个 tab / {pane_total} 个窗格，激活 #{active_idx}（cwd 失效 {stale_cwd} 个）",
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
            last_sessions_snapshot: None,
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
            terminal_focused: true,
            egui_repaint_at: None,
            was_popup_open: false,
            shell_state: shell::ShellState::default(),
        };
        state.shell_state.settings.font_hint = font_hint;
        // 恢复条目中保存的 cwd 已失效：回退默认目录并提示一次（F4）。
        if stale_cwd > 0 {
            state.shell_state.toast.push(
                shell::toast::ToastKind::Warn,
                format!("{stale_cwd} 个会话的保存目录已失效，已回退默认目录"),
            );
        }
        // 窗口标题对齐激活会话（恢复多会话时 active 可能非 0）。
        state.update_window_title();
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
                        state.tabs[ti].panes[pi].term.advance(&bytes);
                        got_data[k] = true;
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

        // —— 生命周期：shell 退出关闭对应**窗格**（F5：最后一个窗格
        // 才关 tab，最后一个 tab 关闭才退出应用）。
        for sid in exited {
            let Some((ti, pi)) = state.find_pane(sid) else {
                continue;
            };
            info!("会话 id={sid} 的 shell 已退出，关闭对应窗格");
            if state.close_pane(ti, pi) {
                info!("最后一个会话已关闭，退出应用");
                event_loop.exit();
                return;
            }
        }

        // 后台数据滞留：补发一个 wake 接着消化（与转发线程同一套去重）。
        if backlog
            && !state.wake_pending.swap(true, Ordering::AcqRel)
            && state.proxy.send_event(PtyWake).is_err()
        {
            error!("补发 PtyWake 失败：事件循环已关闭");
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
                ));
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
                event_loop.exit();
            }
            WindowEvent::ModifiersChanged(mods) => state.modifiers = mods.state(),
            WindowEvent::Resized(size) => {
                state.renderer.resize_surface(size.width, size.height);
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
                state.renderer.set_scale_factor(scale_factor as f32);
                let ap = &state.settings.appearance;
                state
                    .renderer
                    .reconfigure_font(&ap.font_family, ap.font_size);
                state.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                use winit::keyboard::{Key, NamedKey};
                let pressed = event.state == ElementState::Pressed;
                // —— 外壳级快捷键：Ctrl+T 新建 / Ctrl+W 关闭当前 /
                // Ctrl+Tab 下一个（Ctrl+Shift+Tab 上一个）/
                // Ctrl+B 文件树开合 / Ctrl+, 设置页开合 ——
                // 路由规则：非 alt-screen 时优先于终端直通拦截（窗口级，
                // 终端是否聚焦都生效）；**alt screen 激活时全部直通终端
                // 不拦截**——vim 的 Ctrl+W 是窗口操作前缀键、全屏 TUI
                // 也可能吃 Ctrl+Tab，抢按键会毁掉用户操作；重命名编辑
                // 中按键归 egui 的输入框，同样不拦截。
                // 例外：覆盖层（设置/登录）已打开时按键根本到不了终端
                // （terminal_focused=false 的闸在下方拦截），「让键给
                // vim」的前提不成立——放行快捷键块，否则 alt screen 下
                // Ctrl+, 关不掉已打开的设置页（开关不对称，按键被静默
                // 吞掉）。match 内部的 login/settings 守卫臂保证放行后
                // 唯一新增的行为就是 Ctrl+, 的关闭路径。
                let overlay_open = state.shell_state.settings.open || state.shell_state.login.open;
                // —— 分屏快捷键（F5 批1 验证通道；+ 按钮归批次2）——
                // Ctrl+Shift+D 焦点 tab 内新增窗格 / Ctrl+Shift+W 关闭
                // 焦点窗格（最后一个窗格 = 关整个 tab）。守卫与外壳快
                // 捷键一致：重命名/文件树对话框/覆盖层打开时让位 egui；
                // **alt screen 不让位、全局拦截**（裁决：vim 的 Ctrl+W
                // 前缀不带 Shift、全屏 TUI 几乎不绑 Ctrl+Shift+字母；
                // 且传统键盘编码下 Ctrl+Shift+W 与 Ctrl+W 字节相同，
                // 放行也只会被对端当成裸 ^W——拦截收益大于冲突风险）。
                if pressed
                    && state.modifiers.control_key()
                    && state.modifiers.shift_key()
                    && !overlay_open
                    && state.shell_state.renaming.is_none()
                    && !state.shell_state.filetree.dialog_open()
                {
                    if let Key::Character(c) = &event.logical_key {
                        if c.eq_ignore_ascii_case("d") {
                            state.new_pane();
                            return;
                        }
                        if c.eq_ignore_ascii_case("w") {
                            let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);
                            if state.close_pane(ti, pi) {
                                info!("最后一个会话已关闭，退出应用");
                                event_loop.exit();
                            }
                            return;
                        }
                    }
                }
                if pressed
                    && state.modifiers.control_key()
                    && (overlay_open || !state.focused_pane().term.is_alt_screen())
                    && state.shell_state.renaming.is_none()
                    // 文件树对话框（新建/删除确认）打开期间键盘归 egui
                    // 的输入框，外壳快捷键不响应（Ctrl+W 关会话等）。
                    && !state.shell_state.filetree.dialog_open()
                {
                    let shift = state.modifiers.shift_key();
                    match &event.logical_key {
                        // 登录覆盖层打开期间外壳快捷键全部不响应（键盘归
                        // egui 的输入框，Esc 关闭由 Modal 处理）；按键也
                        // 不会直通 PTY——下方 terminal_focused=false 的闸
                        // 会拦住。
                        _ if state.shell_state.login.open => {}
                        // Ctrl+, 开/关设置页（终端焦点与 IME 路由随之切换）。
                        Key::Character(c) if !shift && c.as_str() == "," => {
                            if state.shell_state.settings.open {
                                state.shell_state.settings.open = false;
                                state.terminal_focused = true;
                            } else {
                                state.shell_state.settings.open_with(&state.settings);
                                state.terminal_focused = false;
                            }
                            state.window.request_redraw();
                            return;
                        }
                        // 设置页打开期间其余外壳快捷键不响应（避免在覆盖
                        // 层背后偷偷增删会话）；按键也不会直通 PTY——
                        // 下方 terminal_focused=false 的闸会拦住。
                        _ if state.shell_state.settings.open => {}
                        Key::Character(c) if !shift && c.eq_ignore_ascii_case("t") => {
                            state.new_tab();
                            return;
                        }
                        Key::Character(c) if !shift && c.eq_ignore_ascii_case("w") => {
                            // Ctrl+W 关整个 tab（关焦点窗格是 Ctrl+Shift+W）。
                            if state.close_tab(state.active_tab) {
                                info!("最后一个会话已关闭，退出应用");
                                event_loop.exit();
                            }
                            return;
                        }
                        Key::Character(c) if !shift && c.eq_ignore_ascii_case("b") => {
                            // 文件树开合：终端区宽度随之变化，下一帧
                            // egui 布局产出新矩形并触发离屏重建+resize。
                            let ft = &mut state.shell_state.filetree;
                            ft.visible = !ft.visible;
                            state.window.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::Tab) => {
                            state.cycle_tab(if shift { -1 } else { 1 });
                            return;
                        }
                        _ => {}
                    }
                }
                // 终端聚焦时键盘绕过 egui 直通此处；非聚焦时按键归
                // egui，无论它是否消费都不再写 PTY（点了侧栏还往
                // shell 灌字节是事故）。
                if !state.terminal_focused {
                    return;
                }
                // 键盘直通焦点窗格（F5：键盘/IME/粘贴/块操作全跟焦点）。
                // 用字段级下标而非访问器：clipboard 等字段需要同时可变
                // 借用，访问器会借住整个 state。
                let (ti, pi) = (state.active_tab, state.tabs[state.active_tab].focused);
                // 抬起事件仅在 win32-input-mode 下投递（协议需要 Kd=0）。
                if !pressed {
                    if state.tabs[ti].panes[pi].term.win32_input()
                        && std::env::var_os("LUMEN_WIN32_INPUT").is_some()
                    {
                        if let Some(bytes) = input::encode_key_win32(&event, state.modifiers, false)
                        {
                            if let Err(e) = state.tabs[ti].panes[pi].pty.write(&bytes) {
                                error!("写入 PTY 失败: {e:#}");
                            }
                        }
                    }
                    return;
                }
                // Shift+PgUp/PgDn 本地翻屏，不发给 shell。
                if state.modifiers.shift_key() {
                    let rows = state.tabs[ti].panes[pi].term.grid().rows() as isize;
                    let scrolled = match event.logical_key {
                        Key::Named(NamedKey::PageUp) => {
                            state.tabs[ti].panes[pi]
                                .term
                                .grid_mut()
                                .scroll_display(rows - 1);
                            true
                        }
                        Key::Named(NamedKey::PageDown) => {
                            state.tabs[ti].panes[pi]
                                .term
                                .grid_mut()
                                .scroll_display(-(rows - 1));
                            true
                        }
                        _ => false,
                    };
                    if scrolled {
                        state.window.request_redraw();
                        return;
                    }
                    // Shift+Insert 粘贴。
                    if matches!(event.logical_key, Key::Named(NamedKey::Insert)) {
                        state.tabs[ti].panes[pi].paste_clipboard(&mut state.clipboard);
                        return;
                    }
                }
                if state.modifiers.control_key() {
                    // Ctrl+↑/↓：命令块间跳转。备用屏幕（vim/codex）里
                    // 块不可见也无意义，按键放行给应用。
                    if !state.tabs[ti].panes[pi].term.is_alt_screen() {
                        match event.logical_key {
                            Key::Named(NamedKey::ArrowUp) => {
                                if state.tabs[ti].panes[pi].jump_block(-1) {
                                    state.window.request_redraw();
                                }
                                return;
                            }
                            Key::Named(NamedKey::ArrowDown) => {
                                if state.tabs[ti].panes[pi].jump_block(1) {
                                    state.window.request_redraw();
                                }
                                return;
                            }
                            _ => {}
                        }
                    }
                    let ch = match &event.logical_key {
                        Key::Character(s) => s.chars().next().map(|c| c.to_ascii_lowercase()),
                        _ => None,
                    };
                    match ch {
                        // Ctrl+C 优先级：文本选区复制 → 选中块复制输出 →
                        // 发送中断（Windows Terminal 惯例扩展）。
                        Some('c') => {
                            if state.tabs[ti].panes[pi].copy_selection(&mut state.clipboard) {
                                state.tabs[ti].panes[pi].selection = None;
                                state.window.request_redraw();
                                return;
                            }
                            // 块处于选中态（用户可见高亮）就必须消费按键：
                            // 复制失败（空输出/块已淘汰）也只清选中、绝不
                            // 下穿成中断——误发 ^C 会取消用户输入的命令行。
                            if state.tabs[ti].panes[pi].selected_block.is_some() {
                                state.tabs[ti].panes[pi].copy_selected_block(&mut state.clipboard);
                                state.tabs[ti].panes[pi].selected_block = None;
                                state.window.request_redraw();
                                return;
                            }
                            if state.modifiers.shift_key() {
                                return; // Ctrl+Shift+C 无选区时不下发
                            }
                        }
                        // Ctrl+V / Ctrl+Shift+V 粘贴。
                        Some('v') => {
                            state.tabs[ti].panes[pi].paste_clipboard(&mut state.clipboard);
                            return;
                        }
                        _ => {}
                    }
                }
                // win32-input-mode 实验性开关（LUMEN_WIN32_INPUT=1 启用）：
                // 实测当前编码实现反而更卡，默认关闭待核对协议规范。
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
                    if let Err(e) = state.tabs[ti].panes[pi].pty.write(&bytes) {
                        error!("写入 PTY 失败: {e:#}");
                    }
                    state.last_key_at = Some(write_t0);
                    state.perf_log(format_args!("key 写入耗时 {:?}", write_t0.elapsed()));
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                state.mouse_pos = (position.x, position.y);
                if state.focused_pane().selecting {
                    // 拖选跟随焦点窗格：端点按窗格矩形换算（cell_at 已
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
                    // 焦点仲裁（F5）：点击窗格聚焦该窗格 + 终端拿键盘/
                    // IME 焦点；点击 egui 面板交出焦点（路由随之切换）。
                    let Some(pi) = state.pane_under_mouse() else {
                        state.terminal_focused = false;
                        return;
                    };
                    state.terminal_focused = true;
                    state.focus_pane(pi);
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
                    // 右键：有选区则复制，否则粘贴（Windows Terminal 惯例）。
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
            WindowEvent::Ime(Ime::Commit(text)) => {
                // 仅终端聚焦时把 IME 提交文本写入 shell（焦点窗格）；
                // egui 输入框聚焦时事件已喂给 egui 消化，再写 PTY 就是
                // 双投。
                if !state.terminal_focused {
                    return;
                }
                // 与按键路径一致：输入即回滚到底部——翻看历史时提交
                // 中文，视图不跳回底部会看不到自己的回显。
                let s = state.focused_pane_mut();
                s.term.grid_mut().scroll_to_bottom();
                if let Err(e) = s.pty.write(text.as_bytes()) {
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
                    .map(|(i, p)| shell::PaneView {
                        tex: state.pane_textures.get(&p.id).copied(),
                        focused: i == tab.focused,
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
                let shell_input = shell::ShellInput {
                    panes: &panes_view,
                    tabs: &entries,
                    profile: state.profile.as_ref(),
                    cwd: active_cwd.as_deref(),
                    shell_idle,
                };
                let shell_state = &mut state.shell_state;
                let app_settings = &mut state.settings;
                let mut shell_out = None;
                let full_output = state.egui_ctx.run_ui(raw_input, |ui| {
                    shell_out = Some(shell::show(ui, &shell_input, shell_state, app_settings));
                });
                let Some(shell_out) = shell_out else {
                    return; // run_ui 必然执行闭包，防御分支
                };
                if shell_out.term_clicked {
                    state.terminal_focused = true;
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

                // —— 覆盖层（设置页/登录页）焦点路由：先处理关闭再处理
                // 打开——登录页关闭时设置页可能仍开着（Account 入口的
                // 叠层场景），后判打开保证焦点不被错误交还终端 ——
                if shell_out.settings_closed || shell_out.login_closed {
                    // 关闭后焦点交还终端（IME 强制复位链路每帧照旧执行）。
                    state.terminal_focused = true;
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
                    .then(|| format!("系统中未找到「{}」，已回退「{actual}」", ap.font_family));
                }
                if shell_out.settings_theme_changed {
                    // 主题即时生效：终端配色（含行排版缓存失效，行哈希
                    // 不含主题解析色）+ 外壳 egui 样式联动。
                    let theme = state.settings.appearance.theme;
                    state.renderer.set_theme(theme.terminal_theme());
                    shell::theme::apply_style(
                        &state.egui_ctx,
                        shell::theme::palette(theme.is_light()),
                    );
                }
                if shell_out.settings_font_changed || shell_out.settings_theme_changed {
                    // 变更即写盘（写临时文件后改名，防半写损坏）。失败
                    // 弹 toast：用户以为改完即存，静默丢失重启才发现。
                    if let Some(err) = state.settings.save() {
                        state.shell_state.toast.push(
                            shell::toast::ToastKind::Error,
                            format!("设置保存失败：{err}"),
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
                        format!("已登录：{}", p.display_name),
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
                    if let Err(e) = s.pty.write(&cmd) {
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
                if let Some((pi, path)) = shell_out.insert_path {
                    let bytes = shell::filetree::path_insert_text(&path);
                    // 下标对应 run_ui 时的布局；本帧结构若已被上方动作
                    // 改变（增删窗格）则跳过本次插入（防御，拖放与增删
                    // 同帧发生的概率可忽略）。
                    if !bytes.is_empty() && pi < state.tabs[state.active_tab].panes.len() {
                        state.focus_pane(pi);
                        let s = state.focused_pane_mut();
                        s.term.grid_mut().scroll_to_bottom();
                        if let Err(e) = s.pty.write(&bytes) {
                            error!("写入 PTY 失败: {e:#}");
                        }
                        // 插入后把键盘/IME 焦点交还终端，接着编辑命令行。
                        state.terminal_focused = true;
                    }
                }
                // —— 文件树右键菜单：复制绝对/相对路径到剪贴板 ——
                if let Some(text) = shell_out.copy_text {
                    let ok = matches!(
                        state.clipboard.as_mut().map(|c| c.set_text(text.clone())),
                        Some(Ok(()))
                    );
                    if ok {
                        state
                            .shell_state
                            .toast
                            .push(shell::toast::ToastKind::Info, format!("已复制：{text}"));
                    } else {
                        error!("写剪贴板失败（复制路径）");
                        state
                            .shell_state
                            .toast
                            .push(shell::toast::ToastKind::Error, "复制失败：剪贴板不可用");
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
                let structure_unchanged = state.tabs.get(state.active_tab).is_some_and(|t| {
                    t.panes.len() == layout_pane_ids.len()
                        && t.panes
                            .iter()
                            .zip(&layout_pane_ids)
                            .all(|(p, id)| p.id == *id)
                });
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
                    state.pane_rects_px.clear();
                    for (i, r) in shell_out.pane_rects.iter().enumerate() {
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
                        let (rows, cols) = state.renderer.grid_size_for(tw, th);
                        let s = &mut state.tabs[state.active_tab].panes[i];
                        let g = s.term.grid();
                        if (rows, cols) != (g.rows(), g.cols()) {
                            s.term.resize(rows, cols);
                            let _ = s.pty.resize(rows as u16, cols as u16);
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
                        if let Err(e) = state.renderer.render(
                            s.id,
                            &s.term,
                            s.selection.as_ref(),
                            s.cursor_displayed,
                            s.selected_block,
                        ) {
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
                    if let Some((px, py, _, _)) = state.focused_pane_rect_px() {
                        let s = state.focused_pane();
                        let g = s.term.grid();
                        let view_row = (g.display_offset() + s.cursor_displayed.0)
                            .min(g.rows().saturating_sub(1));
                        let (cx, cy) = state.renderer.cell_origin(view_row, s.cursor_displayed.1);
                        let (cw, ch) = state.renderer.cell_size();
                        state.window.set_ime_cursor_area(
                            winit::dpi::PhysicalPosition::new((px + cx) as f64, (py + cy) as f64),
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
    use super::drain_order;

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
}
