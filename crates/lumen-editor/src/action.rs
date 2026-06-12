//! 编辑动作枚举（[`EditAction`]）与移动语义（[`Motion`]）。
//!
//! 所有变更必须经由 [`crate::Editor::apply`] 派发，此处两个枚举均实现
//! `serde::{Serialize, Deserialize}`——第一天可序列化（设计稿铁律）。
//! 同一 Action 序列可录制后在新 Editor 上重放，终态确定性相同（M4 远程一致性地基）。

use serde::{Deserialize, Serialize};

use crate::cursor::Selection;

/// 光标/选区移动语义。
///
/// 与 [`EditAction::Move`] 配合使用；`extend = true` 时移动锚点不动，产生/扩展选区。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Motion {
    /// 向左移动一个 grapheme cluster。
    GraphemeLeft,
    /// 向右移动一个 grapheme cluster。
    GraphemeRight,
    /// 向左跳一个词（空白/字母数字/符号三类切换处，不引入 UAX#29 词边界）。
    WordLeft,
    /// 向右跳一个词。
    WordRight,
    /// 跳到当前行行首（字节偏移 0）。
    LineStart,
    /// 跳到当前行行尾（最后一个字节之后）。
    LineEnd,
    /// 向上移动一行，保持「目标列」显示宽度记忆（宽字符占 2 列）。
    Up,
    /// 向下移动一行，保持「目标列」显示宽度记忆。
    Down,
    /// 跳到文档首行行首。
    DocStart,
    /// 跳到文档末行行尾。
    DocEnd,
}

/// 编辑器的唯一状态变更指令。
///
/// 所有字段全部可序列化，支持录制/回放（M4 远程一致性地基）。
/// 非法参数（如空字符串的 `InsertText`、越界的 `SetSelection`）由
/// [`crate::Editor::apply`] 静默夹紧为合法操作，**不 panic**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditAction {
    /// 在光标处插入文本（可含多行；`\n` 分割为新行）。
    /// 连续的 `InsertText` 在光标未跳变时合并为同一 undo 组。
    InsertText(String),

    /// 在光标处插入换行（等价于在当前位置分行）。
    /// 此 Action 会断开 undo 组（下次 `InsertText` 另起新组）。
    InsertNewline,

    /// 删除光标左侧一个 grapheme cluster；有选区时删除选区内容。
    DeleteBackward,

    /// 删除光标右侧一个 grapheme cluster；有选区时删除选区内容。
    DeleteForward,

    /// 向左删除一个词（Ctrl+Backspace 语义）。
    DeleteWordBackward,

    /// 移动光标（或扩展选区）。
    ///
    /// `extend = false`：光标移动后收起选区；
    /// `extend = true`：锚点固定，光标移动产生/扩展选区（Shift+方向键）。
    Move {
        /// 移动方向与粒度。
        motion: Motion,
        /// `true` 表示 Shift 键按住：锚点不动，扩展选区。
        extend: bool,
    },

    /// 直接设置选区（鼠标拖选、API 调用）。
    /// 越界坐标由 apply 静默夹紧到文档合法范围。
    SetSelection(Selection),

    /// 全选（等同于 DocStart anchor + DocEnd cursor）。
    SelectAll,

    /// 整体替换文档内容（历史导航/补全接受的入口）。
    /// 会断开 undo 组，替换前的状态可通过 Undo 恢复。
    SetText(String),

    /// 撤销（回退一个 undo 组）。
    Undo,

    /// 重做（前进一个 undo 组）。
    Redo,

    /// 清空文档（等价于 `SetText("")` 但语义更明确，Ctrl+C 在缓冲非空时调用）。
    /// 清空前的文本由 [`crate::Editor::take_abandoned`] 取回（放弃稿语义）。
    Clear,
}
