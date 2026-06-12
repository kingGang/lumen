//! 会话：一个 PTY 子进程 + 终端状态机 + 每会话的渲染/交互状态。
//!
//! 多会话架构（M3.2，规格见 docs/M3应用外壳设计.md §2；M3.6 P5 通道
//! 改造；M3.7 F5 分屏升级为 `Vec<Tab>`，每 tab 1~6 个窗格、窗格 =
//! [`Session`]）：各会话的 PTY 事件经独立转发线程送入**本会话自己的
//! 有界通道**（[`Session::rx`]），背压只作用于该会话的读线程链路、
//! 互不连坐；`PtyWake` 无数据 user event + 全局 `wake_pending` 去重
//! 协议与单会话时代零变化（任一会话的转发线程都可触发 wake）。渲染
//! 调度只对激活 tab 的窗格生效；后台 tab 的窗格照常消化数据并回写
//! 应答（DSR/DA 不回写会卡死对端程序），有新输出时只标记未读点。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use log::{error, info};
#[cfg(feature = "input-editor")]
use lumen_editor::Editor;
use lumen_pty::{PtyEvent, PtySession};
use lumen_term::{Selection, Terminal};
use winit::event_loop::EventLoopProxy;

use crate::shell::layout::PaneLayout;
use crate::PtyWake;

#[cfg(feature = "input-editor")]
use lumen_renderer::composer_view::PreeditState;

/// 会话唯一标识。自增分配、关闭后不复用——退出列表/侧栏动作等按
/// id 寻会话，复用会与滞后引用相撞；通道里的残留事件随本会话的
/// Receiver 一并丢弃，无需按 id 过滤。
pub type SessionId = u64;

/// Tab 唯一标识。自增分配、关闭后不复用（侧栏动作按 id 寻 tab，
/// 与 SessionId 同理防滞后引用相撞）。
pub type TabId = u64;

/// 每个 tab 的窗格数上限（F5 拍板：最多 6 格）。
pub const MAX_PANES: usize = 6;

/// 一个侧栏 tab：1~6 个终端窗格（分屏，F5 / M3.7）。
///
/// 语义映射：侧栏条目 = Tab；原「激活会话」概念 = 「激活 tab 的
/// 焦点窗格」（窗口标题、IME、键盘路由、文件树 cwd 都取它）。
/// 窗格按布局顺序存放：两排时先上排后下排、行内自左向右
/// （见 shell::layout::pane_rects）。
pub struct Tab {
    pub id: TabId,
    /// 用户重命名的标题；None 时跟随默认规则（焦点窗格 cwd >
    /// OSC 标题，见 [`Self::display_title`]）。
    pub custom_title: Option<String>,
    /// 窗格列表。**恒非空**——最后一个窗格关闭即关整个 tab
    /// （main::close_pane 维护此不变量）。
    pub panes: Vec<Session>,
    /// 焦点窗格在 `panes` 中的下标。增删窗格时由调用方维护合法性，
    /// 访问器仍做防御夹紧。
    pub focused: usize,
    /// 窗格比例布局（F7③：每排高度权重 + 排内列宽权重，网格结构由
    /// 窗格数推导）。增删窗格时由 main 重置均分（简单正确优先）；
    /// 拖动分隔条调整，随 sessions.json 持久化、重启还原。
    pub layout: PaneLayout,
    /// 最大化的窗格下标（P14）：Some 时该窗格占满整个终端工作区，
    /// 其余窗格隐藏（照常后台消化输出、不渲染，同「非激活 tab」
    /// 闸门）；焦点强制为该窗格。增删窗格自动退出；随 sessions.json
    /// 持久化、重启保持。不变量：Some(m) 时 m < panes.len()（toggle/
    /// 增删/恢复路径维护）。
    pub maximized: Option<usize>,
}

impl Tab {
    /// 焦点窗格（键盘/IME/滚轮/选区/粘贴/块操作的路由目标）。
    /// 下标防御夹紧；`panes` 恒非空（见字段不变量）。
    pub fn focused_pane(&self) -> &Session {
        &self.panes[self.focused.min(self.panes.len() - 1)]
    }

    /// 焦点窗格（可变）。
    pub fn focused_pane_mut(&mut self) -> &mut Session {
        let i = self.focused.min(self.panes.len() - 1);
        &mut self.panes[i]
    }

    /// 展示标题（侧栏条目与窗口标题同源此函数）。取值优先级：
    /// 自定义名（用户重命名）> 焦点窗格 cwd（OSC 9;9 完整路径，
    /// cd 后跟随）> 焦点窗格 OSC 0/2 标题 > 「会话 N」（N = tab
    /// id + 1）。
    pub fn display_title(&self) -> String {
        if let Some(t) = &self.custom_title {
            return t.clone();
        }
        self.focused_pane().default_title().unwrap_or_else(|| {
            crate::i18n::fmt1(crate::i18n::strings().session_default_name_fmt, self.id + 1)
        })
    }

    /// 默认标题当前是否来自焦点窗格的 cwd（侧栏据此挂全路径悬停提示）。
    pub fn title_is_cwd(&self) -> bool {
        self.custom_title.is_none() && self.focused_pane().term.cwd().is_some()
    }

