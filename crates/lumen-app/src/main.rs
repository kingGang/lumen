//! Lumen 主程序：winit 事件循环，组装 PTY → 终端状态机 → 渲染器。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod input;

use std::sync::Arc;

use anyhow::{Context, Result};
use log::{error, info};
use lumen_pty::{PtyEvent, PtySession};
use lumen_renderer::Renderer;
use lumen_term::Terminal;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

/// scrollback 容量（行）。
const SCROLLBACK: usize = 10_000;

/// 自定义事件：PTY 输出经转发线程注入事件循环。
#[derive(Debug)]
enum UserEvent {
    Pty(PtyEvent),
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("创建事件循环失败")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App { proxy, state: None };
    event_loop.run_app(&mut app).context("事件循环异常退出")?;
    Ok(())
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    state: Option<AppState>,
}

struct AppState {
    window: Arc<Window>,
    renderer: Renderer,
    term: Terminal,
    pty: PtySession,
    modifiers: ModifiersState,
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

        // 转发线程：crossbeam channel → winit 事件循环。
        let proxy = self.proxy.clone();
        std::thread::Builder::new()
            .name("lumen-pty-forward".into())
            .spawn(move || {
                for ev in rx {
                    if proxy.send_event(UserEvent::Pty(ev)).is_err() {
                        break;
                    }
                }
            })
            .context("启动 PTY 转发线程失败")?;

        Ok(AppState {
            window,
            renderer,
            term,
            pty,
            modifiers: ModifiersState::default(),
        })
    }
}

impl ApplicationHandler<UserEvent> for App {
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

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match event {
            UserEvent::Pty(PtyEvent::Data(bytes)) => {
                state.term.advance(&bytes);
                // 终端应答（DSR/DA 等）回写给 shell。
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
                state.window.request_redraw();
            }
            UserEvent::Pty(PtyEvent::Exited) => {
                info!("shell 已退出，关闭窗口");
                event_loop.exit();
            }
        }
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
                // Shift+PgUp/PgDn 本地翻屏，不发给 shell。
                use winit::keyboard::{Key, NamedKey};
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
                }
                if let Some(bytes) = input::encode_key(&event, state.modifiers) {
                    state.term.grid_mut().scroll_to_bottom();
                    if let Err(e) = state.pty.write(&bytes) {
                        error!("写入 PTY 失败: {e:#}");
                    }
                }
            }
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
                if let Err(e) = state.renderer.render(&state.term) {
                    error!("渲染失败: {e:#}");
                }
            }
            _ => {}
        }
    }
}
