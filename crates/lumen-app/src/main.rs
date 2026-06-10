//! Lumen 主程序：winit 事件循环，组装 PTY → 终端状态机 → 渲染器 → egui 外壳。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod input;
mod session;
mod settings;
mod shell;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use log::{error, info};
use lumen_pty::PtyEvent;
use lumen_renderer::{wgpu, Renderer};
use lumen_term::{SelPoint, Selection};
use session::{Session, SessionId};
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
/// `yes` 级输出不限量会抢占主线程拖慢前台打字。超限的事件留在通道
/// 里（靠 bounded 容量反压读线程），并补发一个 wake 下轮继续消化。
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

struct AppState {
    /// 性能埋点输出（LUMEN_PERF=<路径> 启用）。
    perf: Option<std::fs::File>,
    perf_t0: Instant,
    last_render_at: Option<Instant>,
    window: Arc<Window>,
    renderer: Renderer,
    /// 全部会话（per-session 状态见 [`Session`]）。至少一个；最后
    /// 一个关闭即退出应用。
    sessions: Vec<Session>,
    /// 激活会话在 `sessions` 中的下标。
    active: usize,
    /// 会话 id 自增分配器（关闭不回收，残留事件按 id 丢弃）。
    next_session_id: SessionId,
    /// 全局 PTY 事件通道发送端（新建会话的转发线程汇入用）。
    pty_tx: Sender<(SessionId, PtyEvent)>,
    /// 全局 PTY 事件通道接收端（各会话转发线程汇入，元素带会话 id）。
    pty_rx: Receiver<(SessionId, PtyEvent)>,
    /// 后台会话超出单轮消化上限时滞留的事件（下个 wake 优先处理，
    /// 保持同会话事件顺序）。
    carry: Option<(SessionId, PtyEvent)>,
    /// 与转发线程共享的「wake 已挂起」标志，用于事件去重（全局一个，
    /// 唤醒协议与单会话时代零变化）。
    wake_pending: Arc<AtomicBool>,
    /// 事件循环唤醒句柄（补发 wake / 新建会话的转发线程用）。
    proxy: EventLoopProxy<PtyWake>,
    /// 应用设置（设置页编辑的数据源；变更即写盘）。
    settings: settings::Settings,
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
    /// 终端纹理的 egui 句柄（离屏重建后原地换绑，id 不变）。
    term_tex_id: egui::TextureId,
    /// 终端区矩形（物理像素 x/y/w/h），来自最近一帧 egui 布局。
    term_rect_px: (f32, f32, f32, f32),
    /// 终端是否持有键盘/IME 焦点：点击终端区 true、点击 egui 面板
    /// false。egui 不会为非控件区域持焦点，键盘与 IME 路由全靠它。
    terminal_focused: bool,
    /// egui 主动要求的下次重绘时刻（动画等），about_to_wait 里与
    /// 终端渲染计划合流取 min。
    egui_repaint_at: Option<Instant>,
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

    /// 鼠标当前位置是否落在终端区上（且未被 egui 弹层盖住）。
    ///
    /// 终端区鼠标交互（选区/块点击/滚轮）以此为闸，不依赖 egui 的
    /// consumed（CentralPanel 覆盖终端区，悬停即视为「在 egui 区域
    /// 上」，consumed 对鼠标无判别力）。右键菜单等弹层可能盖在终端
    /// 上：面板与 CentralPanel 同属 Background 层，弹层在更高层——
    /// 命中非背景层即视为「鼠标在 egui 弹层上」，交互归 egui。
    fn mouse_in_term(&self) -> bool {
        let (x, y, w, h) = self.term_rect_px;
        let (mx, my) = self.mouse_pos;
        let inside =
            mx >= x as f64 && my >= y as f64 && mx < (x + w) as f64 && my < (y + h) as f64;
        if !inside {
            return false;
        }
        let ppp = self.egui_ctx.pixels_per_point();
        let pos = egui::pos2(mx as f32 / ppp, my as f32 / ppp);
        self.egui_ctx
            .layer_id_at(pos)
            .is_none_or(|l| l.order == egui::Order::Background)
    }