    /// tab 内任意窗格有未读输出（侧栏未读点；切到本 tab 时全清）。
    pub fn has_unseen(&self) -> bool {
        self.panes.iter().any(|p| p.has_unseen_output)
    }
}

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
    /// [`SESSION_EVENT_CAP`]）。drain 由主循环按「焦点窗格优先」轮询。
    pub rx: Receiver<PtyEvent>,
    /// 启动恢复时的初始工作目录（F4 持久化）：OSC 9;9 尚未上报期间，
    /// 会话快照写盘以它为 cwd 回退值——否则恢复后还没等到提示符上报
    /// 就触发写盘（如切 tab）会把保存的 cwd 冲成 None。新建会话为
    /// None（cwd 未知，等首个提示符上报）。
    pub initial_cwd: Option<PathBuf>,
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
    /// M4.1 批D1：焦点窗格的输入编辑器（`input-editor` feature 门控）。
    /// 挂在窗格生命周期内，模式切换不清缓冲（草稿保全）。
    #[cfg(feature = "input-editor")]
    pub editor: Editor,
    /// M4.1 批D1：pending_submit — 提交但尚未看到 C 标记的在途命令。
    /// `(提交文本, 提交时刻, 历史库条目下标)`；C 标记到达后（块切 Running）清空。
    #[cfg(feature = "input-editor")]
    pub pending_submit: Option<(String, std::time::Instant, usize)>,
    /// M4.1 批D2：上次已消费的已闭合块总数（闭合块计数去重探针）。
    /// `advance()` 后若闭合块数增加，说明有新 OSC 133 D 事件，即块刚闭合。
    #[cfg(feature = "input-editor")]
    pub last_seen_closed_blocks: usize,
    /// M4.1 批D2：IME 预编辑状态（Compose 态 Preedit 事件更新，Commit/离开 Compose 时清空）。
    #[cfg(feature = "input-editor")]
    pub preedit: Option<PreeditState>,
    /// M4.1 批D2：退出码角标（块闭合时设置，任意键盘事件时清空）。
    #[cfg(feature = "input-editor")]
    pub exit_badge: Option<lumen_renderer::composer_view::ExitBadge>,
    /// M4.1 批D2：footer 目标像素高度（用于增高防抖）。
    #[cfg(feature = "input-editor")]
    pub footer_target_h: f32,
    /// M4.1 批D2：footer 目标高度上次变化时刻（增高防抖计时）。
    #[cfg(feature = "input-editor")]
    pub footer_h_changed_at: std::time::Instant,
    /// M4.1 批D2：当前已提交给 renderer 的 footer 高度（防抖实际生效值）。
    #[cfg(feature = "input-editor")]
    pub footer_committed_h: f32,
}

impl Session {
    /// 启动一个新会话：spawn shell、起转发线程把 PTY 事件送入本会话
    /// 自己的有界通道，并以去重信号唤醒事件循环（信号挂起期间不重复
    /// 发，避免事件风暴——协议与单会话时代一致）。
    /// `cwd` 为 shell 初始工作目录（会话恢复用，F4；调用方须先验证
    /// 目录存在）；None 沿用默认目录。
    pub fn spawn(
        id: SessionId,
        rows: usize,
        cols: usize,
        scrollback: usize,
        wake_pending: Arc<AtomicBool>,
        proxy: EventLoopProxy<PtyWake>,
        cwd: Option<&Path>,
    ) -> Result<Self> {
        let term = Terminal::new(rows, cols, scrollback);
        let (pty, pty_rx) = PtySession::spawn(
            None,
            &shell_integration_args(),
            rows as u16,
            cols as u16,
            cwd,
        )?;
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
            initial_cwd: cwd.map(Path::to_path_buf),
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
            #[cfg(feature = "input-editor")]
            editor: Editor::default(),
            #[cfg(feature = "input-editor")]
            pending_submit: None,
            #[cfg(feature = "input-editor")]
            last_seen_closed_blocks: 0,
            #[cfg(feature = "input-editor")]
            preedit: None,
            #[cfg(feature = "input-editor")]
            exit_badge: None,
            #[cfg(feature = "input-editor")]
            footer_target_h: 0.0,
            #[cfg(feature = "input-editor")]
            footer_h_changed_at: std::time::Instant::now(),
            #[cfg(feature = "input-editor")]
            footer_committed_h: 0.0,
        })
    }

    /// 窗格的默认标题：cwd（OSC 9;9 上报的当前目录完整路径，cd 后
    /// 随下一个提示符上报自动跟随）> OSC 0/2 标题；两者皆无返回
    /// None（由 [`Tab::display_title`] 落「会话 N」兜底）。PowerShell
    /// 默认把窗口标题设成 shell exe 路径，直接展示并不直观——cwd
    /// 优先于它。
    pub fn default_title(&self) -> Option<String> {
        if let Some(cwd) = self.term.cwd() {
            return Some(cwd.display().to_string());
        }
        let t = self.term.title();
        (!t.is_empty()).then(|| t.to_owned())
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

    /// 向 PTY 写入用户主动产生的输入（统一收口）。
    ///
    /// **所有**用户写 PTY 的路径必须经此方法，不得直接调用 `pty.write`：
    /// - 键盘编码写入（`encode_key` / `encode_key_win32`）
    /// - IME Commit 文本写入
    /// - 粘贴（`paste_clipboard` 内部调用此方法）
    /// - 文件树 cd 注入（`cd_command` 生成后经此写入）
    /// - 拖拽路径插入（`path_insert_text` 生成后经此写入）
    ///
    /// 统一收口便于将来在此处叠加审计、限流等横切逻辑，无需逐处改动。
    /// B3-7：注入守卫跟踪字段（user_input_since_prompt / shell_waiting_input_last /
    /// last_inject_at）已随 resize 注入机制整体拆除，包装方法本身保留收口价值。
    ///
    /// # Errors
    /// 返回 `pty.write` 的底层错误（PTY 管道写失败，通常为 ConPTY 进
    /// 程已退出）。
    pub fn write_user_input(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.pty.write(bytes)
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
        if let Err(e) = self.write_user_input(&payload) {
            error!("粘贴写入 PTY 失败: {e:#}");
        }
    }
}

