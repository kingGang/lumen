//! 光标位置（[`Position`]）与选区（[`Selection`]）。
//!
//! 坐标单位说明：
//! - `line`：行下标（0-based，`Vec<String>` 索引）
//! - `byte`：行内字节偏移（0-based，`String::len()` 为行尾哨兵值）
//!
//! 移动以 **grapheme cluster** 为最小单位，宽字符（CJK、emoji）在显示宽度上
//! 占 2 列——Up/Down 跨行时用显示列记忆列（`goal_col`）保持视觉对齐。

use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// 文档坐标：行号 + 行内字节偏移。
///
/// `byte` 始终是合法 UTF-8 字符边界（不会落在多字节字符中间）。
/// 越界值由 [`crate::Editor::apply`] 夹紧，调用方无需自行检查。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub struct Position {
    /// 行下标（0-based）。
    pub line: usize,
    /// 行内字节偏移（0-based；等于行长度时表示行尾）。
    pub byte: usize,
}

impl Position {
    /// 构造坐标；调用方负责传入合法值（库内部使用）。
    pub(crate) fn new(line: usize, byte: usize) -> Self {
        Self { line, byte }
    }
}

/// 文本选区：`anchor`（锚点）+ `cursor`（活动端）。
///
/// `anchor == cursor` 即纯光标（无选区）。
/// `anchor < cursor` 时为顺向选区；`anchor > cursor` 时为逆向选区。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Selection {
    /// 选区固定端（Shift 按住时不移动的一端）。
    pub anchor: Position,
    /// 选区活动端（随光标移动的一端）。
    pub cursor: Position,
}

impl Selection {
    /// 纯光标（无选区）。
    pub fn caret(pos: Position) -> Self {
        Self {
            anchor: pos,
            cursor: pos,
        }
    }

    /// 返回选区有序端点 `(start, end)`，`start <= end`。
    pub fn ordered(&self) -> (Position, Position) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// 是否为纯光标（锚点 == 活动端）。
    pub fn is_caret(&self) -> bool {
        self.anchor == self.cursor
    }
}

// ─── grapheme 工具函数（库内部使用）──────────────────────────────────────────

