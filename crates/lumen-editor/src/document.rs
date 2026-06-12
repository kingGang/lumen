//! 文档模型：行数组存储（[`Document`]）。
//!
//! 命令行输入 <10KB，`Vec<String>` 是正确的选型（rope/SumTree 属过度设计，
//! 设计稿 §12 明确禁止）。接口不暴露存储形态，上层只通过方法访问内容。
//!
//! 所有变更方法均由 [`crate::Editor`] 通过 [`crate::Editor::apply`] 间接调用，
//! **不对外暴露可变引用**。

use crate::cursor::{
    byte_to_display_col, display_col_to_byte, next_grapheme_boundary, prev_grapheme_boundary,
    word_end_right, word_start_left, Position,
};

/// 文档行数组。
///
/// 不变量：
/// - `lines` 至少含 1 个元素（空文档 = `[""]`）。
/// - 每个 `String` 不含 `'\n'`（换行以行分割体现）。
#[derive(Debug, Clone)]
pub(crate) struct Document {
    pub(crate) lines: Vec<String>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
        }
    }
}

impl Document {
    /// 行数（>= 1）。
    pub(crate) fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// 获取指定行（越界时返回空串引用）。
    pub(crate) fn line(&self, row: usize) -> &str {
        if row < self.lines.len() {
            &self.lines[row]
        } else {
            ""
        }
    }

    /// 全文内容（行间插入 `\n`）。
    pub(crate) fn to_string_full(&self) -> String {
        self.lines.join("\n")
    }

