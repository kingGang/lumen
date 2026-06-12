//! Action 总线（M4.1 批B）——设计稿 §6。
//!
//! # 纪律（铁律）
//! **凡绕过 [`AppState::dispatch`] 直接改状态的代码，code review 一律打回。**
//!
//! 所有用户输入（键盘 / IME / 鼠标 / M4 远程指令）必须在翻译为 Action 后
//! 经由 `dispatch` 统一执行；`dispatch` 返回 [`Vec<StateEvent>`] 供消费方
//! 驱动渲染/历史库/状态条/M4 状态增量同步。
//!
//! # 分期说明
//! - 批B（本批）：Action 枚举定义 + KeyStroke + StateEvent 骨架 + dispatch 骨架；
//!   `Edit(_)` / `Composer(_)` 本批返回空事件（批D 接编辑器时填充）；
//!   `Term(_)` 本批完整实现（VT 编码下沉、写 PTY）。
//! - 批D：`Edit(EditAction)` 接 lumen-editor；`Composer(ComposerAction)` 接历史/补全。
//! - M4：Action 整体上移 `lumen-protocol`，编辑器 crate 不动。

use serde::{Deserialize, Serialize};

/// 统一 Action 枚举（全 serde derive，带版本注释，M4 上移 lumen-protocol 不破坏接口）。
///
/// # Variants
/// - `Edit`：由 `lumen-editor` 消费；批B 暂存，批D 接线。
/// - `Composer`：提交 / 历史 / 补全等；批B 仅定义枚举，批D 接行为。
/// - `Term`：语义键写 PTY / 翻屏 / 块跳转等；批B 完整实现。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    /// 编辑器文档操作（lumen-editor::EditAction），批D 接线。
    Edit(EditAction),
    /// Composer 控制器操作（提交 / 历史 / 补全），批D 接行为。
    Composer(ComposerAction),
    /// 终端操作（写 PTY / 翻屏 / 块跳转 / 粘贴 / 复制）。
    Term(TermAction),
}

// ─────────────────────────────────────────────────────────────────
// EditAction（来自 lumen-editor 设计稿 §5，批D 接线时与 crate 对齐）
// ─────────────────────────────────────────────────────────────────

/// 编辑器文档操作（设计稿 §5，serde derive 为 M4 远程回放准备）。
///
/// 批B 阶段 dispatch 收到 `Edit(_)` 仅记 debug log，不接编辑器。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EditAction {
    /// 插入文本。
    InsertText(String),
    /// 插入换行。
    InsertNewline,
    /// 向后删除一个字符（Backspace）。
    DeleteBackward,
    /// 向前删除一个字符（Delete）。
    DeleteForward,
    /// 向后删除一个词。
    DeleteWordBackward,
    /// 移动光标 / 扩展选区。
    Move { motion: Motion, extend: bool },
    /// 设置选区（光标绝对定位）。
    SetSelection(Selection),
    /// 全选。
    SelectAll,
    /// 整体替换内容（历史 / 补全入口）。
    SetText(String),
    /// 撤销。
    Undo,
    /// 重做。
    Redo,
    /// 清空编辑器缓冲。
    Clear,
}

/// 光标移动方式（设计稿 §5）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Motion {
    /// 按 grapheme 向左。
    GraphemeLeft,
    /// 按 grapheme 向右。
    GraphemeRight,
    /// 向左跳一词。
    WordLeft,
    /// 向右跳一词。
    WordRight,
    /// 行首。
    LineStart,
    /// 行尾。
    LineEnd,
    /// 上一行。
    Up,
    /// 下一行。
    Down,
    /// 文档开头。
    DocStart,
    /// 文档结尾。
    DocEnd,
}

/// 文本选区（字节偏移）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Selection {
    /// 锚点位置。
    pub anchor: Position,
    /// 光标（活动端）位置。
    pub head: Position,
}

/// 光标/锚点在文档中的位置（字节偏移）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    /// 行下标（0-based）。
    pub line: usize,
    /// 字节偏移（按 grapheme 对齐）。
    pub byte: usize,
}

// ─────────────────────────────────────────────────────────────────
// ComposerAction（设计稿 §6，批B 只定义枚举）
// ─────────────────────────────────────────────────────────────────

/// Composer 控制器操作（设计稿 §6）。
///
/// 批B 阶段 dispatch 收到 `Composer(_)` 仅记 debug log；批D 接历史 / 补全。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ComposerAction {
    /// 提交当前编辑器缓冲（Enter 键，Compose 态）。
    Submit,
    /// 历史向上导航（↑）。
    HistoryPrev,
    /// 历史向下导航（↓）。
    HistoryNext,
    /// 历史搜索面板（Ctrl+R，M3.3）。
    HistorySearch { query: String },
    /// 补全弹窗打开（Tab，M3.4）。
    CompletionOpen,
    /// 补全选下一项。
    CompletionNext,
    /// 补全选上一项。
    CompletionPrev,
    /// 接受当前补全项。
    CompletionAccept,
    /// 关闭补全弹窗。
    CompletionDismiss,
    /// 取消当前输入行（Ctrl+C 清空并存「放弃稿」）。
    CancelLine,
}

// ─────────────────────────────────────────────────────────────────
// TermAction
// ─────────────────────────────────────────────────────────────────