    /// 把当前鼠标像素位置换算成选区端点（绝对行号，取激活会话网格）。
    /// cell_at 接相对终端区原点的坐标。
    fn sel_point_at_mouse(&self) -> SelPoint {
        let (row, col) = self.renderer.cell_at(
            self.mouse_pos.0 - self.term_rect_px.0 as f64,
            self.mouse_pos.1 - self.term_rect_px.1 as f64,
        );
        SelPoint {
            line: self.sessions[self.active].term.grid().view_top_abs_line() + row as u64,
            col,
        }
    }

    /// 切换激活会话：清掉目标会话的冻结计时与渲染计划（属于「上次
    /// 激活期间」的旧时间轴，带过来会借用过期的调度），清未读点，
    /// 同步窗口标题并立即重绘。切到的终端默认拿键盘/IME 焦点。
    fn activate(&mut self, idx: usize) {
        self.active = idx;
        let s = &mut self.sessions[idx];
        s.cursor_frozen_at = None;
        s.redraw_at = None;
        s.redraw_hard_at = None;
        s.redraw_abs_at = None;
        s.has_unseen_output = false;
        self.terminal_focused = true;
        self.update_window_title();
        self.window.request_redraw();
    }

    /// 关闭会话：从列表移除即随 `PtySession` Drop 杀掉子进程。
    /// 返回是否已无会话（调用方应退出应用）。
    fn close_session(&mut self, idx: usize) -> bool {
        let removed = self.sessions.remove(idx);
        info!("关闭会话 id={}", removed.id);
        // 丢弃属于该会话的滞留事件（通道里的残留靠 drain 时按 id 丢）。
        if self.carry.as_ref().is_some_and(|(sid, _)| *sid == removed.id) {
            self.carry = None;
        }
        drop(removed);
        if self.sessions.is_empty() {
            return true;
        }
        if idx < self.active {
            // 移除位在激活位之前：激活会话整体左移一位，无需切换。
            self.active -= 1;
        } else if idx == self.active {
            // 关闭激活 tab：切到邻位（右邻顶上原位；无右邻取末位）。
            self.activate(idx.min(self.sessions.len() - 1));
        }
        false
    }

    /// 新建会话（继承当前 shell 配置）并切换为激活。
    /// 行列数取当前终端区（所有会话同尺寸）。
    fn new_session(&mut self) {
        let g = self.sessions[self.active].term.grid();
        let (rows, cols) = (g.rows(), g.cols());
        let id = self.next_session_id;
        self.next_session_id += 1;
        match Session::spawn(
            id,
            rows,
            cols,
            SCROLLBACK,
            self.pty_tx.clone(),
            self.wake_pending.clone(),
            self.proxy.clone(),
        ) {
            Ok(s) => {
                self.sessions.push(s);
                self.activate(self.sessions.len() - 1);
            }
            Err(e) => error!("新建会话失败: {e:#}"),
        }
    }

    /// 循环切换激活会话：dir 为 1（下一个）或 -1（上一个）。
    fn cycle_session(&mut self, dir: isize) {
        let n = self.sessions.len() as isize;
        if n <= 1 {
            return;
        }
        let idx = (self.active as isize + dir).rem_euclid(n) as usize;
        self.activate(idx);
    }