    /// 整体替换内容；`\n` 分割为多行。
    pub(crate) fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(|s| s.to_string()).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
    }

    /// 夹紧坐标到合法范围（不 panic）。
    pub(crate) fn clamp(&self, pos: Position) -> Position {
        let line = pos.line.min(self.lines.len().saturating_sub(1));
        let byte = pos.byte.min(self.lines[line].len());
        // 夹紧到合法 UTF-8 字符边界
        let byte = snap_to_char_boundary(&self.lines[line], byte);
        Position::new(line, byte)
    }

    // ─── 插入 ────────────────────────────────────────────────────────────────

    /// 在 `pos` 处插入文本（可含 `\n`）；返回插入后光标位置。
    pub(crate) fn insert_text(&mut self, pos: Position, text: &str) -> Position {
        let pos = self.clamp(pos);
        if text.is_empty() {
            return pos;
        }
        let mut parts: Vec<&str> = text.split('\n').collect();
        if parts.is_empty() {
            return pos;
        }

        let row = pos.line;
        let byte = pos.byte;
        let original_line = self.lines[row].clone();
        let prefix = &original_line[..byte];
        let suffix = &original_line[byte..];

        if parts.len() == 1 {
            // 单行插入
            self.lines[row] = format!("{}{}{}", prefix, parts[0], suffix);
            Position::new(row, byte + parts[0].len())
        } else {
            // 多行插入
            let first = format!("{}{}", prefix, parts[0]);
            let last_part = parts.pop().unwrap_or("");
            let new_cursor_byte = last_part.len();
            let last = format!("{}{}", last_part, suffix);

            self.lines[row] = first;
            let insert_row = row + 1;
            // 中间行
            for (i, &part) in parts[1..].iter().enumerate() {
                self.lines.insert(insert_row + i, part.to_string());
            }
            let last_row = insert_row + parts.len() - 1;
            self.lines.insert(last_row, last);

            Position::new(last_row, new_cursor_byte)
        }
    }

    /// 在 `pos` 处插入换行；返回新行首位置。
    pub(crate) fn insert_newline(&mut self, pos: Position) -> Position {
        let pos = self.clamp(pos);
        let row = pos.line;
        let byte = pos.byte;
        let rest = self.lines[row][byte..].to_string();
        self.lines[row].truncate(byte);
        self.lines.insert(row + 1, rest);
        Position::new(row + 1, 0)
    }

    // ─── 删除 ────────────────────────────────────────────────────────────────

    /// 删除 `[start, end)` 区间的内容；返回合并后的光标位置（start）。
    ///
    /// 跨行删除时，start 行 suffix 与 end 行的剩余拼接。
    pub(crate) fn delete_range(&mut self, start: Position, end: Position) -> Position {
        let start = self.clamp(start);
        let end = self.clamp(end);
        if start >= end {
            return start;
        }
        if start.line == end.line {
            // 同行删除
            self.lines[start.line].drain(start.byte..end.byte);
            return start;
        }
        // 跨行：保留 start 行前缀 + end 行后缀
        let end_suffix = self.lines[end.line][end.byte..].to_string();
        self.lines[start.line].truncate(start.byte);
        self.lines[start.line].push_str(&end_suffix);
        // 删除中间行及 end 行
        self.lines.drain((start.line + 1)..=(end.line));
        start
    }

    /// 删除光标左侧一个 grapheme cluster；返回新光标位置。
    pub(crate) fn delete_backward(&mut self, pos: Position) -> Position {
        let pos = self.clamp(pos);
        if pos.byte > 0 {
            let prev = prev_grapheme_boundary(&self.lines[pos.line], pos.byte);
            let end = pos;
            let start = Position::new(pos.line, prev);
            self.delete_range(start, end)
        } else if pos.line > 0 {
            // 行首：与上一行合并
            let prev_row = pos.line - 1;
            let prev_len = self.lines[prev_row].len();
            let current = self.lines[pos.line].clone();
            self.lines[prev_row].push_str(&current);
            self.lines.remove(pos.line);
            Position::new(prev_row, prev_len)
        } else {
            pos // 文档首，无操作
        }
    }

    /// 删除光标右侧一个 grapheme cluster；返回新光标位置（不变）。
    pub(crate) fn delete_forward(&mut self, pos: Position) -> Position {
        let pos = self.clamp(pos);
        let line_len = self.lines[pos.line].len();
        if pos.byte < line_len {
            let next = next_grapheme_boundary(&self.lines[pos.line], pos.byte);
            let start = pos;
            let end = Position::new(pos.line, next);
            self.delete_range(start, end)
        } else if pos.line + 1 < self.lines.len() {
            // 行尾：与下一行合并
            let next_row = pos.line + 1;
            let next_content = self.lines[next_row].clone();
            self.lines[pos.line].push_str(&next_content);
            self.lines.remove(next_row);
            pos
        } else {
            pos // 文档末，无操作
        }
    }

    /// 向左删除一词；返回新光标位置。
    pub(crate) fn delete_word_backward(&mut self, pos: Position) -> Position {
        let pos = self.clamp(pos);
        if pos.byte == 0 && pos.line == 0 {
            return pos;
        }
        if pos.byte == 0 {
            // 行首：与上一行合并（等价于 DeleteBackward）
            return self.delete_backward(pos);
        }
        let word_start = word_start_left(&self.lines[pos.line], pos.byte);
        let start = Position::new(pos.line, word_start);
        self.delete_range(start, pos)
    }

    // ─── 光标移动（纯查询，不修改文档）──────────────────────────────────────

    /// 向左移动一个 grapheme；返回新位置。
    pub(crate) fn move_grapheme_left(&self, pos: Position) -> Position {
        let pos = self.clamp(pos);
        if pos.byte > 0 {
            let prev = prev_grapheme_boundary(&self.lines[pos.line], pos.byte);
            Position::new(pos.line, prev)
        } else if pos.line > 0 {
            Position::new(pos.line - 1, self.lines[pos.line - 1].len())
        } else {
            pos
        }
    }

    /// 向右移动一个 grapheme；返回新位置。
    pub(crate) fn move_grapheme_right(&self, pos: Position) -> Position {
        let pos = self.clamp(pos);
        let line_len = self.lines[pos.line].len();
        if pos.byte < line_len {
            let next = next_grapheme_boundary(&self.lines[pos.line], pos.byte);
            Position::new(pos.line, next)
        } else if pos.line + 1 < self.lines.len() {
            Position::new(pos.line + 1, 0)
        } else {
            pos
        }
    }

    /// 向左跳一词；返回新位置。
    pub(crate) fn move_word_left(&self, pos: Position) -> Position {
        let pos = self.clamp(pos);
        if pos.byte == 0 {
            if pos.line > 0 {
                Position::new(pos.line - 1, self.lines[pos.line - 1].len())
            } else {
                pos
            }
        } else {
            let b = word_start_left(&self.lines[pos.line], pos.byte);
            Position::new(pos.line, b)
        }
    }

    /// 向右跳一词；返回新位置。
    pub(crate) fn move_word_right(&self, pos: Position) -> Position {
        let pos = self.clamp(pos);
        let line_len = self.lines[pos.line].len();
        if pos.byte >= line_len {
            if pos.line + 1 < self.lines.len() {
                Position::new(pos.line + 1, 0)
            } else {
                pos
            }
        } else {
            let b = word_end_right(&self.lines[pos.line], pos.byte);
            Position::new(pos.line, b)
        }
    }

    /// 移动到行首。
    pub(crate) fn move_line_start(&self, pos: Position) -> Position {
        let pos = self.clamp(pos);
        Position::new(pos.line, 0)
    }

    /// 移动到行尾。
    pub(crate) fn move_line_end(&self, pos: Position) -> Position {
        let pos = self.clamp(pos);
        Position::new(pos.line, self.lines[pos.line].len())
    }

    /// 向上移动一行，保持目标显示列。
    /// `goal_col` 为 `None` 时用当前列；返回 `(新位置, 新 goal_col)`。
    pub(crate) fn move_up(&self, pos: Position, goal_col: Option<usize>) -> (Position, usize) {
        let pos = self.clamp(pos);
        let col = goal_col.unwrap_or_else(|| byte_to_display_col(&self.lines[pos.line], pos.byte));
        if pos.line == 0 {
            // 已在首行：跳到行首
            return (Position::new(0, 0), col);
        }
        let new_row = pos.line - 1;
        let new_byte = display_col_to_byte(&self.lines[new_row], col);
        (Position::new(new_row, new_byte), col)
    }

    /// 向下移动一行，保持目标显示列。
    pub(crate) fn move_down(&self, pos: Position, goal_col: Option<usize>) -> (Position, usize) {
        let pos = self.clamp(pos);
        let col = goal_col.unwrap_or_else(|| byte_to_display_col(&self.lines[pos.line], pos.byte));
        let last_row = self.lines.len() - 1;
        if pos.line >= last_row {
            // 已在末行：跳到行尾
            return (Position::new(last_row, self.lines[last_row].len()), col);
        }
        let new_row = pos.line + 1;
        let new_byte = display_col_to_byte(&self.lines[new_row], col);
        (Position::new(new_row, new_byte), col)
    }

    /// 跳到文档首。
    pub(crate) fn move_doc_start(&self) -> Position {
        Position::new(0, 0)
    }

    /// 跳到文档末。
    pub(crate) fn move_doc_end(&self) -> Position {
        let last = self.lines.len() - 1;
        Position::new(last, self.lines[last].len())
    }
}