/// 生成 shell 启动参数，把集成脚本（OSC 133 命令边界 + OSC 9;9 cwd 上报）
/// **内联注入** shell——不落地任何文件。
///
/// 仅 Windows PowerShell（pwsh/powershell）走此路径；其它 shell 暂返回空参数
/// （shell 照常可用，只是无命令块/cwd 上报，自然降级 Fallback）。
///
/// 用 `-EncodedCommand`（base64 的 UTF-16LE）取代旧的「写 .ps1 到 Temp + dot-source」，
/// 一举根治三类分发故障（海风哥 2026-06-13 拷贝到他机实测）：
/// 1. **不落地文件**——换机 / 只拷 exe / 受限环境都不再「找不到文件」；
/// 2. **不读 .ps1 文件**——ExecutionPolicy / 组策略只管文件加载、管不到内联命令，
///    企业锁策略的机器也能注入；
/// 3. **base64 精确传字节**——规避脚本（含中文注释）被读取端系统 ANSI 代码页解码
///    致乱码、解析报 `UnexpectedToken`（本机中文系统 + UTF-8 pwsh 正常，拷到西文 /
///    Windows PowerShell 5.1 机器即触发的真凶）。
fn shell_integration_args() -> Vec<String> {
    if !cfg!(windows) {
        // TODO（跨平台）：unix bash/zsh 走各自内联注入（--rcfile /dev/stdin、
        // PROMPT_COMMAND、ZDOTDIR 等）；此处先不注入、干净降级。
        return Vec::new();
    }
    let script = include_str!("../assets/integration.ps1");
    // PowerShell `-EncodedCommand` 约定：base64( UTF-16LE 编码的命令文本 )。
    let utf16le: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    vec![
        "-NoLogo".into(),
        "-NoExit".into(),
        "-EncodedCommand".into(),
        base64_encode(&utf16le),
    ]
}

/// 标准 base64 编码（RFC 4648，带 `=` padding，无换行）。
///
/// 自实现、零依赖：仅 [`shell_integration_args`] 注入 `-EncodedCommand` 用，
/// 只需编码方向。字母表 `A-Za-z0-9+/`，PowerShell `-EncodedCommand` 接受此标准变体。
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_编码_rfc4648_标准向量() {
        // RFC 4648 §10 测试向量。
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_编码_含高位字节与padding() {
        // +/= 边界字符与单/双 padding 覆盖。
        assert_eq!(base64_encode(&[0xFB, 0xFF, 0xBF]), "+/+/");
        assert_eq!(base64_encode(&[0xFF]), "/w==");
        assert_eq!(base64_encode(&[0xFF, 0xFF]), "//8=");
    }

    #[test]
    fn base64_编码_往返_utf16le_中文不丢() {
        // 中文经 UTF-16LE→base64 应保留全部字节（长度=字符数×2，4 字节一组对齐）。
        let s = "命令块 OSC133";
        let utf16le: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let enc = base64_encode(&utf16le);
        // 解码字符集合法 + 长度符合 base64( n 字节 ) = ceil(n/3)*4。
        assert_eq!(enc.len(), utf16le.len().div_ceil(3) * 4);
        assert!(enc
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')));
    }

    #[cfg(windows)]
    #[test]
    fn shell集成_走encoded_command不落地文件() {
        let args = shell_integration_args();
        assert!(
            args.iter().any(|a| a == "-EncodedCommand"),
            "Windows 下应走 -EncodedCommand 内联注入"
        );
        assert!(
            !args.iter().any(|a| a.contains(".ps1")),
            "不应再落地或引用任何 .ps1 文件"
        );
        assert!(args.iter().any(|a| a == "-NoExit"));
        let encoded = args.last().expect("应有 base64 参数");
        assert!(!encoded.is_empty(), "脚本 base64 不应为空");
        assert!(
            encoded
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')),
            "base64 应只含标准字母表"
        );
    }
}
