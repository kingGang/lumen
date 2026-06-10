//! Lumen 主程序：winit 事件循环，组装 PTY → 终端状态机 → 渲染器。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod input;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use log::{error, info};
use lumen_pty::{PtyEvent, PtySession};
use lumen_renderer::Renderer;
use lumen_term::{SelPoint, Selection, Terminal};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

/// scrollback 容量（行）。
const SCROLLBACK: usize = 10_000;

/// PTY 输出的合帧窗口：数据到达后稍等片刻，把同一逻辑帧被 ConPTY
/// 切开的碎块攒齐再渲染，避免把 TUI 重绘中间态（光标停在临时
/// 位置、半成品行）画上屏。对打字回显是无感的延迟。
const REDRAW_DEBOUNCE: Duration = Duration::from_millis(5);
/// 同步更新顺延的硬上限：高帧率 TUI 下合帧到点时常处于下一帧的
/// BSU/ESU 区间内，需要顺延等帧完成；但应用若卡死在 BSU 不放，
/// 超过该时长就强制渲染，避免画面冻结。
const REDRAW_HARD_CAP: Duration = Duration::from_millis(100);

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
    window: Arc<Window>,
    renderer: Renderer,
    term: Terminal,
    pty: PtySession,
    pty_rx: Receiver<PtyEvent>,
    /// 与转发线程共享的「wake 已挂起」标志，用于事件去重。
    wake_pending: Arc<AtomicBool>,
    modifiers: ModifiersState,
    clipboard: Option<arboard::Clipboard>,
    /// 合帧渲染的截止时刻（最早一批未渲染数据的到达时间 + 窗口）。
    redraw_at: Option<Instant>,
    /// 本轮合帧的强制渲染时刻（同步区间顺延的硬上限）。
    redraw_hard_at: Option<Instant>,
    /// 鼠标最近一次的窗口内像素位置。
    mouse_pos: (f64, f64),
    /// 左键按住拖选中。
    selecting: bool,
    selection: Option<Selection>,
}

impl AppState {
    /// 把当前鼠标像素位置换算成选区端点（绝对行号）。
    fn sel_point_at_mouse(&self) -> SelPoint {
        let (row, col) = self.renderer.cell_at(self.mouse_pos.0, self.mouse_pos.1);
        SelPoint {
            line: self.term.grid().view_top_abs_line() + row as u64,
            col,
        }
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

        let size = window.inner_size();
        let scale = window.scale_factor() as f32;
        let renderer = Renderer::new(window.clone(), size.width, size.height, scale)
            .context("初始化渲染器失败")?;
        let (rows, cols) = renderer.grid_size();
        info!("终端尺寸: {rows} 行 x {cols} 列");

        let term = Terminal::new(rows, cols, SCROLLBACK);
        let (pty, rx) = PtySession::spawn(None, rows as u16, cols as u16)?;

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

        Ok(AppState {
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
            mouse_pos: (0.0, 0.0),
            selecting: false,
            selection: None,
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

        let mut got_data = false;
        let mut exited = false;
        while let Ok(ev) = state.pty_rx.try_recv() {
            match ev {
                PtyEvent::Data(bytes) => {
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
            // 有新输出时跟随到底部。
            state.term.grid_mut().scroll_to_bottom();
            if !state.term.title().is_empty() {
                state
                    .window
                    .set_title(&format!("Lumen — {}", state.term.title()));
            }
            // DEC 2026 同步更新进行中不渲染，等 ESU 到达的批次一次画出，
            // 否则会把 TUI 重绘中间态（光标游走）上屏。
            // 渲染本身经合帧窗口延迟（见 about_to_wait），吸收 ConPTY
            // 对同一逻辑帧的切块。
            if !state.term.is_synchronized() && state.redraw_at.is_none() {
                let now = Instant::now();
                state.redraw_at = Some(now + REDRAW_DEBOUNCE);
                state.redraw_hard_at = Some(now + REDRAW_HARD_CAP);
            }
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
        let Some(at) = state.redraw_at else {
            return;
        };
        let now = Instant::now();
        if now < at {
            event_loop.set_control_flow(ControlFlow::WaitUntil(at));
            return;
        }
        // 到点时若已进入下一帧的同步区间，顺延等帧完成（高帧率 TUI
        // 下两帧间隔可小于合帧窗口），但不超过硬上限。
        if state.term.is_synchronized() && state.redraw_hard_at.is_some_and(|h| now < h) {
            let next = now + REDRAW_DEBOUNCE;
            state.redraw_at = Some(next);
            event_loop.set_control_flow(ControlFlow::WaitUntil(next));
            return;
        }
        state.redraw_at = None;
        state.redraw_hard_at = None;
        event_loop.set_control_flow(ControlFlow::Wait);
        state.window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(mods) => state.modifiers = mods.state(),
            WindowEvent::Resized(size) => {
                state.renderer.resize(size.width, size.height);
                let (rows, cols) = state.renderer.grid_size();
                state.term.resize(rows, cols);
                let _ = state.pty.resize(rows as u16, cols as u16);
                state.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                use winit::keyboard::{Key, NamedKey};
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
                    let ch = match &event.logical_key {
                        Key::Character(s) => s.chars().next().map(|c| c.to_ascii_lowercase()),
                        _ => None,
                    };
                    match ch {
                        // 有选区时 Ctrl+C 复制（Windows Terminal 惯例），
                        // 无选区时按正常路径发送中断（0x03）。
                        Some('c') => {
                            if state.copy_selection() {
                                state.selection = None;
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
                if let Some(bytes) = input::encode_key(&event, state.modifiers) {
                    state.term.grid_mut().scroll_to_bottom();
                    if let Err(e) = state.pty.write(&bytes) {
                        error!("写入 PTY 失败: {e:#}");
                    }
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
                    let p = state.sel_point_at_mouse();
                    state.selecting = true;
                    // 单击先建立空选区（不高亮），拖动后才有内容。
                    state.selection = Some(Selection { anchor: p, head: p });
                    state.window.request_redraw();
                }
                (MouseButton::Left, ElementState::Released) => {
                    state.selecting = false;
                    if state.selection.is_some_and(|s| s.is_empty()) {
                        state.selection = None;
                        state.window.request_redraw();
                    }
                }
                (MouseButton::Right, ElementState::Pressed) => {
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
                // 中文等 IME 提交的文本直接写入 shell。
                if let Err(e) = state.pty.write(text.as_bytes()) {
                    error!("写入 PTY 失败: {e:#}");
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
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
                state.term.grid_mut().take_dirty();
                if let Err(e) = state.renderer.render(&state.term, state.selection.as_ref()) {
                    error!("渲染失败: {e:#}");
                }
            }
            _ => {}
        }
    }
}