/// 将字节偏移夹紧到合法 UTF-8 字符边界（向左夹）。
fn snap_to_char_boundary(s: &str, byte: usize) -> usize {
    let byte = byte.min(s.len());
    // 从 byte 向左找第一个合法字符边界
    let mut b = byte;
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Document {
        let mut d = Document::default();
        d.set_text(text);
        d
    }

    #[test]
    fn test_insert_text_单行() {
        let mut d = doc("hello");
        let pos = d.insert_text(Position::new(0, 5), " world");
        assert_eq!(d.line(0), "hello world");
        assert_eq!(pos, Position::new(0, 11));
    }

    #[test]
    fn test_insert_text_含换行() {
        let mut d = doc("ab");
        let pos = d.insert_text(Position::new(0, 1), "x\ny");
        // 结果：["ax", "yb"]
        assert_eq!(d.line(0), "ax");
        assert_eq!(d.line(1), "yb");
        assert_eq!(pos, Position::new(1, 1));
    }

    #[test]
    fn test_insert_newline() {
        let mut d = doc("hello world");
        let pos = d.insert_newline(Position::new(0, 5));
        assert_eq!(d.line(0), "hello");
        assert_eq!(d.line(1), " world");
        assert_eq!(pos, Position::new(1, 0));
    }

    #[test]
    fn test_delete_backward_ascii() {
        let mut d = doc("hello");
        let pos = d.delete_backward(Position::new(0, 5));
        assert_eq!(d.line(0), "hell");
        assert_eq!(pos.byte, 4);
    }

    #[test]
    fn test_delete_backward_cjk() {
        let mut d = doc("中文");
        // "中文" = 6 字节，删左侧 grapheme（3字节）
        let pos = d.delete_backward(Position::new(0, 6));
        assert_eq!(d.line(0), "中");
        assert_eq!(pos.byte, 3);
    }

    #[test]
    fn test_delete_backward_跨行合并() {
        let mut d = doc("abc\ndef");
        // 行首 delete_backward → 与上行合并
        let pos = d.delete_backward(Position::new(1, 0));
        assert_eq!(d.line_count(), 1);
        assert_eq!(d.line(0), "abcdef");
        assert_eq!(pos, Position::new(0, 3));
    }

    #[test]
    fn test_delete_forward_行尾合并() {
        let mut d = doc("abc\ndef");
        // 行尾 delete_forward → 与下行合并
        let pos = d.delete_forward(Position::new(0, 3));
        assert_eq!(d.line_count(), 1);
        assert_eq!(d.line(0), "abcdef");
        assert_eq!(pos, Position::new(0, 3));
    }

    #[test]
    fn test_delete_range_跨行() {
        let mut d = doc("abc\ndefg\nhij");
        let start = Position::new(0, 1);
        let end = Position::new(1, 3);
        let pos = d.delete_range(start, end);
        // 结果：["ag", "hij"]（不对——应是 a + "g\nhij" 的 end 后缀）
        // start(0,1) → prefix="a"，end(1,3) → suffix="g"
        assert_eq!(d.line(0), "ag");
        assert_eq!(d.line(1), "hij");
        assert_eq!(pos, Position::new(0, 1));
    }

    #[test]
    fn test_delete_word_backward() {
        let mut d = doc("hello world");
        let pos = d.delete_word_backward(Position::new(0, 11));
        assert_eq!(d.line(0), "hello ");
        assert_eq!(pos, Position::new(0, 6));
    }

    #[test]
    fn test_move_up_down_目标列记忆() {
        let d = doc("hello\nhi");
        // 从 (0, 3) 向下：目标列 3，行 "hi" 只有 2 列，夹紧到末尾
        let (down_pos, goal) = d.move_down(Position::new(0, 3), None);
        assert_eq!(down_pos.line, 1);
        assert_eq!(goal, 3);
        // 然后向上回来：目标列 3，行 "hello" 列 3 = 字节 3
        let (up_pos, _) = d.move_up(down_pos, Some(goal));
        assert_eq!(up_pos, Position::new(0, 3));
    }

    #[test]
    fn test_move_up_down_cjk目标列() {
        // "中文" 各 2 列；"ab" 各 1 列
        let d = doc("中文\nab");
        // 从 (0, 3)（第二个汉字起始，显示列 2）向下
        let (pos, goal) = d.move_down(Position::new(0, 3), None);
        assert_eq!(goal, 2); // 显示列 2
                             // "ab" 列 2 → 字节 2（末尾）
        assert_eq!(pos, Position::new(1, 2));
    }

    #[test]
    fn test_clamp_越界() {
        let d = doc("hi");
        let p = d.clamp(Position::new(99, 999));
        assert_eq!(p, Position::new(0, 2));
    }

    #[test]
    fn test_to_string_full() {
        let d = doc("a\nb\nc");
        assert_eq!(d.to_string_full(), "a\nb\nc");
    }
}
