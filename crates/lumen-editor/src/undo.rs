//! Undo/Redo 栈（[`UndoStack`]）。
//!
//! 合并策略：
//! - **连续 `InsertText` 合并**：相邻的 `InsertText` 在光标连续（无跳变）时合并为一个 undo 组。
//! - **断组触发器**：`InsertNewline`、`DeleteBackward`、`DeleteForward`、`DeleteWordBackward`、
//!   `Move`、`SetText`、`Clear`、`SelectAll`、`SetSelection`——这些 Action 处理后
//!   [`UndoStack::close_group`] 被调用，下一次 `InsertText` 开启新组。
//! - **上限**：最大 1000 组，超出时丢弃最旧条目（防无限膨胀）。

/// undo 栈最大组数。超出时丢弃最旧条目。
const MAX_GROUPS: usize = 1000;

/// 一条 undo 快照（文档文本 + 光标位置 + 选区锚点）。
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    /// 全文内容（行间含 `\n`）。
    pub(crate) text: String,
    /// 光标字节偏移（行索引 + 行内字节）。
    pub(crate) cursor_line: usize,
    pub(crate) cursor_byte: usize,
    /// 锚点（`None` = 纯光标）。
    pub(crate) anchor_line: Option<usize>,
    pub(crate) anchor_byte: Option<usize>,
}

/// Undo/Redo 栈。
///
/// 内部维护两个 `Vec<Snapshot>`：`undo_stack`（可 undo 的历史）
/// 和 `redo_stack`（可 redo 的未来）。任何新操作到来时清空 redo_stack。
#[derive(Debug, Clone, Default)]
pub(crate) struct UndoStack {
    /// 可撤销的历史快照（最后一项 = 最新的可撤销状态）。
    undo_stack: Vec<Snapshot>,
    /// 可重做的快照（最后一项 = 最近撤销的状态）。
    redo_stack: Vec<Snapshot>,
    /// 当前 undo 组是否已关闭（下一次操作需开新组）。
    group_closed: bool,
}

impl UndoStack {
    /// 在操作**前**保存快照（push 到 undo_stack）。
    ///
    /// `is_continuation`：是否是连续打字合并（`true` 时若当前组未关闭则不重新 push，
    /// 而是原地更新——调用 [`UndoStack::amend_top`] 更新快照文本）。
    ///
    /// 注意：调用方负责在操作**执行后**调用 [`UndoStack::amend_top`] 更新当前快照的
    /// 「执行后」文本，以便 redo 时恢复正确状态。本设计为简单两段式：
    /// - `push_before`：操作前快照（undo 时回到此状态）
    /// - 操作执行（由 Document 完成）
    /// - `amend_top` / `update_after`：不需要；undo 时直接用 before 快照。
    ///
    /// 实际实现：每次保存「执行前」快照压栈，undo 时弹出并恢复。
    pub(crate) fn push_before(&mut self, snap: Snapshot, is_continuation: bool) {
        if is_continuation && !self.group_closed && !self.undo_stack.is_empty() {
            // 连续打字：不新增条目，保留最早的「执行前」快照（已在栈中）
            self.redo_stack.clear();
            return;
        }
        // 新 undo 组：清空 redo，压当前快照
        self.redo_stack.clear();
        self.undo_stack.push(snap);
        if self.undo_stack.len() > MAX_GROUPS {
            self.undo_stack.remove(0);
        }
        self.group_closed = false;
    }

    /// 标记当前组已关闭（下次操作开新组）。
    pub(crate) fn close_group(&mut self) {
        self.group_closed = true;
    }

    /// 弹出 undo 快照（如有）；同时把「undo 前的当前状态」压入 redo_stack。
    ///
    /// 调用方负责传入 `current`（undo 前的状态快照），用于 redo 恢复。
    pub(crate) fn undo(&mut self, current: Snapshot) -> Option<Snapshot> {
        let snap = self.undo_stack.pop()?;
        self.redo_stack.push(current);
        self.group_closed = true;
        Some(snap)
    }

    /// 弹出 redo 快照（如有）；同时把「redo 前的状态」压回 undo_stack。
    pub(crate) fn redo(&mut self, current: Snapshot) -> Option<Snapshot> {
        let snap = self.redo_stack.pop()?;
        self.undo_stack.push(current);
        self.group_closed = true;
        Some(snap)
    }

    /// undo 栈是否有内容（调试/视图用）。
    #[allow(dead_code)]
    pub(crate) fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    /// redo 栈是否有内容（调试/视图用）。
    #[allow(dead_code)]
    pub(crate) fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(text: &str) -> Snapshot {
        Snapshot {
            text: text.to_string(),
            cursor_line: 0,
            cursor_byte: text.len(),
            anchor_line: None,
            anchor_byte: None,
        }
    }

    #[test]
    fn test_undo_基本压栈弹出() {
        let mut stack = UndoStack::default();
        stack.push_before(snap("a"), false);
        stack.close_group();
        stack.push_before(snap("ab"), false);
        let restored = stack.undo(snap("abc")).unwrap();
        assert_eq!(restored.text, "ab");
    }

    #[test]
    fn test_连续打字合并() {
        let mut stack = UndoStack::default();
        // 第一次打字：开新组（保存 "a" 前状态 ""）
        stack.push_before(snap(""), false);
        // 连续打字：不新增
        stack.push_before(snap("a"), true);
        stack.push_before(snap("ab"), true);
        // undo 应回到 ""（合并为一组）
        let restored = stack.undo(snap("abc")).unwrap();
        assert_eq!(restored.text, "");
        // redo 回到 "abc"
        let redone = stack.redo(snap("")).unwrap();
        assert_eq!(redone.text, "abc");
    }

    #[test]
    fn test_断组后新组() {
        let mut stack = UndoStack::default();
        stack.push_before(snap(""), false);
        stack.push_before(snap("a"), true);
        stack.close_group(); // 移动断组
        stack.push_before(snap("ab"), false); // 新组
                                              // 两次 undo：分别回到 "ab" 和 ""
        let r1 = stack.undo(snap("abc")).unwrap();
        assert_eq!(r1.text, "ab");
        let r2 = stack.undo(snap("ab")).unwrap();
        assert_eq!(r2.text, "");
    }

    #[test]
    fn test_redo_被新操作清空() {
        let mut stack = UndoStack::default();
        stack.push_before(snap(""), false);
        stack.close_group();
        let _ = stack.undo(snap("a"));
        // 新操作：清空 redo
        stack.push_before(snap(""), false);
        assert!(!stack.can_redo());
    }

    #[test]
    fn test_上限防膨胀() {
        let mut stack = UndoStack::default();
        for i in 0..=MAX_GROUPS + 10 {
            stack.push_before(snap(&i.to_string()), false);
            stack.close_group();
        }
        assert!(stack.undo_stack.len() <= MAX_GROUPS);
    }
}
