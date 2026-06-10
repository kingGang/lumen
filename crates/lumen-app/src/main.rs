//! Lumen 主程序：winit 事件循环，组装 PTY → 终端状态机 → 渲染器 → egui 外壳。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod input;
mod shell;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use log::{error, info};
use lumen_pty::{PtyEvent, PtySession};
use lumen_renderer::{wgpu, Renderer};
use lumen_term::{SelPoint, Selection, Terminal};
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

/// 把 shell integration 脚本写到临时目录，返回注入用的启动参数。
/// 写入失败时返回空参数（shell 照常可用，只是没有命令块标记）。
fn shell_integration_args() -> Vec<String> {
    let script = include_str!("../assets/integration.ps1");
    let path = std::env::temp_dir().join("lumen_integration.ps1");
    match std::fs::write(&path, script) {
        Ok(()) => vec![
            "-NoLogo".into(),
            // 进程级放行：Windows PowerShell 5.1 默认 Restricted 策略
            // 会拒绝加载任何 .ps1，注入会在终端顶部报红错。
            "-ExecutionPolicy".into(),
            "Bypass".into(),
            "-NoExit".into(),
            "-Command".into(),
            format!(". '{}'", path.display()),
        ],
        Err(e) => {
            error!("写出 shell integration 脚本失败: {e}");
            Vec::new()
        }
    }
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
    term: Terminal,
    pty: PtySession,
    pty_rx: Receiver<PtyEvent>,
    /// 与转发线程共享的「wake 已挂起」标志，用于事件去重。
    wake_pending: Arc<AtomicBool>,
    modifiers: ModifiersState,
    clipboard: Option<arboard::Clipboard>,
    /// 静默渲染时刻：最后一批数据到达时间 + 静默窗口（每批后推）。
    redraw_at: Option<Instant>,
    /// 强制渲染时刻：首批未渲染数据 + 硬上限（保障最低刷新率）。
    redraw_hard_at: Option<Instant>,
    /// 绝对兜底时刻：超过后即使在同步区间内也渲染。
    redraw_abs_at: Option<Instant>,
    /// 上次处理批的 ESU 标记，用于检测「本批完成了同步帧」。
    last_esu_mark: u64,
    /// 最近一次按键时刻（端到端延迟埋点用）。
    last_key_at: Option<Instant>,
    /// 实际绘制中的光标态 (行, 列, 可见)。光标处于「帧尾未归位」
    /// 状态（见 Terminal::cursor_unsettled）时冻结不跟随。
    cursor_displayed: (usize, usize, bool),
    /// 光标冻结起点（超时后强制信任当前位置）。
    cursor_frozen_at: Option<Instant>,
    /// 鼠标最近一次的窗口内像素位置。
    mouse_pos: (f64, f64),
    /// 左键按住拖选中。
    selecting: bool,
    selection: Option<Selection>,
    /// 选中的命令块 id。
    selected_block: Option<u64>,

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

    /// 鼠标当前位置是否落在终端区矩形内。
    ///
    /// M3.1 终端区鼠标交互（选区/块点击/滚轮）以此为闸，不依赖 egui
    /// 的 consumed（CentralPanel 覆盖终端区，悬停即视为「在 egui 区域
    /// 上」，consumed 对鼠标无判别力）。M3.2+ 出现盖在终端上的弹层
    /// 时需在此叠加 egui 层命中检测。
    fn mouse_in_term(&self) -> bool {
        let (x, y, w, h) = self.term_rect_px;
        let (mx, my) = self.mouse_pos;
        mx >= x as f64 && my >= y as f64 && mx < (x + w) as f64 && my < (y + h) as f64
    }

    /// 把当前鼠标像素位置换算成选区端点（绝对行号）。
    /// cell_at 接相对终端区原点的坐标。
    fn sel_point_at_mouse(&self) -> SelPoint {
        let (row, col) = self.renderer.cell_at(
            self.mouse_pos.0 - self.term_rect_px.0 as f64,
            self.mouse_pos.1 - self.term_rect_px.1 as f64,
        );
        SelPoint {
            line: self.term.grid().view_top_abs_line() + row as u64,
            col,
        }
    }

    /// 复制选中命令块的输出到剪贴板，返回是否复制了内容。
    fn copy_selected_block(&mut self) -> bool {
        let Some(id) = self.selected_block else {
            return false;
        };
        let Some(block) = self.term.block_by_id(id) else {
            return false;
        };
        let text = self.term.block_output_text(block);
        if text.is_empty() {
            return false;
        }
        match self.clipboard.as_mut().map(|c| c.set_text(text)) {
            Some(Ok(())) => true,
            other => {
                if let Some(Err(e)) = other {
                    error!("写剪贴板失败: {e}");
                }
                false
            }
        }
    }

    /// 块间跳转：dir 为 -1（上一块）或 1（下一块），滚动到块首。
    fn jump_block(&mut self, dir: i64) {
        // 选中的块可能已被上限淘汰：先清掉失效 id，按未选中逻辑走。
        if self
            .selected_block
            .is_some_and(|id| self.term.block_by_id(id).is_none())
        {
            self.selected_block = None;
        }
        let blocks = self.term.blocks();
        if blocks.is_empty() {
            return;
        }
        let idx = match self
            .selected_block
            .and_then(|id| blocks.iter().position(|b| b.id == id))
        {
            Some(i) => (i as i64 + dir).clamp(0, blocks.len() as i64 - 1) as usize,
            // 未选中时：↑ 从最后一块开始，↓ 选最后一块。
            None => {
                if dir < 0 {
                    blocks.len().saturating_sub(2)
                } else {
                    blocks.len() - 1
                }
            }
        };
        let (id, line) = (blocks[idx].id, blocks[idx].prompt_line);
        self.selected_block = Some(id);
        self.term.grid_mut().scroll_to_abs_line(line);
        self.window.request_redraw();
    }

    /// 复制选区文本到剪贴板，返回是否真的复制了内容。
    fn copy_selection(&mut self) -> bool {
        let Some(sel) = self.selection.filter(|s| !s.is_empty()) else {
            return false;
        };
        let text = self.term.selection_text(&sel);
        if text.is_empty() {
            return false;
        }
        match self.clipboard.as_mut().map(|c| c.set_text(text)) {
            Some(Ok(())) => true,
            other => {
                if let Some(Err(e)) = other {
                    error!("写剪贴板失败: {e}");
                }
                false
            }
        }
    }

    /// 粘贴剪贴板文本：换行规整为 CR，按需包 bracketed paste 标记。
    fn paste_clipboard(&mut self) {
        let Some(Ok(text)) = self.clipboard.as_mut().map(|c| c.get_text()) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        let payload = if self.term.bracketed_paste() {
            let mut p = Vec::with_capacity(normalized.len() + 12);
            p.extend_from_slice(b"\x1b[200~");
            p.extend_from_slice(normalized.as_bytes());
            p.extend_from_slice(b"\x1b[201~");
            p
        } else {
            normalized.into_bytes()
        };
        self.term.grid_mut().scroll_to_bottom();
        if let Err(e) = self.pty.write(&payload) {
            error!("粘贴写入 PTY 失败: {e:#}");
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

        // —— egui 三件套 ——
        let egui_ctx = egui::Context::default();
        shell::theme::apply_style(&egui_ctx);
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

        let term = Terminal::new(rows, cols, SCROLLBACK);
        let (pty, rx) = PtySession::spawn(
            None,
            &shell_integration_args(),
            rows as u16,
            cols as u16,
        )?;

        // 转发线程：把事件搬进主循环可 drain 的通道，并以去重信号唤醒
        // 事件循环（信号挂起期间不重复发，避免事件风暴）。
        let (tx2, rx2) = crossbeam_channel::bounded::<PtyEvent>(256);
        let wake_pending = Arc::new(AtomicBool::new(false));
        let proxy = self.proxy.clone();
        let pending = wake_pending.clone();
        std::thread::Builder::new()
            .name("lumen-pty-forward".into())
            .spawn(move || {
                for ev in rx {
                    if tx2.send(ev).is_err() {
                        break;
                    }
                    if !pending.swap(true, Ordering::AcqRel)
                        && proxy.send_event(PtyWake).is_err()
                    {
                        break;
                    }
                }
            })
            .context("启动 PTY 转发线程失败")?;

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

        Ok(AppState {
            perf,
            perf_t0: Instant::now(),
            last_render_at: None,
            window,
            renderer,
            term,
            pty,
            pty_rx: rx2,
            wake_pending,
            modifiers: ModifiersState::default(),
            clipboard,
            redraw_at: None,
            redraw_hard_at: None,
            redraw_abs_at: None,
            last_esu_mark: 0,
            last_key_at: None,
            cursor_displayed: (0, 0, true),
            cursor_frozen_at: None,
            mouse_pos: (0.0, 0.0),
            selecting: false,
            selection: None,
            selected_block: None,
            egui_ctx,
            egui_state,
            egui_renderer,
            term_tex_id,
            term_rect_px: (sidebar_px, 0.0, term_w as f32, term_h as f32),
            terminal_focused: true,
            egui_repaint_at: None,
        })
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
        // 先清挂起标志再 drain：drain 期间新到的数据会触发下一个 wake，不丢。
        state.wake_pending.store(false, Ordering::Release);

        let drain_t0 = Instant::now();
        let mut drained_bytes = 0usize;
        let mut got_data = false;
        let mut exited = false;
        while let Ok(ev) = state.pty_rx.try_recv() {
            match ev {
                PtyEvent::Data(bytes) => {
                    drained_bytes += bytes.len();
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
                    state.term.advance(&bytes);
                    got_data = true;
                }
                PtyEvent::Exited => exited = true,
            }
        }

        if got_data {
            // 终端应答（DSR/DA/DECRQM 等）回写给 shell。
            let resp = state.term.take_responses();
            if !resp.is_empty() {
                let _ = state.pty.write(&resp);
            }
            // 进入备用屏幕（vim/codex 全屏）时块交互无意义且不可见，
            // 清掉选中态，避免 Ctrl+C 被残留选中块吞成「复制」。
            if state.term.is_alt_screen() && state.selected_block.is_some() {
                state.selected_block = None;
            }
            if !state.term.title().is_empty() {
                state
                    .window
                    .set_title(&format!("Lumen — {}", state.term.title()));
            }
            let sync = state.term.is_synchronized();
            let esu_mark = state.term.esu_mark();
            let frame_completed = esu_mark != state.last_esu_mark && !sync;
            state.last_esu_mark = esu_mark;

            if frame_completed {
                // 本批完成了 DEC 2026 同步帧：协议语义就是「立即原子
                // 呈现」，零等待直接渲染（codex 打字回显走这条快路）。
                // 但渲染频率以 ~8ms 为下限：极速输入（百帧每秒级回显）
                // 时把积压帧合并，避免渲染请求超出显示能力拖垮主线程。
                let now = Instant::now();
                let recent = state
                    .last_render_at
                    .is_some_and(|t| now.duration_since(t) < Duration::from_millis(8));
                if recent {
                    let at = state.last_render_at.unwrap() + Duration::from_millis(8);
                    state.redraw_at = Some(at);
                    state.redraw_hard_at = None;
                    state.redraw_abs_at = Some(at + Duration::from_millis(50));
                } else {
                    state.redraw_at = None;
                    state.redraw_hard_at = None;
                    state.redraw_abs_at = None;
                    state.window.request_redraw();
                }
            } else {
                // 无同步协议的流（普通 shell/claude）：静默合帧，每批
                // 数据推后渲染时刻，流停了才画（见 about_to_wait）；
                // 硬上限自首批起算，保障刷新率。
                let now = Instant::now();
                state.redraw_at = Some(now + REDRAW_DEBOUNCE);
                if state.redraw_hard_at.is_none() {
                    state.redraw_hard_at = Some(now + REDRAW_HARD_CAP);
                    state.redraw_abs_at = Some(now + REDRAW_ABS_CAP);
                }
            }
            let unsettled = state.term.cursor_unsettled();
            state.perf_log(format_args!(
                "drain {drained_bytes}B 耗时 {:?} sync={sync} esu帧={frame_completed} unsettled={unsettled}",
                drain_t0.elapsed()
            ));
        }
        if exited {
            info!("shell 已退出，关闭窗口");
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        // 终端渲染时刻 = 静默窗口与强制刷新中先到者；egui 重绘计划
        // （动画等）独立成项，与终端计划取 min 定下次唤醒。
        let term_due = state
            .redraw_at
            .map(|soft| state.redraw_hard_at.map_or(soft, |h| soft.min(h)));
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
            && state.term.is_synchronized()
            && state.redraw_abs_at.is_some_and(|a| now < a)
        {
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_millis(2)));
            return;
        }
        // 只清掉已到点的计划：egui 提前到点不应连带提前终端的静默合
        // 帧计划（半成品 TUI 帧会闪烁），反之亦然。
        if term_due.is_some_and(|t| now >= t) {
            state.redraw_at = None;
            state.redraw_hard_at = None;
            state.redraw_abs_at = None;
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
                // 终端聚焦时键盘绕过 egui 直通此处；非聚焦时按键归
                // egui，无论它是否消费都不再写 PTY（点了侧栏还往
                // shell 灌字节是事故）。
                if !state.terminal_focused {
                    return;
                }
                use winit::keyboard::{Key, NamedKey};
                let pressed = event.state == ElementState::Pressed;
                // 抬起事件仅在 win32-input-mode 下投递（协议需要 Kd=0）。
                if !pressed {
                    if state.term.win32_input()
                        && std::env::var_os("LUMEN_WIN32_INPUT").is_some()
                    {
                        if let Some(bytes) =
                            input::encode_key_win32(&event, state.modifiers, false)
                        {
                            if let Err(e) = state.pty.write(&bytes) {
                                error!("写入 PTY 失败: {e:#}");
                            }
                        }
                    }
                    return;
                }
                // Shift+PgUp/PgDn 本地翻屏，不发给 shell。
                if state.modifiers.shift_key() {
                    let rows = state.term.grid().rows() as isize;
                    let scrolled = match event.logical_key {
                        Key::Named(NamedKey::PageUp) => {
                            state.term.grid_mut().scroll_display(rows - 1);
                            true
                        }
                        Key::Named(NamedKey::PageDown) => {
                            state.term.grid_mut().scroll_display(-(rows - 1));
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
                        state.paste_clipboard();
                        return;
                    }
                }
                if state.modifiers.control_key() {
                    // Ctrl+↑/↓：命令块间跳转。备用屏幕（vim/codex）里
                    // 块不可见也无意义，按键放行给应用。
                    if !state.term.is_alt_screen() {
                        match event.logical_key {
                            Key::Named(NamedKey::ArrowUp) => {
                                state.jump_block(-1);
                                return;
                            }
                            Key::Named(NamedKey::ArrowDown) => {
                                state.jump_block(1);
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
                            if state.copy_selection() {
                                state.selection = None;
                                state.window.request_redraw();
                                return;
                            }
                            // 块处于选中态（用户可见高亮）就必须消费按键：
                            // 复制失败（空输出/块已淘汰）也只清选中、绝不
                            // 下穿成中断——误发 ^C 会取消用户输入的命令行。
                            if state.selected_block.is_some() {
                                state.copy_selected_block();
                                state.selected_block = None;
                                state.window.request_redraw();
                                return;
                            }
                            if state.modifiers.shift_key() {
                                return; // Ctrl+Shift+C 无选区时不下发
                            }
                        }
                        // Ctrl+V / Ctrl+Shift+V 粘贴。
                        Some('v') => {
                            state.paste_clipboard();
                            return;
                        }
                        _ => {}
                    }
                }
                // win32-input-mode 实验性开关（LUMEN_WIN32_INPUT=1 启用）：
                // 实测当前编码实现反而更卡，默认关闭待核对协议规范。
                let use_win32 = state.term.win32_input()
                    && std::env::var_os("LUMEN_WIN32_INPUT").is_some();
                let bytes = if use_win32 {
                    input::encode_key_win32(&event, state.modifiers, true)
                } else {
                    input::encode_key(&event, state.modifiers)
                };
                if let Some(bytes) = bytes {
                    state.term.grid_mut().scroll_to_bottom();
                    let write_t0 = Instant::now();
                    if let Err(e) = state.pty.write(&bytes) {
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
                if state.selecting {
                    let head = state.sel_point_at_mouse();
                    if let Some(sel) = state.selection.as_mut() {
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
                    state.selecting = true;
                    // 单击先建立空选区（不高亮），拖动后才有内容。
                    state.selection = Some(Selection { anchor: p, head: p });
                    state.window.request_redraw();
                }
                (MouseButton::Left, ElementState::Released) => {
                    // 本次按下不在终端区（点的是 egui 面板）则与终端无关。
                    if !state.selecting {
                        return;
                    }
                    state.selecting = false;
                    if state.selection.is_some_and(|s| s.is_empty()) {
                        // 单击（未拖动）：选中/清除所在命令块。
                        // 备用屏幕下块行号坐标系不可用，不做块选中。
                        state.selection = None;
                        if !state.term.is_alt_screen() {
                            let p = state.sel_point_at_mouse();
                            let hit = state.term.block_at_line(p.line).map(|b| b.id);
                            state.selected_block = if hit == state.selected_block {
                                None
                            } else {
                                hit
                            };
                        }
                        state.window.request_redraw();
                    }
                }
                (MouseButton::Right, ElementState::Pressed) => {
                    if !state.mouse_in_term() {
                        return;
                    }
                    // 右键：有选区则复制，否则粘贴（Windows Terminal 惯例）。
                    if state.copy_selection() {
                        state.selection = None;
                        state.window.request_redraw();
                    } else {
                        state.paste_clipboard();
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
                if let Err(e) = state.pty.write(text.as_bytes()) {
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
                    state.term.grid_mut().scroll_display(lines);
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

                state.term.grid_mut().take_dirty();
                // 光标跟随策略：正常情况下零延迟跟随终端光标；处于
                // 「帧尾未归位」窗口（ESU 后还没重新显示光标）时冻结
                // 旧位置，等归位序列或超时，避免画出重绘残留位。
                let now = Instant::now();
                let g = state.term.grid();
                let seen = (g.cursor.row, g.cursor.col, g.cursor.visible);
                // 同行近距移动是打字/退格的特征，即时跟随不冻结；
                // 动画残留位的特征是跨行大跳，才需要等归位确认。
                let typing_move = seen.2
                    && state.cursor_displayed.2
                    && seen.0 == state.cursor_displayed.0
                    && seen.1.abs_diff(state.cursor_displayed.1) <= 4;
                if !state.term.cursor_unsettled() || typing_move {
                    state.cursor_frozen_at = None;
                    state.cursor_displayed = seen;
                } else {
                    let frozen = *state.cursor_frozen_at.get_or_insert(now);
                    if now.duration_since(frozen) >= CURSOR_FREEZE_CAP {
                        state.cursor_displayed = seen;
                        state.cursor_frozen_at = None;
                    } else if state.cursor_displayed != seen {
                        // 安排超时时刻补画一帧，防止光标停滞在旧位。
                        let at = frozen + CURSOR_FREEZE_CAP;
                        state.redraw_at = Some(state.redraw_at.map_or(at, |x| x.min(at)));
                    }
                }

                // —— egui 帧：跑 UI 布局，产出本帧终端区矩形 ——
                let raw_input = state.egui_state.take_egui_input(&state.window);
                let title = state.term.title().to_owned();
                let tex_id = state.term_tex_id;
                let mut shell_out = None;
                let full_output = state.egui_ctx.run_ui(raw_input, |ui| {
                    shell_out = Some(shell::show(ui, tex_id, &title));
                });
                let Some(shell_out) = shell_out else {
                    return; // run_ui 必然执行闭包，防御分支
                };
                if shell_out.term_clicked {
                    state.terminal_focused = true;
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
                    let (rows, cols) = state.renderer.grid_size_for(tw, th);
                    let g = state.term.grid();
                    if (rows, cols) != (g.rows(), g.cols()) {
                        state.term.resize(rows, cols);
                        let _ = state.pty.resize(rows as u16, cols as u16);
                        // 尺寸变化会夹紧光标位置，立即同步绘制态。
                        let g = state.term.grid();
                        state.cursor_displayed = (g.cursor.row, g.cursor.col, g.cursor.visible);
                    }
                }
                let cursor = state
                    .cursor_displayed
                    .2
                    .then_some((state.cursor_displayed.0, state.cursor_displayed.1));

                // —— 终端管线渲染到离屏纹理（damage/行缓存机制原样）——
                if let Err(e) = state.renderer.render(
                    &state.term,
                    state.selection.as_ref(),
                    cursor,
                    state.selected_block,
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
                    let g = state.term.grid();
                    let view_row = (g.display_offset() + state.cursor_displayed.0)
                        .min(g.rows().saturating_sub(1));
                    let (cx, cy) = state
                        .renderer
                        .cell_origin(view_row, state.cursor_displayed.1);
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
