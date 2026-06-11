//! 会话：一个 PTY 子进程 + 终端状态机 + 每会话的渲染/交互状态。
//!
//! 多会话架构（M3.2，规格见 docs/M3应用外壳设计.md §2；M3.6 P5 通道
//! 改造）：主循环持有 `Vec<Session>`，各会话的 PTY 事件经独立转发
//! 线程送入**本会话自己的有界通道**（[`Session::rx`]），背压只作用于
//! 该会话的读线程链路、互不连坐；`PtyWake` 无数据 user event + 全局
//! `wake_pending` 去重协议与单会话时代零变化（任一会话的转发线程都
//! 可触发 wake）。渲染调度只对激活会话生效；后台会话照常消化数据并
//! 回写应答（DSR/DA 不回写会卡死对端程序），有新输出时只标记未读点。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use log::{error, info};
use lumen_pty::{PtyEvent, PtySession};
use lumen_term::{Selection, Terminal};
use winit::event_loop::EventLoopProxy;

use crate::PtyWake;

/// 会话唯一标识。自增分配、关闭后不复用——退出列表/侧栏动作等按
/// id 寻会话，复用会与滞后引用相撞；通道里的残留事件随本会话的
/// Receiver 一并丢弃，无需按 id 过滤。
pub type SessionId = u64;

/// 每会话事件通道容量（事件粒度为 PTY 读线程的单次 read，至多
/// 8KiB）：满时转发线程的 send 阻塞，背压沿「转发线程 → PTY 读线程
/// → ConPTY 管道」传导回本会话的 shell，**不再连坐其他会话**——旧
/// 的全局单通道下，后台洪泛会让激活会话的回显排在最坏 ~2MB 之后
/// （队头阻塞，延迟尖峰 10~30ms，需求池 P5）。
const SESSION_EVENT_CAP: usize = 256;

/// 一个终端会话的全部独立状态。
///
/// Drop 即随 `PtySession` 杀掉 shell 子进程（关 tab = 杀进程）；
/// [`Self::rx`] 同时被丢弃，转发线程 send 失败退出，其持有的 PTY
/// 事件接收端随之释放、读线程跟着结束，无需额外清理。
pub struct Session {
    pub id: SessionId,
    pub term: Terminal,
    pub pty: PtySession,
    /// 本会话的 PTY 事件接收端（per-session 有界通道，见
    /// [`SESSION_EVENT_CAP`]）。drain 由主循环按「激活优先」轮询。
    pub rx: Receiver<PtyEvent>,
    /// 用户重命名的标题；None 时跟随 term.title()（OSC 0/2）。
    pub custom_title: Option<String>,
    /// 上次处理批的 ESU 标记，用于检测「本批完成了同步帧」。
    pub last_esu_mark: u64,
    /// 实际绘制中的光标态 (行, 列, 可见)。光标处于「帧尾未归位」
    /// 状态（见 Terminal::cursor_unsettled）时冻结不跟随。
    pub cursor_displayed: (usize, usize, bool),
    /// 光标冻结起点（超时后强制信任当前位置）。
    pub cursor_frozen_at: Option<Instant>,
    /// 左键按住拖选中。
    pub selecting: bool,
    pub selection: Option<Selection>,
    /// 选中的命令块 id。
    pub selected_block: Option<u64>,
    /// 静默渲染时刻：最后一批数据到达时间 + 静默窗口（每批后推）。
    /// 渲染计划只在本会话激活时被调度与执行；切换激活时清除残留。
    pub redraw_at: Option<Instant>,
    /// 强制渲染时刻：首批未渲染数据 + 硬上限（保障最低刷新率）。
    pub redraw_hard_at: Option<Instant>,
    /// 绝对兜底时刻：超过后即使在同步区间内也渲染。
    pub redraw_abs_at: Option<Instant>,
    /// 「欠一帧终端渲染」的起点时刻：渲染计划到点（about_to_wait 清
    /// 计划并请求重绘）或 ESU 快路直渲时置位（已置位保留更早起点）。
    /// RedrawRequested 的同步区间门控对欠帧**可以暂缓**——新 BSU 批
    /// 赶在重绘执行前重新拉起同步区间时，交给重新武装的渲染计划在
    /// ESU 后补画完整帧，不把半成品 grid 画上屏（蓝条闪烁来源之一，
    /// 需求池 P1）；但暂缓不超过 REDRAW_ABS_CAP：欠帧超龄后无论是否
    /// 同步一律放行渲染，保住「不会卡死在 BSU 画面冻结」的绝对兜底
    /// （否则计划被反复重新武装会让欠帧无限顺延）。终端离屏真正渲染
    /// 过即清 None。
    pub term_frame_due_since: Option<Instant>,
    /// 后台期间有新输出（tab 未读点；切换到本会话时清除）。
    pub has_unseen_output: bool,
}