/// 返回字符串中字节偏移 `byte` **左侧**一个 grapheme cluster 的起始字节偏移。
/// 若已在行首（`byte == 0`）返回 `0`。
pub(crate) fn prev_grapheme_boundary(s: &str, byte: usize) -> usize {
    let byte = byte.min(s.len());
    let prefix = &s[..byte];
    // 倒序迭代 grapheme，取最后一个的起始位置
    prefix
        .grapheme_indices(true)
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// 返回字符串中字节偏移 `byte` **右侧**一个 grapheme cluster 后的字节偏移。
/// 若已在行尾返回 `s.len()`。
pub(crate) fn next_grapheme_boundary(s: &str, byte: usize) -> usize {
    let byte = byte.min(s.len());
    let suffix = &s[byte..];
    let mut iter = suffix.grapheme_indices(true);
    match iter.next() {
        None => s.len(),
        Some((_, g)) => byte + g.len(),
    }
}

/// 返回行内字节偏移 `byte` 处的显示列（宽字符占 2 列）。
pub(crate) fn byte_to_display_col(s: &str, byte: usize) -> usize {
    let byte = byte.min(s.len());
    UnicodeWidthStr::width(&s[..byte])
}

/// 返回最接近显示列 `col` 的字节偏移（不超过行长）。
///
/// 对于宽字符（如 CJK 占 2 列），`col` 落在字符内部时返回该字符的起始字节偏移。
/// 例：`"中文"` 中 `col=1` 落在第一个汉字（列 0-1）内部，返回 `0`。
///
/// # Examples
/// ```
/// use lumen_editor::display_col_to_byte;
/// assert_eq!(display_col_to_byte("hello", 2), 2);
/// assert_eq!(display_col_to_byte("中文", 2), 3); // 第二个汉字起始
/// ```
pub fn display_col_to_byte(s: &str, col: usize) -> usize {
    let mut acc = 0usize;
    for (i, g) in s.grapheme_indices(true) {
        let w = UnicodeWidthStr::width(g);
        // 列 col 落在本 grapheme 的显示范围 [acc, acc+w) 内
        if col < acc + w {
            return i;
        }
        acc += w;
        if acc == col {
            // 精确对齐到本 grapheme 右边界
            return i + g.len();
        }
    }
    s.len()
}

/// 向左查找词边界（空白/字母数字/符号三类切换处）。
/// 返回应跳到的字节偏移。
///
/// CJK 连续字符属于同一类别（字母数字），视为同一词。
pub fn word_start_left(s: &str, byte: usize) -> usize {
    let byte = byte.min(s.len());
    let prefix = &s[..byte];
    // 按 grapheme 逆序遍历，跳过同类别字符
    let graphemes: Vec<(usize, &str)> = prefix.grapheme_indices(true).collect();
    if graphemes.is_empty() {
        return 0;
    }
    let start_cat = match graphemes.last() {
        Some((_, g)) => char_category(g),
        None => return 0,
    };
    let mut result = 0usize;
    for (i, g) in graphemes.iter().rev() {
        if char_category(g) != start_cat {
            result = i + g.len();
            return result;
        }
        result = *i;
    }
    result
}

/// 向右查找词边界。返回应跳到的字节偏移。
///
/// CJK 连续字符属于同一类别（字母数字），视为同一词。
pub fn word_end_right(s: &str, byte: usize) -> usize {
    let byte = byte.min(s.len());
    let suffix = &s[byte..];
    let graphemes: Vec<(usize, &str)> = suffix.grapheme_indices(true).collect();
    if graphemes.is_empty() {
        return s.len();
    }
    let start_cat = match graphemes.first() {
        Some((_, g)) => char_category(g),
        None => return s.len(),
    };
    for (i, g) in graphemes.iter() {
        if char_category(g) != start_cat {
            return byte + i;
        }
    }
    s.len()
}

/// 字符分类（词边界判断）：0=空白，1=字母数字，2=其他符号。
fn char_category(g: &str) -> u8 {
    match g.chars().next() {
        None => 0,
        Some(c) if c.is_whitespace() => 0,
        Some(c) if c.is_alphanumeric() || c == '_' => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prev_grapheme_boundary_ascii() {
        let s = "hello";
        assert_eq!(prev_grapheme_boundary(s, 3), 2);
        assert_eq!(prev_grapheme_boundary(s, 0), 0);
    }

    #[test]
    fn test_next_grapheme_boundary_ascii() {
        let s = "hello";
        assert_eq!(next_grapheme_boundary(s, 0), 1);
        assert_eq!(next_grapheme_boundary(s, 5), 5);
    }

    #[test]
    fn test_grapheme_boundary_cjk() {
        // 中文每字 3 字节 UTF-8
        let s = "中文";
        assert_eq!(next_grapheme_boundary(s, 0), 3);
        assert_eq!(prev_grapheme_boundary(s, 6), 3);
        assert_eq!(prev_grapheme_boundary(s, 3), 0);
    }

    #[test]
    fn test_grapheme_boundary_emoji_zwj() {
        // 👨‍👩‍👧‍👦 是 ZWJ 序列，应作一个 grapheme
        let s = "👨‍👩‍👧‍👦";
        let len = s.len();
        assert_eq!(next_grapheme_boundary(s, 0), len);
        assert_eq!(prev_grapheme_boundary(s, len), 0);
    }

    #[test]
    fn test_grapheme_boundary_combining() {
        // é = e（1字节）+ U+0301 COMBINING ACUTE ACCENT（2字节）= 3字节，一个 grapheme
        let s = "e\u{0301}x"; // é(3字节) + x(1字节)
                              // é 是一个 grapheme，占 3 字节；x 从字节 3 开始
        assert_eq!(next_grapheme_boundary(s, 0), 3);
        assert_eq!(prev_grapheme_boundary(s, 3), 0);
    }

    #[test]
    fn test_display_col_cjk() {
        let s = "中文abc";
        assert_eq!(byte_to_display_col(s, 6), 4); // 两个 CJK 各占 2 列
        assert_eq!(byte_to_display_col(s, 9), 7); // + "abc"
    }

    #[test]
    fn test_display_col_to_byte_cjk() {
        let s = "中文abc";
        // 列 0 → 字节 0
        assert_eq!(display_col_to_byte(s, 0), 0);
        // 列 2 → 字节 3（第二个汉字）
        assert_eq!(display_col_to_byte(s, 2), 3);
        // 列 4 → 字节 6（ascii 'a'）
        assert_eq!(display_col_to_byte(s, 4), 6);
    }

    #[test]
    fn test_word_boundary_left() {
        let s = "hello world";
        // 从 "d" 之后（字节 11）向左跳词 → 跳到 "world" 起始（字节 6）
        assert_eq!(word_start_left(s, 11), 6);
        // 从 "o" 之后（字节 5）向左跳词 → 跳到 "hello" 起始（字节 0）
        assert_eq!(word_start_left(s, 5), 0);
    }

    #[test]
    fn test_word_boundary_right() {
        let s = "hello world";
        // 从字节 0 向右跳词 → 跳到空格（字节 5）
        assert_eq!(word_end_right(s, 0), 5);
        // 从空格（字节 5）向右跳词 → 跳到 "world" 起始（字节 6）
        assert_eq!(word_end_right(s, 5), 6);
    }

    #[test]
    fn test_selection_ordered() {
        let a = Position::new(0, 3);
        let b = Position::new(1, 0);
        let sel = Selection {
            anchor: b,
            cursor: a,
        };
        let (s, e) = sel.ordered();
        assert_eq!(s, a);
        assert_eq!(e, b);
    }

    #[test]
    fn test_selection_is_caret() {
        let p = Position::new(2, 4);
        assert!(Selection::caret(p).is_caret());
        let sel = Selection {
            anchor: p,
            cursor: Position::new(2, 5),
        };
        assert!(!sel.is_caret());
    }
}
