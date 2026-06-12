//! # lumen-editor — Lumen 输入编辑器纯状态机
//!
//! ## 设计铁律（摘自输入编辑器设计.md §1 / §5）
//!
//! 1. **唯一可变入口**：[`Editor::apply`] 是唯一修改 Editor 状态的公开方法，
//!    字段全私有，类型层强制不绕过。
//! 2. **确定性 replay**：同一 [`EditAction`] 序列在任意新 Editor 上重放，
//!    终态（文本 + 光标 + 选区 + revision）完全一致（M4 远程一致性地基）。
//! 3. **零 winit/wgpu/pty 依赖**：本 crate 100% 可单测，不引入任何图形/IO 库。
//!
//! ## 快速示例
//!
//! ```rust
//! use lumen_editor::{Editor, EditAction, Motion};
//!
//! let mut editor = Editor::default();
//! editor.apply(&EditAction::InsertText("hello".to_string()));
//! editor.apply(&EditAction::Move { motion: Motion::LineStart, extend: false });
//! editor.apply(&EditAction::InsertText("say: ".to_string()));
//! let view = editor.view();
//! assert_eq!(view.text(), "say: hello");
//! ```

pub mod action;
pub mod cursor;
mod document;
mod undo;

pub use action::{EditAction, Motion};
pub use cursor::{Position, Selection};

use document::Document;
use undo::{Snapshot, UndoStack};

// ─── 公开类型 ─────────────────────────────────────────────────────────────────

/// [`Editor::apply`] 的返回值：描述本次操作的影响范围。
///
/// 渲染层可依据 `doc_changed` 决定是否使文字缓存失效，
/// 依据 `revision` 判断是否需要重绘。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOutcome {
    /// 操作后 Editor 的 revision 版本（单调递增；即使文档未变也可能变更，如移动光标）。
    pub revision: u64,
    /// 文档内容是否发生变化（`true` = 渲染缓存需要失效）。
    pub doc_changed: bool,
    /// 光标/选区是否发生变化（`true` = 渲染需要重绘光标）。
    pub selection_changed: bool,
}

/// Editor 的只读视图，借用自 Editor 内部状态（零拷贝）。
///
/// 渲染层通过此类型读取文本/光标/选区，不持有 Editor 可变引用，
/// 不能触发任何状态变更（类型层保证）。
pub struct EditorView<'a> {
    doc: &'a Document,
    cursor: Position,
    anchor: Position,
    goal_col: Option<usize>,
}

impl<'a> EditorView<'a> {
    /// 全文内容（行间含 `\n`）。
    pub fn text(&self) -> String {
        self.doc.to_string_full()
    }

    /// 按行迭代（零分配，借用内部 `Vec<String>`）。
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.doc.lines.iter().map(|s| s.as_str())
    }

    /// 行数（>= 1）。
    pub fn line_count(&self) -> usize {
        self.doc.line_count()
    }

    /// 指定行内容。
    pub fn line(&self, row: usize) -> &str {
        self.doc.line(row)
    }

    /// 光标位置。
    pub fn cursor(&self) -> Position {
        self.cursor
    }

    /// 选区（`anchor == cursor` 时为纯光标）。
    pub fn selection(&self) -> Selection {
        Selection {
            anchor: self.anchor,
            cursor: self.cursor,
        }
    }

    /// 是否有非空选区。
    pub fn has_selection(&self) -> bool {
        self.anchor != self.cursor
    }

    /// 当前目标列（Up/Down 跨行时的显示列记忆）。
    pub fn goal_col(&self) -> Option<usize> {
        self.goal_col
    }
}

// ─── Editor ──────────────────────────────────────────────────────────────────

/// 输入编辑器状态机。
///
/// 所有字段私有。外部只能通过 [`Editor::apply`] 修改状态，
/// 通过 [`Editor::view`] 读取只读视图。
///
/// 不变量（类型层 + 运行时保证）：
/// - `lines` 至少含 1 行（空文档 = `[""]`）。
/// - `cursor` 和 `anchor` 始终在文档合法范围内（合法 UTF-8 字符边界）。
/// - `revision` 单调递增，任何 `apply` 调用后至少 +1。
#[derive(Default)]
pub struct Editor {
    doc: Document,
    /// 光标活动端（始终合法）。
    cursor: Position,
    /// 选区锚点（与 cursor 相同时为纯光标）。
    anchor: Position,
    /// Up/Down 跨行时记忆的目标显示列；移动后由 apply 按操作类型更新或清零。
    goal_col: Option<usize>,
    undo: UndoStack,
    /// Ctrl+C 放弃稿（`take_abandoned` 取走后置 `None`）。
    abandoned: Option<String>,
    /// 单调递增版本号（渲染缓存失效判据 + M4 增量同步版本锚点）。
    revision: u64,
}