impl Session {
    /// 启动一个新会话：spawn shell、起转发线程把 PTY 事件送入本会话
    /// 自己的有界通道，并以去重信号唤醒事件循环（信号挂起期间不重复
    /// 发，避免事件风暴——协议与单会话时代一致）。
    pub fn spawn(
        id: SessionId,
        rows: usize,
        cols: usize,
        scrollback: usize,
        wake_pending: Arc<AtomicBool>,
        proxy: EventLoopProxy<PtyWake>,
    ) -> Result<Self> {
        let term = Terminal::new(rows, cols, scrollback);
        let (pty, pty_rx) =
            PtySession::spawn(None, &shell_integration_args(), rows as u16, cols as u16)?;
        // per-session 有界通道：主循环持接收端，转发线程持发送端。
        let (tx, rx) = crossbeam_channel::bounded::<PtyEvent>(SESSION_EVENT_CAP);
        std::thread::Builder::new()
            .name(format!("lumen-pty-forward-{id}"))
            .spawn(move || {
                for ev in pty_rx {
                    // 通道满时 send 阻塞（背压只传导回本会话的读线程）；
                    // 会话关闭（Receiver 随 Session Drop 丢弃）时返回
                    // Err，线程自然退出——阻塞中的 send 也会被唤醒。
                    if tx.send(ev).is_err() {
                        break;
                    }
                    if !wake_pending.swap(true, Ordering::AcqRel)
                        && proxy.send_event(PtyWake).is_err()
                    {
                        break;
                    }
                }
            })
            .context("启动 PTY 转发线程失败")?;
        info!("会话创建 id={id}（{rows} 行 x {cols} 列）");
        Ok(Self {
            id,
            term,
            pty,
            rx,
            custom_title: None,
            last_esu_mark: 0,
            cursor_displayed: (0, 0, true),
            cursor_frozen_at: None,
            selecting: false,
            selection: None,
            selected_block: None,
            redraw_at: None,
            redraw_hard_at: None,
            redraw_abs_at: None,
            term_frame_due_since: None,
            has_unseen_output: false,
        })
    }

    /// 展示用标题：用户自定义名优先，否则跟随终端 OSC 标题。
    pub fn display_title(&self) -> &str {
        self.custom_title
            .as_deref()
            .unwrap_or_else(|| self.term.title())
    }

    /// 复制选中命令块的输出到剪贴板，返回是否复制了内容。
    pub fn copy_selected_block(&mut self, clipboard: &mut Option<arboard::Clipboard>) -> bool {
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
        match clipboard.as_mut().map(|c| c.set_text(text)) {
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
    /// 返回是否发生了跳转（调用方据此请求重绘）。
    pub fn jump_block(&mut self, dir: i64) -> bool {
        // 选中的块可能已被上限淘汰：先清掉失效 id，按未选中逻辑走。
        if self
            .selected_block
            .is_some_and(|id| self.term.block_by_id(id).is_none())
        {
            self.selected_block = None;
        }
        let blocks = self.term.blocks();
        if blocks.is_empty() {
            return false;
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
        true
    }

    /// 复制选区文本到剪贴板，返回是否真的复制了内容。
    pub fn copy_selection(&mut self, clipboard: &mut Option<arboard::Clipboard>) -> bool {
        let Some(sel) = self.selection.filter(|s| !s.is_empty()) else {
            return false;
        };
        let text = self.term.selection_text(&sel);
        if text.is_empty() {
            return false;
        }
        match clipboard.as_mut().map(|c| c.set_text(text)) {
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
    pub fn paste_clipboard(&mut self, clipboard: &mut Option<arboard::Clipboard>) {
        let Some(Ok(text)) = clipboard.as_mut().map(|c| c.get_text()) else {
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