/// 终端操作（设计稿 §6）。
///
/// `SendKey(KeyStroke)` 在 dispatch 内查终端模式（bracketed_paste /
/// win32-input / alt-screen）现场编码后写 PTY；本地与远端走同一路径。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TermAction {
    /// 语义键（键位 + 修饰），dispatch 内编码写 PTY。
    SendKey(KeyStroke),
    /// 直接写文本到 PTY（IME Commit / Fallback / Running 态文字输入）。
    SendText(String),
    /// 发送 ETX（Ctrl+C 中断，无条件直通）。
    Interrupt,
    /// 粘贴文本（dispatch 内按 bracketed_paste 状态包装）。
    Paste(String),
    /// 翻屏（Shift+PgUp / Shift+PgDn）。
    Scroll(ScrollDir),
    /// 命令块间跳转（Ctrl+↑ / Ctrl+↓）。
    JumpBlock(i64),
    /// 复制选区（Ctrl+C 第一级）。
    CopySelection,
    /// 复制选中块输出（Ctrl+C 第二级）。
    CopyBlock,
    /// 滚到底部（submit 前保证可见）。
    ScrollToBottom,
    /// 从剪贴板粘贴（dispatch 内按 bracketed_paste 状态包装）。
    PasteClipboard,
    /// 切换经典直通模式（Ctrl+Shift+E，全模式可用）。
    ToggleFallback,
}

/// 翻屏方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollDir {
    /// 向上翻一屏（Shift+PgUp）。
    Up,
    /// 向下翻一屏（Shift+PgDn）。
    Down,
}

/// 可序列化的语义键（不含 winit 类型，符合 M4 serde 铁律）。
///
/// 编码时 dispatch 内部按终端模式选 [`crate::input::encode_key`]
/// 或 [`crate::input::encode_key_win32`]，此结构体本身不含 VT 字节。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyStroke {
    /// 物理键的语义表示。
    pub key: LogicalKey,
    /// 修饰键状态。
    pub mods: KeyMods,
}

/// 逻辑键（不依赖 winit 类型，可序列化，M4 远程协议用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LogicalKey {
    /// 可打印字符（单字符字符串，如 `"a"` / `"1"` / `","`）。
    Character(String),
    /// 具名键（Enter / Tab / Backspace / 方向键 / Fn 键等）。
    Named(NamedKey),
}

/// 具名键枚举（覆盖 VT 编码所需的全部键，与 winit::NamedKey 一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NamedKey {
    // 控制
    /// Enter / Return。
    Enter,
    /// Tab。
    Tab,
    /// Backspace。
    Backspace,
    /// Escape。
    Escape,
    /// Space。
    Space,
    // 方向
    /// 上方向键。
    ArrowUp,
    /// 下方向键。
    ArrowDown,
    /// 左方向键。
    ArrowLeft,
    /// 右方向键。
    ArrowRight,
    // 导航
    /// Home。
    Home,
    /// End。
    End,
    /// Page Up。
    PageUp,
    /// Page Down。
    PageDown,
    /// Insert。
    Insert,
    /// Delete。
    Delete,
    // 功能键
    /// F1。
    F1,
    /// F2。
    F2,
    /// F3。
    F3,
    /// F4。
    F4,
    /// F5。
    F5,
    /// F6。
    F6,
    /// F7。
    F7,
    /// F8。
    F8,
    /// F9。
    F9,
    /// F10。
    F10,
    /// F11。
    F11,
    /// F12。
    F12,
}

/// 修饰键状态（可序列化）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyMods {
    /// Ctrl 键。
    pub ctrl: bool,
    /// Shift 键。
    pub shift: bool,
    /// Alt 键。
    pub alt: bool,
    /// Super / Win 键。
    pub logo: bool,
}

// ALLOW: 这些常量在批D（keymap 远程协议序列化）才会用到，当前批B骨架阶段尚未引用。
#[allow(dead_code)]
impl KeyMods {
    /// 无修饰键。
    pub const NONE: Self = Self {
        ctrl: false,
        shift: false,
        alt: false,
        logo: false,
    };

    /// 只有 Ctrl。
    pub const CTRL: Self = Self {
        ctrl: true,
        shift: false,
        alt: false,
        logo: false,
    };

    /// 只有 Shift。
    pub const SHIFT: Self = Self {
        ctrl: false,
        shift: true,
        alt: false,
        logo: false,
    };

    /// 只有 Alt。
    pub const ALT: Self = Self {
        ctrl: false,
        shift: false,
        alt: true,
        logo: false,
    };

    /// Ctrl + Shift。
    pub const CTRL_SHIFT: Self = Self {
        ctrl: true,
        shift: true,
        alt: false,
        logo: false,
    };
}

// ─────────────────────────────────────────────────────────────────
// StateEvent
// ─────────────────────────────────────────────────────────────────

/// dispatch 返回的状态事件（设计稿 §6 观察者出口）。
///
/// M3 阶段：驱动状态条 / 历史库；M4：即状态增量同步的事件源。
/// 批B：只定义枚举 + dispatch 返回骨架，消费方后批接。
// ALLOW: ModeChanged 变体目前只在事件推导中产生，消费方在 M4 状态增量接通时引用。
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum StateEvent {
    /// 输入模式发生了切换（模式纯函数值变化）。
    ModeChanged(crate::mode::InputMode),
    /// 用户提交了文本（批D 接 Composer 时填充）。
    SubmittedText {
        /// 提交的原始文本。
        text: String,
        /// 提交时刻（用于 pending_submit 时序计算）。
        submitted_at: std::time::Instant,
        /// 历史库条目下标（用于块闭合时回填 exit_code）。
        history_idx: usize,
    },
    /// 命令块闭合（退出码已知，M4.1 批D2 接消费方）。
    BlockClosed {
        /// 命令块 id。
        block_id: u64,
        /// 进程退出码。
        exit_code: Option<i32>,
        /// 命令耗时（毫秒；从 pending_submit.submitted_at 到块闭合的时长）。
        duration_ms: u64,
    },
    /// 编辑器 revision 变更（批D 接编辑器时填充）。
    EditorRevision(u64),
    /// 经典直通模式切换（Ctrl+Shift+E）。
    FallbackToggled(bool),
}