impl Editor {
    // ─── 只读 API ─────────────────────────────────────────────────────────────

    /// 返回只读视图（文本 + 光标 + 选区）。
    pub fn view(&self) -> EditorView<'_> {
        EditorView {
            doc: &self.doc,
            cursor: self.cursor,
            anchor: self.anchor,
            goal_col: self.goal_col,
        }
    }

    /// 当前 revision 版本号。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// 取走放弃稿（Ctrl+C 存入；取走后置 `None`）。
    pub fn take_abandoned(&mut self) -> Option<String> {
        self.abandoned.take()
    }

    /// 存入放弃稿（供 app 层在 Ctrl+C 语义下调用）。
    /// 已有放弃稿时会被覆盖（保留最近一次）。
    pub fn stash_abandoned(&mut self, text: String) {
        self.abandoned = Some(text);
    }

    /// 是否存在放弃稿。
    pub fn has_abandoned(&self) -> bool {
        self.abandoned.is_some()
    }

    /// M3.2 预留：检测是否需要续行（未闭合引号/括号/行尾管道等）。
    /// 当前批次仅返回 `false`（占位；M3.2 实现 PowerShell tokenizer 后填充）。
    pub fn needs_continuation(&self) -> bool {
        false
    }

    // ─── 唯一可变入口 ─────────────────────────────────────────────────────────

    /// 处理一个 [`EditAction`]，更新内部状态，返回 [`EditOutcome`]。
    ///
    /// **这是 Editor 的唯一可变入口。**
    ///
    /// 越界参数（如 `SetSelection` 超出文档范围）静默夹紧为合法操作，不 panic，
    /// 也不返回 `Err`——所有操作保证成功。
    ///
    /// # Errors
    /// 此方法不返回 `Result`，永不失败。
    pub fn apply(&mut self, action: &EditAction) -> EditOutcome {
        let before_doc = self.doc.to_string_full();
        let before_cursor = self.cursor;
        let before_anchor = self.anchor;

        self.dispatch(action);

        self.revision += 1;
        let after_doc = self.doc.to_string_full();
        EditOutcome {
            revision: self.revision,
            doc_changed: after_doc != before_doc,
            selection_changed: self.cursor != before_cursor || self.anchor != before_anchor,
        }
    }

    // ─── 内部 dispatch ────────────────────────────────────────────────────────

    fn dispatch(&mut self, action: &EditAction) {
        match action {
            EditAction::InsertText(text) => self.do_insert_text(text),
            EditAction::InsertNewline => self.do_insert_newline(),
            EditAction::DeleteBackward => self.do_delete_backward(),
            EditAction::DeleteForward => self.do_delete_forward(),
            EditAction::DeleteWordBackward => self.do_delete_word_backward(),
            EditAction::Move { motion, extend } => self.do_move(motion, *extend),
            EditAction::SetSelection(sel) => self.do_set_selection(*sel),
            EditAction::SelectAll => self.do_select_all(),
            EditAction::SetText(text) => self.do_set_text(text),
            EditAction::Undo => self.do_undo(),
            EditAction::Redo => self.do_redo(),
            EditAction::Clear => self.do_clear(),
        }
    }

    // ─── Action 实现 ──────────────────────────────────────────────────────────

    fn do_insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // 有选区时先删除选区内容
        let had_selection = self.delete_selection_if_any();
        // 保存 undo 快照（连续打字合并）
        let is_continuation = !had_selection && self.goal_col.is_none();
        self.push_undo(is_continuation);
        // 插入
        let new_cursor = self.doc.insert_text(self.cursor, text);
        self.cursor = new_cursor;
        self.anchor = new_cursor;
        self.goal_col = None;
    }

    fn do_insert_newline(&mut self) {
        self.delete_selection_if_any();
        self.push_undo(false);
        let new_cursor = self.doc.insert_newline(self.cursor);
        self.cursor = new_cursor;
        self.anchor = new_cursor;
        self.goal_col = None;
        self.undo.close_group();
    }

    fn do_delete_backward(&mut self) {
        if self.cursor != self.anchor {
            // 有选区：删选区
            self.delete_selection_with_undo();
        } else {
            self.push_undo(false);
            let new_pos = self.doc.delete_backward(self.cursor);
            self.cursor = new_pos;
            self.anchor = new_pos;
        }
        self.goal_col = None;
        self.undo.close_group();
    }

    fn do_delete_forward(&mut self) {
        if self.cursor != self.anchor {
            self.delete_selection_with_undo();
        } else {
            self.push_undo(false);
            let new_pos = self.doc.delete_forward(self.cursor);
            self.cursor = new_pos;
            self.anchor = new_pos;
        }
        self.goal_col = None;
        self.undo.close_group();
    }

    fn do_delete_word_backward(&mut self) {
        if self.cursor != self.anchor {
            self.delete_selection_with_undo();
        } else {
            self.push_undo(false);
            let new_pos = self.doc.delete_word_backward(self.cursor);
            self.cursor = new_pos;
            self.anchor = new_pos;
        }
        self.goal_col = None;
        self.undo.close_group();
    }

    fn do_move(&mut self, motion: &Motion, extend: bool) {
        let (new_cursor, new_goal) = self.compute_move(motion, extend);
        if !extend {
            // 无 Shift：收起选区（锚点跟随光标）
            self.anchor = new_cursor;
        }
        self.cursor = new_cursor;
        self.goal_col = new_goal;
        self.undo.close_group();
    }

    fn do_set_selection(&mut self, sel: Selection) {
        let anchor = self.doc.clamp(sel.anchor);
        let cursor = self.doc.clamp(sel.cursor);
        self.anchor = anchor;
        self.cursor = cursor;
        self.goal_col = None;
        self.undo.close_group();
    }

    fn do_select_all(&mut self) {
        self.anchor = self.doc.move_doc_start();
        self.cursor = self.doc.move_doc_end();
        self.goal_col = None;
        self.undo.close_group();
    }

    fn do_set_text(&mut self, text: &str) {
        // SetText 保存独立 undo 组（整体替换可撤销）
        self.push_undo(false);
        self.doc.set_text(text);
        let end = self.doc.move_doc_end();
        self.cursor = end;
        self.anchor = end;
        self.goal_col = None;
        self.undo.close_group();
    }

    fn do_undo(&mut self) {
        let current = self.make_snapshot();
        if let Some(snap) = self.undo.undo(current) {
            self.restore_snapshot(snap);
        }
        self.goal_col = None;
    }

    fn do_redo(&mut self) {
        let current = self.make_snapshot();
        if let Some(snap) = self.undo.redo(current) {
            self.restore_snapshot(snap);
        }
        self.goal_col = None;
    }

    fn do_clear(&mut self) {
        // 存放弃稿
        let text = self.doc.to_string_full();
        if !text.is_empty() {
            self.abandoned = Some(text);
        }
        self.push_undo(false);
        self.doc.set_text("");
        self.cursor = Position::default();
        self.anchor = Position::default();
        self.goal_col = None;
        self.undo.close_group();
    }

    // ─── 辅助方法 ─────────────────────────────────────────────────────────────

    /// 计算 Motion 对应的新光标位置和新 goal_col。
    ///
    /// `extend = false` 时，有选区的 `GraphemeLeft/Right` 会收起选区（跳到选区 min/max 端），
    /// 而不是在活动端上再移动一步（与主流编辑器一致）。
    /// `extend = true` 时，光标始终在当前活动端上移动，锚点由调用方（`do_move`）保持不动。
    /// 返回 `(新光标, 新 goal_col)`。
    fn compute_move(&self, motion: &Motion, extend: bool) -> (Position, Option<usize>) {
        match motion {
            Motion::GraphemeLeft => {
                if !extend && self.cursor != self.anchor {
                    // extend=false 且有选区：收起到 min 端（不再移动）
                    let (start, _) = Selection {
                        anchor: self.anchor,
                        cursor: self.cursor,
                    }
                    .ordered();
                    (start, None)
                } else {
                    (self.doc.move_grapheme_left(self.cursor), None)
                }
            }
            Motion::GraphemeRight => {
                if !extend && self.cursor != self.anchor {
                    // extend=false 且有选区：收起到 max 端（不再移动）
                    let (_, end) = Selection {
                        anchor: self.anchor,
                        cursor: self.cursor,
                    }
                    .ordered();
                    (end, None)
                } else {
                    (self.doc.move_grapheme_right(self.cursor), None)
                }
            }
            Motion::WordLeft => (self.doc.move_word_left(self.cursor), None),
            Motion::WordRight => (self.doc.move_word_right(self.cursor), None),
            Motion::LineStart => (self.doc.move_line_start(self.cursor), None),
            Motion::LineEnd => (self.doc.move_line_end(self.cursor), None),
            Motion::Up => {
                let (pos, col) = self.doc.move_up(self.cursor, self.goal_col);
                (pos, Some(col))
            }
            Motion::Down => {
                let (pos, col) = self.doc.move_down(self.cursor, self.goal_col);
                (pos, Some(col))
            }
            Motion::DocStart => (self.doc.move_doc_start(), None),
            Motion::DocEnd => (self.doc.move_doc_end(), None),
        }
    }

    /// 若有选区，删除选区内容并返回 `true`；否则返回 `false`。
    fn delete_selection_if_any(&mut self) -> bool {
        if self.cursor == self.anchor {
            return false;
        }
        let (start, end) = Selection {
            anchor: self.anchor,
            cursor: self.cursor,
        }
        .ordered();
        let new_pos = self.doc.delete_range(start, end);
        self.cursor = new_pos;
        self.anchor = new_pos;
        true
    }

    /// 有选区时：保存 undo 并删除选区。
    fn delete_selection_with_undo(&mut self) {
        self.push_undo(false);
        self.delete_selection_if_any();
    }

    /// 构造当前状态快照（用于 undo/redo）。
    fn make_snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.doc.to_string_full(),
            cursor_line: self.cursor.line,
            cursor_byte: self.cursor.byte,
            anchor_line: if self.anchor != self.cursor {
                Some(self.anchor.line)
            } else {
                None
            },
            anchor_byte: if self.anchor != self.cursor {
                Some(self.anchor.byte)
            } else {
                None
            },
        }
    }

    /// 从快照恢复状态。
    fn restore_snapshot(&mut self, snap: Snapshot) {
        self.doc.set_text(&snap.text);
        // clamp 确保坐标合法
        self.cursor = self
            .doc
            .clamp(Position::new(snap.cursor_line, snap.cursor_byte));
        self.anchor = match (snap.anchor_line, snap.anchor_byte) {
            (Some(l), Some(b)) => self.doc.clamp(Position::new(l, b)),
            _ => self.cursor,
        };
    }

    /// 压 undo 快照（前状态）。
    fn push_undo(&mut self, is_continuation: bool) {
        let snap = self.make_snapshot();
        self.undo.push_before(snap, is_continuation);
    }
}