    /// 窗口标题跟随激活会话（自定义名优先，无标题回退应用名）。
    fn update_window_title(&self) {
        let title = self.sessions[self.active].display_title();
        if title.is_empty() {
            self.window.set_title("Lumen");
        } else {
            self.window.set_title(&format!("Lumen — {title}"));
        }
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
        .then(|| format!("系统中未找到「{}」，已回退「{actual_family}」", ap.font_family));
        // 首次启动（无设置文件）落盘默认值，方便用户直接手改；
        // 文件存在但损坏时不在此覆盖（保留现场，变更时才写）。
        if settings::Settings::path().is_some_and(|p| !p.exists()) {
            app_settings.save();
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
        let mut egui_renderer = egui_wgpu::Renderer::new(
            renderer.device(),
            renderer.surface_format(),
            egui_wgpu::RendererOptions::default(),
        );

        // 终端区初值：窗口减去侧栏宽度（首帧 egui 布局后按实际矩形校正）。
        let sidebar_px = (shell::SIDEBAR_WIDTH * scale).round();
        let term_w = ((size.width as f32 - sidebar_px).max(1.0)) as u32;
        let term_h = size.height.max(1);
        renderer.ensure_offscreen(term_w, term_h);
        let term_tex_id = egui_renderer.register_native_texture(
            renderer.device(),
            renderer.offscreen_view(),
            wgpu::FilterMode::Nearest,
        );

        let (rows, cols) = renderer.grid_size_for(term_w, term_h);
        info!("终端尺寸: {rows} 行 x {cols} 列");

        // 全局 PTY 事件通道：各会话的转发线程汇入同一条（元素带会话
        // id），bounded 容量对高产出会话形成背压。
        let (pty_tx, pty_rx) = crossbeam_channel::bounded::<(SessionId, PtyEvent)>(256);
        let wake_pending = Arc::new(AtomicBool::new(false));
        let first = Session::spawn(
            0,
            rows,
            cols,
            SCROLLBACK,
            pty_tx.clone(),
            wake_pending.clone(),
            self.proxy.clone(),
        )?;

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
            window,
            renderer,
            sessions: vec![first],
            active: 0,
            next_session_id: 1,
            pty_tx,
            pty_rx,
            carry: None,
            wake_pending,
            proxy: self.proxy.clone(),
            settings: app_settings,
            modifiers: ModifiersState::default(),
            clipboard,
            last_key_at: None,
            mouse_pos: (0.0, 0.0),
            egui_ctx,
            egui_state,
            egui_renderer,
            term_tex_id,
            term_rect_px: (sidebar_px, 0.0, term_w as f32, term_h as f32),
            terminal_focused: true,
            egui_repaint_at: None,
            shell_state: shell::ShellState::default(),
        };
        state.shell_state.settings.font_hint = font_hint;
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
        if state.sessions.is_empty() {
            return; // 退出流程中（exit 后仍可能有滞后事件）
        }
        // 先清挂起标志再 drain：drain 期间新到的数据会触发下一个 wake，不丢。
        state.wake_pending.store(false, Ordering::Release);

        let drain_t0 = Instant::now();
        // 每会话本轮已消化字节数 / 是否有新数据（按 sessions 下标）。
        let mut consumed = vec![0usize; state.sessions.len()];
        let mut got_data = vec![false; state.sessions.len()];
        let mut exited: Vec<SessionId> = Vec::new();
        // 后台会话超限导致提前停止 drain（需补发 wake 续处理）。
        let mut backlog = false;
        // Receiver 克隆一份避免 drain 循环内长借用 state。
        let rx = state.pty_rx.clone();
        // 上轮滞留的事件优先处理（保持同会话事件顺序）。
        let mut pending = state.carry.take();
        loop {
            let (sid, ev) = match pending.take() {
                Some(x) => x,
                None => match rx.try_recv() {
                    Ok(x) => x,
                    Err(_) => break,
                },
            };
            // 已关闭会话的残留事件直接丢弃。
            let Some(idx) = state.sessions.iter().position(|s| s.id == sid) else {
                continue;
            };
            match ev {
                PtyEvent::Data(bytes) => {
                    if idx != state.active && consumed[idx] >= BG_DRAIN_CAP {
                        // 后台会话本轮额度用尽：事件滞留、停止 drain
                        // （通道 FIFO，不能越过它取后面的事件），剩余
                        // 留到补发的下一个 wake 再消化，前台打字不被
                        // yes 级后台输出抢占主线程。
                        state.carry = Some((sid, PtyEvent::Data(bytes)));
                        backlog = true;
                        break;
                    }
                    consumed[idx] += bytes.len();
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
                    state.sessions[idx].term.advance(&bytes);
                    got_data[idx] = true;
                }
                PtyEvent::Exited => exited.push(sid),
            }
        }

        // —— 每会话的批后处理：应答回写对所有会话照常执行（后台不回
        // 写 DSR/DA 会卡死对端程序）；渲染调度只对激活会话生效，后台
        // 只更新 ESU 标记并标未读点。
        let mut active_stats = None;
        for (idx, s) in state.sessions.iter_mut().enumerate() {
            if !got_data[idx] {
                continue;
            }
            let is_active = idx == state.active;
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

            if !is_active {
                s.has_unseen_output = true;
                continue;
            }
            active_stats = Some((sync, frame_completed, s.term.cursor_unsettled()));
            if frame_completed {
                // 本批完成了 DEC 2026 同步帧：协议语义就是「立即原子
                // 呈现」，零等待直接渲染（codex 打字回显走这条快路）。
                // 但渲染频率以 ~8ms 为下限：极速输入（百帧每秒级回显）
                // 时把积压帧合并，避免渲染请求超出显示能力拖垮主线程。
                let now = Instant::now();
                let recent = state
                    .last_render_at
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
                    state.window.request_redraw();
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
        // 窗口标题跟随激活会话（OSC 标题可能随本批数据更新）。
        if active_stats.is_some() {
            state.update_window_title();
        }
        let total: usize = consumed.iter().sum();
        if total > 0 {
            let (sync, fc, unsettled) = active_stats.unwrap_or_default();
            state.perf_log(format_args!(
                "drain {total}B 耗时 {:?} sync={sync} esu帧={fc} unsettled={unsettled} 后台积压={backlog}",
                drain_t0.elapsed()
            ));
        }

        // —— 生命周期：shell 退出关闭对应 tab，最后一个 tab 关闭才退出。
        for sid in exited {
            let Some(idx) = state.sessions.iter().position(|s| s.id == sid) else {
                continue;
            };
            info!("会话 id={sid} 的 shell 已退出，关闭对应 tab");
            if state.close_session(idx) {
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
        if state.sessions.is_empty() {
            return; // 退出流程中
        }
        // 渲染调度只看激活会话的计划（后台会话不设计划、不打扰渲染）。
        let s = &mut state.sessions[state.active];
        // 终端渲染时刻 = 静默窗口与强制刷新中先到者；egui 重绘计划
        // （动画等）独立成项，与终端计划取 min 定下次唤醒。
        let term_due = s
            .redraw_at
            .map(|soft| s.redraw_hard_at.map_or(soft, |h| soft.min(h)));
        let due = match (term_due, state.egui_repaint_at) {
            // 没有任何待渲染计划时必须显式回到 Wait：ControlFlow 是粘性的，
            // 残留的 WaitUntil(过去时刻) 会让事件循环全速空转（曾导致
            // ESU 直渲后单核拉满、键盘处理抖动、conhost 被抢 CPU）。
            (None, None) => {
                event_loop.set_control_flow(ControlFlow::Wait);
                return;
            }
            (Some(t), None) => t,
            (None, Some(e)) => e,
            (Some(t), Some(e)) => t.min(e),
        };
        let now = Instant::now();
        if now < due {
            event_loop.set_control_flow(ControlFlow::WaitUntil(due));
            return;
        }
        // 终端计划到点但正处于同步区间：小步顺延等帧完成（ESU 通常随
        // 下一批数据立刻到达），但不超过绝对兜底时刻。egui 计划即使
        // 到点也跟着顺延（2ms 粒度，对 UI 动画无感），避免把半成品
        // 终端帧画上屏。
        if term_due.is_some_and(|t| now >= t)
            && s.term.is_synchronized()
            && s.redraw_abs_at.is_some_and(|a| now < a)
        {
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_millis(2)));
            return;
        }
        // 只清掉已到点的计划：egui 提前到点不应连带提前终端的静默合
        // 帧计划（半成品 TUI 帧会闪烁），反之亦然。
        if term_due.is_some_and(|t| now >= t) {
            s.redraw_at = None;
            s.redraw_hard_at = None;
            s.redraw_abs_at = None;
        }
        if state.egui_repaint_at.is_some_and(|e| now >= e) {
            state.egui_repaint_at = None;
        }
        event_loop.set_control_flow(ControlFlow::Wait);
        state.window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.sessions.is_empty() {
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
                state.window.request_redraw();
            }
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(mods) => state.modifiers = mods.state(),
            WindowEvent::Resized(size) => {
                state.renderer.resize_surface(size.width, size.height);
                // 终端行列数跟随 egui 布局出的终端区矩形，统一在
                // RedrawRequested 里检测变化并 resize（离屏纹理同步重建）。
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
                if pressed
                    && state.modifiers.control_key()
                    && !state.sessions[state.active].term.is_alt_screen()
                    && state.shell_state.renaming.is_none()
                {
                    let shift = state.modifiers.shift_key();
                    match &event.logical_key {
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
                            state.new_session();
                            return;
                        }
                        Key::Character(c) if !shift && c.eq_ignore_ascii_case("w") => {
                            if state.close_session(state.active) {
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
                            state.cycle_session(if shift { -1 } else { 1 });
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
                let active = state.active;
                // 抬起事件仅在 win32-input-mode 下投递（协议需要 Kd=0）。
                if !pressed {
                    if state.sessions[active].term.win32_input()
                        && std::env::var_os("LUMEN_WIN32_INPUT").is_some()
                    {
                        if let Some(bytes) =
                            input::encode_key_win32(&event, state.modifiers, false)
                        {
                            if let Err(e) = state.sessions[active].pty.write(&bytes) {
                                error!("写入 PTY 失败: {e:#}");
                            }
                        }
                    }
                    return;
                }
                // Shift+PgUp/PgDn 本地翻屏，不发给 shell。
                if state.modifiers.shift_key() {
                    let rows = state.sessions[active].term.grid().rows() as isize;
                    let scrolled = match event.logical_key {
                        Key::Named(NamedKey::PageUp) => {
                            state.sessions[active].term.grid_mut().scroll_display(rows - 1);
                            true
                        }
                        Key::Named(NamedKey::PageDown) => {
                            state.sessions[active]
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
                        state.sessions[active].paste_clipboard(&mut state.clipboard);
                        return;
                    }
                }
                if state.modifiers.control_key() {
                    // Ctrl+↑/↓：命令块间跳转。备用屏幕（vim/codex）里
                    // 块不可见也无意义，按键放行给应用。
                    if !state.sessions[active].term.is_alt_screen() {
                        match event.logical_key {
                            Key::Named(NamedKey::ArrowUp) => {
                                if state.sessions[active].jump_block(-1) {
                                    state.window.request_redraw();
                                }
                                return;
                            }
                            Key::Named(NamedKey::ArrowDown) => {
                                if state.sessions[active].jump_block(1) {
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
                            if state.sessions[active].copy_selection(&mut state.clipboard) {
                                state.sessions[active].selection = None;
                                state.window.request_redraw();
                                return;
                            }
                            // 块处于选中态（用户可见高亮）就必须消费按键：
                            // 复制失败（空输出/块已淘汰）也只清选中、绝不
                            // 下穿成中断——误发 ^C 会取消用户输入的命令行。
                            if state.sessions[active].selected_block.is_some() {
                                state.sessions[active]
                                    .copy_selected_block(&mut state.clipboard);
                                state.sessions[active].selected_block = None;
                                state.window.request_redraw();
                                return;
                            }
                            if state.modifiers.shift_key() {
                                return; // Ctrl+Shift+C 无选区时不下发
                            }
                        }
                        // Ctrl+V / Ctrl+Shift+V 粘贴。
                        Some('v') => {
                            state.sessions[active].paste_clipboard(&mut state.clipboard);
                            return;
                        }
                        _ => {}
                    }
                }
                // win32-input-mode 实验性开关（LUMEN_WIN32_INPUT=1 启用）：
                // 实测当前编码实现反而更卡，默认关闭待核对协议规范。
                let use_win32 = state.sessions[active].term.win32_input()
                    && std::env::var_os("LUMEN_WIN32_INPUT").is_some();
                let bytes = if use_win32 {
                    input::encode_key_win32(&event, state.modifiers, true)
                } else {
                    input::encode_key(&event, state.modifiers)
                };
                if let Some(bytes) = bytes {
                    state.sessions[active].term.grid_mut().scroll_to_bottom();
                    let write_t0 = Instant::now();
                    if let Err(e) = state.sessions[active].pty.write(&bytes) {
                        error!("写入 PTY 失败: {e:#}");
                    }
                    state.last_key_at = Some(write_t0);
                    state.perf_log(format_args!(
                        "key 写入耗时 {:?}",
                        write_t0.elapsed()
                    ));
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                state.mouse_pos = (position.x, position.y);
                if state.sessions[state.active].selecting {
                    let head = state.sel_point_at_mouse();
                    let active = state.active;
                    if let Some(sel) = state.sessions[active].selection.as_mut() {
                        if sel.head != head {
                            sel.head = head;
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
                    // 焦点仲裁：点击终端区聚焦终端，点击 egui 面板交出
                    // 焦点（键盘/IME 路由随之切换）。
                    if !state.mouse_in_term() {
                        state.terminal_focused = false;
                        return;
                    }
                    state.terminal_focused = true;
                    let p = state.sel_point_at_mouse();
                    let s = &mut state.sessions[state.active];
                    s.selecting = true;
                    // 单击先建立空选区（不高亮），拖动后才有内容。
                    s.selection = Some(Selection { anchor: p, head: p });
                    state.window.request_redraw();
                }
                (MouseButton::Left, ElementState::Released) => {
                    // 本次按下不在终端区（点的是 egui 面板）则与终端无关。
                    if !state.sessions[state.active].selecting {
                        return;
                    }
                    state.sessions[state.active].selecting = false;
                    if state.sessions[state.active]
                        .selection
                        .is_some_and(|s| s.is_empty())
                    {
                        // 单击（未拖动）：选中/清除所在命令块。
                        // 备用屏幕下块行号坐标系不可用，不做块选中。
                        let p = state.sel_point_at_mouse();
                        let s = &mut state.sessions[state.active];
                        s.selection = None;
                        if !s.term.is_alt_screen() {
                            let hit = s.term.block_at_line(p.line).map(|b| b.id);
                            s.selected_block = if hit == s.selected_block { None } else { hit };
                        }
                        state.window.request_redraw();
                    }
                }
                (MouseButton::Right, ElementState::Pressed) => {
                    if !state.mouse_in_term() {
                        return;
                    }
                    // 右键：有选区则复制，否则粘贴（Windows Terminal 惯例）。
                    let active = state.active;
                    if state.sessions[active].copy_selection(&mut state.clipboard) {
                        state.sessions[active].selection = None;
                        state.window.request_redraw();
                    } else {
                        state.sessions[active].paste_clipboard(&mut state.clipboard);
                    }
                }
                _ => {}
            },
            WindowEvent::Ime(Ime::Commit(text)) => {
                // 仅终端聚焦时把 IME 提交文本写入 shell；egui 输入框
                // 聚焦时事件已喂给 egui 消化，再写 PTY 就是双投。
                if !state.terminal_focused {
                    return;
                }
                if let Err(e) = state.sessions[state.active].pty.write(text.as_bytes()) {
                    error!("写入 PTY 失败: {e:#}");
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // 终端区内滚轮归终端，区外（侧栏等）归 egui。
                if !state.mouse_in_term() {
                    return;
                }
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y * 3.0) as isize,
                    MouseScrollDelta::PixelDelta(p) => {
                        (p.y / state.renderer.cell_size().1 as f64) as isize
                    }
                };
                if lines != 0 {
                    state.sessions[state.active]
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

                {
                    let s = &mut state.sessions[state.active];
                    s.term.grid_mut().take_dirty();
                    // 光标跟随策略：正常情况下零延迟跟随终端光标；处于
                    // 「帧尾未归位」窗口（ESU 后还没重新显示光标）时冻结
                    // 旧位置，等归位序列或超时，避免画出重绘残留位。
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

                // —— egui 帧：跑 UI 布局，产出本帧终端区矩形 ——
                let raw_input = state.egui_state.take_egui_input(&state.window);
                let entries: Vec<shell::SessionEntry> = state
                    .sessions
                    .iter()
                    .enumerate()
                    .map(|(i, s)| shell::SessionEntry {
                        id: s.id,
                        title: {
                            let t = s.display_title();
                            if t.is_empty() {
                                "PowerShell".to_owned()
                            } else {
                                t.to_owned()
                            }
                        },
                        active: i == state.active,
                        unseen: s.has_unseen_output,
                    })
                    .collect();
                let tex_id = state.term_tex_id;
                let was_renaming = state.shell_state.renaming.is_some();
                // 文件树输入：激活会话的 cwd（OSC 9;9 上报）与空闲态
                // （cd 注入闸门，见 Terminal::shell_waiting_input）。
                let active_cwd = state.sessions[state.active]
                    .term
                    .cwd()
                    .map(std::path::Path::to_path_buf);
                let shell_idle = state.sessions[state.active].term.shell_waiting_input();
                let shell_state = &mut state.shell_state;
                let app_settings = &mut state.settings;
                let mut shell_out = None;
                let full_output = state.egui_ctx.run_ui(raw_input, |ui| {
                    shell_out = Some(shell::show(
                        ui,
                        tex_id,
                        &entries,
                        shell_state,
                        app_settings,
                        active_cwd.as_deref(),
                        shell_idle,
                    ));
                });
                let Some(shell_out) = shell_out else {
                    return; // run_ui 必然执行闭包，防御分支
                };
                if shell_out.term_clicked {
                    state.terminal_focused = true;
                }
                // 重命名编辑期间键盘/IME 归 egui 的输入框（右键打开
                // 菜单不经过左键焦点仲裁，必须在此强制让出）；编辑
                // 结束把焦点还给终端。
                if state.shell_state.renaming.is_some() {
                    state.terminal_focused = false;
                } else if was_renaming {
                    state.terminal_focused = true;
                }

                // —— 侧栏动作：切换 / 重命名 / 新建 / 关闭 ——
                if let Some(id) = shell_out.activate {
                    if let Some(idx) = state.sessions.iter().position(|s| s.id == id) {
                        if idx != state.active {
                            state.activate(idx);
                        }
                    }
                }
                if let Some((id, name)) = shell_out.rename {
                    if let Some(s) = state.sessions.iter_mut().find(|s| s.id == id) {
                        // 空名 = 清除自定义名，恢复跟随终端 OSC 标题。
                        s.custom_title = (!name.is_empty()).then_some(name);
                    }
                    state.update_window_title();
                }
                if shell_out.new_session {
                    state.new_session();
                }
                if let Some(id) = shell_out.close {
                    if let Some(idx) = state.sessions.iter().position(|s| s.id == id) {
                        if state.close_session(idx) {
                            info!("最后一个会话已关闭，退出应用");
                            event_loop.exit();
                            return; // 不再呈现本帧（应用退出中）
                        }
                    }
                }

                // —— 设置页动作：焦点路由 + 外观变更即时生效 + 写盘 ——
                if shell_out.settings_closed {
                    // 关闭后焦点交还终端（IME 强制复位链路每帧照旧执行）。
                    state.terminal_focused = true;
                }
                if shell_out.settings_opened || state.shell_state.settings.open {
                    // 打开期间键盘/IME 恒归 egui（设置页是覆盖层，
                    // 终端的 PTY 消化与渲染照常进行，只是不收键盘）。
                    state.terminal_focused = false;
                }
                if shell_out.settings_font_changed {
                    // 字体/字号即时生效：重建字体度量（行排版缓存随之
                    // 失效）；cell 尺寸变化引发的行列数重算与全会话
                    // resize 在下方矩形检查统一处理（同一帧内完成）。
                    let ap = &state.settings.appearance;
                    let actual = state.renderer.reconfigure_font(&ap.font_family, ap.font_size);
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
                    // 变更即写盘（写临时文件后改名，防半写损坏）。
                    state.settings.save();
                }

                // —— 文件树动作：双击目录 cd / 双击文件系统默认程序打开 ——
                if let Some(dir) = shell_out.cd_dir {
                    // UI 已按 shell 空闲闸门过滤，这里直接注入。
                    let cmd = shell::filetree::cd_command(&dir);
                    let s = &mut state.sessions[state.active];
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

                // —— 终端区矩形（物理像素）变化 → 重建离屏 + resize ——
                let ppp = full_output.pixels_per_point;
                let r = shell_out.term_rect;
                state.term_rect_px = (
                    r.min.x * ppp,
                    r.min.y * ppp,
                    r.width() * ppp,
                    r.height() * ppp,
                );
                let tw = (r.width() * ppp).round().max(1.0) as u32;
                let th = (r.height() * ppp).round().max(1.0) as u32;
                if state.renderer.ensure_offscreen(tw, th) {
                    // 原地换绑：TextureId 不变，本帧 egui pass 即采样新视图。
                    state.egui_renderer.update_egui_texture_from_wgpu_texture(
                        state.renderer.device(),
                        state.renderer.offscreen_view(),
                        wgpu::FilterMode::Nearest,
                        state.term_tex_id,
                    );
                }
                // 行列数同时受终端区矩形与 cell 尺寸（设置页字体/字号）
                // 影响，每帧对照激活会话网格检测（廉价的整数比较）。
                // 所有 tab 共享同一终端视口矩形：resize 对全部会话立即
                // 生效（懒 resize 会让后台 TUI 在切换瞬间花屏），新建
                // 会话也以此行列数初始化。设置页改字号即时生效走的就是
                // 这条链路：cell 尺寸变 → 行列数变 → term/pty resize。
                let (rows, cols) = state.renderer.grid_size_for(tw, th);
                let g = state.sessions[state.active].term.grid();
                if (rows, cols) != (g.rows(), g.cols()) {
                    for s in &mut state.sessions {
                        s.term.resize(rows, cols);
                        let _ = s.pty.resize(rows as u16, cols as u16);
                        // 尺寸变化会夹紧光标位置，立即同步绘制态。
                        let g = s.term.grid();
                        s.cursor_displayed = (g.cursor.row, g.cursor.col, g.cursor.visible);
                    }
                }

                // —— 终端管线渲染到离屏纹理（damage/行缓存机制原样）——
                let s = &state.sessions[state.active];
                let cursor = s
                    .cursor_displayed
                    .2
                    .then_some((s.cursor_displayed.0, s.cursor_displayed.1));
                if let Err(e) = state.renderer.render(
                    &s.term,
                    s.selection.as_ref(),
                    cursor,
                    s.selected_block,
                ) {
                    error!("渲染失败: {e:#}");
                }

                // —— egui 平台输出 + IME 强制复位（IME 最大坑对策）——
                // egui 会按自己的文本焦点开关整窗 IME / 挪动候选框；终端
                // 聚焦时必须在 handle_platform_output **之后**强制复位，
                // 并把候选框钉在终端光标所在格子（终端区原点 + cell×行列）。
                state
                    .egui_state
                    .handle_platform_output(&state.window, full_output.platform_output);
                if state.terminal_focused {
                    state.window.set_ime_allowed(true);
                    let s = &state.sessions[state.active];
                    let g = s.term.grid();
                    let view_row = (g.display_offset() + s.cursor_displayed.0)
                        .min(g.rows().saturating_sub(1));
                    let (cx, cy) = state
                        .renderer
                        .cell_origin(view_row, s.cursor_displayed.1);
                    let (cw, ch) = state.renderer.cell_size();
                    state.window.set_ime_cursor_area(
                        winit::dpi::PhysicalPosition::new(
                            (state.term_rect_px.0 + cx) as f64,
                            (state.term_rect_px.1 + cy) as f64,
                        ),
                        winit::dpi::PhysicalSize::new(cw as f64, ch as f64),
                    );
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
                    // 动画进行中要求立即重绘；request_redraw 自带合并。
                    state.window.request_redraw();
                    None
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
                state.perf_log(format_args!(
                    "render 耗时 {:?} 距上帧 {gap:?}{key_to_screen}",
                    render_t0.elapsed()
                ));
            }
            _ => {}
        }
    }
}