// ─── 单元测试 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── 基础 InsertText ────────────────────────────────────────────────────────

    #[test]
    fn test_插入文本_基本() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hello".to_string()));
        assert_eq!(e.view().text(), "hello");
        assert_eq!(e.view().cursor().byte, 5);
    }

    #[test]
    fn test_插入文本_中文() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("你好世界".to_string()));
        let view = e.view();
        assert_eq!(view.text(), "你好世界");
        // 4 个汉字各 3 字节 = 12 字节
        assert_eq!(view.cursor().byte, 12);
    }

    #[test]
    fn test_插入文本_emoji_zwj序列() {
        let mut e = Editor::default();
        let emoji = "👨‍👩‍👧‍👦";
        e.apply(&EditAction::InsertText(emoji.to_string()));
        assert_eq!(e.view().text(), emoji);
        assert_eq!(e.view().cursor().byte, emoji.len());
    }

    #[test]
    fn test_插入文本_组合变音() {
        let mut e = Editor::default();
        // é = U+0065(1字节) + U+0301 COMBINING ACUTE ACCENT(2字节) = 3字节，一个 grapheme
        let s = "e\u{0301}";
        e.apply(&EditAction::InsertText(s.to_string()));
        assert_eq!(e.view().cursor().byte, 3); // 3 字节（1 + 2）
    }

    #[test]
    fn test_插入换行_产生多行() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("ab".to_string()));
        e.apply(&EditAction::Move {
            motion: Motion::LineStart,
            extend: false,
        });
        e.apply(&EditAction::InsertNewline);
        let view = e.view();
        assert_eq!(view.line_count(), 2);
        assert_eq!(view.line(0), "");
        assert_eq!(view.line(1), "ab");
    }

    // ── DeleteBackward / DeleteForward ─────────────────────────────────────────

    #[test]
    fn test_退格删除_ascii() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hello".to_string()));
        e.apply(&EditAction::DeleteBackward);
        assert_eq!(e.view().text(), "hell");
        assert_eq!(e.view().cursor().byte, 4);
    }

    #[test]
    fn test_退格删除_中文grapheme() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("中文".to_string()));
        e.apply(&EditAction::DeleteBackward);
        assert_eq!(e.view().text(), "中");
    }

    #[test]
    fn test_退格删除_emoji_zwj() {
        let mut e = Editor::default();
        let emoji = "👨‍👩‍👧‍👦";
        e.apply(&EditAction::InsertText(emoji.to_string()));
        e.apply(&EditAction::DeleteBackward);
        // 整个 ZWJ 序列作为一个 grapheme 删除
        assert_eq!(e.view().text(), "");
    }

    #[test]
    fn test_前向删除_ascii() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hello".to_string()));
        e.apply(&EditAction::Move {
            motion: Motion::LineStart,
            extend: false,
        });
        e.apply(&EditAction::DeleteForward);
        assert_eq!(e.view().text(), "ello");
        assert_eq!(e.view().cursor().byte, 0);
    }

    #[test]
    fn test_退格删除_跨行合并() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("abc".to_string()));
        e.apply(&EditAction::InsertNewline);
        e.apply(&EditAction::InsertText("def".to_string()));
        // 移到第二行行首
        e.apply(&EditAction::Move {
            motion: Motion::LineStart,
            extend: false,
        });
        e.apply(&EditAction::DeleteBackward);
        assert_eq!(e.view().line_count(), 1);
        assert_eq!(e.view().text(), "abcdef");
    }

    #[test]
    fn test_删词退格() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hello world".to_string()));
        e.apply(&EditAction::DeleteWordBackward);
        assert_eq!(e.view().text(), "hello ");
    }

    // ── 选区操作 ─────────────────────────────────────────────────────────────

    #[test]
    fn test_选区扩展_shift方向() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hello".to_string()));
        e.apply(&EditAction::Move {
            motion: Motion::LineStart,
            extend: false,
        });
        // Shift+Right × 3
        for _ in 0..3 {
            e.apply(&EditAction::Move {
                motion: Motion::GraphemeRight,
                extend: true,
            });
        }
        let view = e.view();
        assert!(view.has_selection());
        let sel = view.selection();
        assert_eq!(sel.anchor.byte, 0);
        assert_eq!(sel.cursor.byte, 3);
    }

    #[test]
    fn test_选区替换输入() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hello world".to_string()));
        // 全选
        e.apply(&EditAction::SelectAll);
        // 输入替换
        e.apply(&EditAction::InsertText("hi".to_string()));
        assert_eq!(e.view().text(), "hi");
    }

    #[test]
    fn test_选区删除() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hello world".to_string()));
        // 选中 "world"（字节 6-11）
        e.apply(&EditAction::SetSelection(Selection {
            anchor: Position::new(0, 6),
            cursor: Position::new(0, 11),
        }));
        e.apply(&EditAction::DeleteBackward);
        assert_eq!(e.view().text(), "hello ");
    }

    #[test]
    fn test_全选() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("ab\ncd".to_string()));
        e.apply(&EditAction::SelectAll);
        let sel = e.view().selection();
        assert_eq!(sel.anchor, Position::new(0, 0));
        assert_eq!(sel.cursor, Position::new(1, 2));
    }

    // ── 光标移动 ─────────────────────────────────────────────────────────────

    #[test]
    fn test_行首行尾移动() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hello".to_string()));
        e.apply(&EditAction::Move {
            motion: Motion::LineStart,
            extend: false,
        });
        assert_eq!(e.view().cursor().byte, 0);
        e.apply(&EditAction::Move {
            motion: Motion::LineEnd,
            extend: false,
        });
        assert_eq!(e.view().cursor().byte, 5);
    }

    #[test]
    fn test_up_down_目标列记忆() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hello\nhi".to_string()));
        // 光标在 (1,2) 即 "hi" 末
        // 向上：记忆列 2，"hello" 列 2 → 字节 2
        e.apply(&EditAction::Move {
            motion: Motion::Up,
            extend: false,
        });
        let view = e.view();
        assert_eq!(view.cursor().line, 0);
        assert_eq!(view.cursor().byte, 2);
        // goal_col 应保留为 2
        assert_eq!(view.goal_col(), Some(2));
    }

    #[test]
    fn test_up_down_cjk目标列() {
        let mut e = Editor::default();
        // 第一行 "中文"（各 2 列），第二行 "a"（1 列）
        e.apply(&EditAction::InsertText("中文\na".to_string()));
        // 光标在 (1,1) 列 1，向上记忆列 1
        e.apply(&EditAction::Move {
            motion: Motion::Up,
            extend: false,
        });
        let view = e.view();
        assert_eq!(view.cursor().line, 0);
        // 列 1 在 "中文" 中：第一个汉字占 0-1 列，不满 2 列 → 字节 0
        // display_col_to_byte("中文", 1) = 0（累积 < 1 时取下一 grapheme 起始）
        // 实际: 汉字宽 2，累积 0 < 1 → 继续；累积 0+2=2 >= 1 → 返回当前 grapheme 起始 = 0
        assert_eq!(view.cursor().byte, 0);
    }

    #[test]
    fn test_docstart_docend() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("ab\ncd".to_string()));
        e.apply(&EditAction::Move {
            motion: Motion::DocStart,
            extend: false,
        });
        assert_eq!(e.view().cursor(), Position::new(0, 0));
        e.apply(&EditAction::Move {
            motion: Motion::DocEnd,
            extend: false,
        });
        assert_eq!(e.view().cursor(), Position::new(1, 2));
    }

    #[test]
    fn test_词跳转() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hello world".to_string()));
        e.apply(&EditAction::Move {
            motion: Motion::WordLeft,
            extend: false,
        });
        assert_eq!(e.view().cursor().byte, 6);
        e.apply(&EditAction::Move {
            motion: Motion::WordLeft,
            extend: false,
        });
        assert_eq!(e.view().cursor().byte, 5);
    }

    // ── Undo / Redo ──────────────────────────────────────────────────────────

    #[test]
    fn test_undo_基本() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hello".to_string()));
        e.apply(&EditAction::DeleteBackward); // 断组
        e.apply(&EditAction::Undo);
        // DeleteBackward 前的状态 = "hello"
        assert_eq!(e.view().text(), "hello");
    }

    #[test]
    fn test_undo_连续打字合并为一组() {
        let mut e = Editor::default();
        // 连续打字应合并
        e.apply(&EditAction::InsertText("h".to_string()));
        e.apply(&EditAction::InsertText("e".to_string()));
        e.apply(&EditAction::InsertText("l".to_string()));
        e.apply(&EditAction::InsertText("l".to_string()));
        e.apply(&EditAction::InsertText("o".to_string()));
        // 移动断组
        e.apply(&EditAction::Move {
            motion: Motion::LineStart,
            extend: false,
        });
        e.apply(&EditAction::Undo);
        // 一次 undo 回到空
        assert_eq!(e.view().text(), "");
    }

    #[test]
    fn test_undo_换行断组() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("ab".to_string()));
        e.apply(&EditAction::InsertNewline); // 断组
        e.apply(&EditAction::InsertText("cd".to_string()));
        e.apply(&EditAction::Move {
            motion: Motion::LineEnd,
            extend: false,
        }); // 断组
            // 第一次 undo：撤销 "cd" 的插入
        e.apply(&EditAction::Undo);
        assert_eq!(e.view().text(), "ab\n");
        // 第二次 undo：撤销 InsertNewline
        e.apply(&EditAction::Undo);
        assert_eq!(e.view().text(), "ab");
        // 第三次 undo：撤销 "ab" 的插入（合并为一组）
        e.apply(&EditAction::Undo);
        assert_eq!(e.view().text(), "");
    }

    #[test]
    fn test_redo() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hello".to_string()));
        e.apply(&EditAction::Move {
            motion: Motion::LineStart,
            extend: false,
        });
        e.apply(&EditAction::Undo);
        assert_eq!(e.view().text(), "");
        e.apply(&EditAction::Redo);
        assert_eq!(e.view().text(), "hello");
    }

    #[test]
    fn test_新操作清空redo栈() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hello".to_string()));
        e.apply(&EditAction::Move {
            motion: Motion::LineStart,
            extend: false,
        });
        e.apply(&EditAction::Undo);
        e.apply(&EditAction::InsertText("x".to_string()));
        // redo 应无效
        e.apply(&EditAction::Redo);
        assert_eq!(e.view().text(), "x");
    }

    // ── SetText / Clear / 放弃稿 ─────────────────────────────────────────────

    #[test]
    fn test_set_text_整体替换() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("old".to_string()));
        e.apply(&EditAction::SetText("new content".to_string()));
        assert_eq!(e.view().text(), "new content");
        assert_eq!(e.view().cursor().byte, 11);
    }

    #[test]
    fn test_set_text_可撤销() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("old".to_string()));
        e.apply(&EditAction::SetText("new".to_string()));
        e.apply(&EditAction::Undo);
        assert_eq!(e.view().text(), "old");
    }

    #[test]
    fn test_clear_存放弃稿() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("my draft".to_string()));
        e.apply(&EditAction::Clear);
        assert_eq!(e.view().text(), "");
        let abandoned = e.take_abandoned().unwrap();
        assert_eq!(abandoned, "my draft");
        assert!(!e.has_abandoned());
    }

    #[test]
    fn test_stash_take_abandoned() {
        let mut e = Editor::default();
        e.stash_abandoned("old draft".to_string());
        assert!(e.has_abandoned());
        let s = e.take_abandoned().unwrap();
        assert_eq!(s, "old draft");
        assert!(!e.has_abandoned());
    }

    // ── revision 单调递增 ────────────────────────────────────────────────────

    #[test]
    fn test_revision_单调递增() {
        let mut e = Editor::default();
        let r0 = e.revision();
        let o1 = e.apply(&EditAction::InsertText("a".to_string()));
        let o2 = e.apply(&EditAction::InsertText("b".to_string()));
        assert!(r0 < o1.revision);
        assert!(o1.revision < o2.revision);
    }

    #[test]
    fn test_doc_changed_标志() {
        let mut e = Editor::default();
        let o1 = e.apply(&EditAction::InsertText("x".to_string()));
        assert!(o1.doc_changed);
        // 纯移动不改文档
        let o2 = e.apply(&EditAction::Move {
            motion: Motion::LineStart,
            extend: false,
        });
        assert!(!o2.doc_changed);
        assert!(o2.selection_changed);
    }

    // ── 多行编辑 ─────────────────────────────────────────────────────────────

    #[test]
    fn test_多行插入与删除() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("line1\nline2\nline3".to_string()));
        assert_eq!(e.view().line_count(), 3);
        // 跨行选区删除
        e.apply(&EditAction::SetSelection(Selection {
            anchor: Position::new(0, 4),
            cursor: Position::new(1, 3),
        }));
        e.apply(&EditAction::DeleteBackward);
        // 选区 (0,4)-(1,3)：删除 "1\nlin"（line1[4..] + \n + line2[..3]）
        // 结果：line1[..4] + line2[3..] = "line" + "e2" = "linee2"，加第三行
        assert_eq!(e.view().text(), "linee2\nline3");
    }

    #[test]
    fn test_设置选区_越界夹紧() {
        let mut e = Editor::default();
        e.apply(&EditAction::InsertText("hi".to_string()));
        e.apply(&EditAction::SetSelection(Selection {
            anchor: Position::new(99, 999),
            cursor: Position::new(0, 1),
        }));
        // anchor 夹紧到文档末（"hi" 只有 1 行）
        let sel = e.view().selection();
        assert_eq!(sel.anchor.line, 0);
        assert!(sel.anchor.byte <= 2);
    }
}
