//! 终端单元格：字符 + 颜色 + 样式。

use std::num::NonZeroU32;

use bitflags::bitflags;

/// 单元格颜色。具体 RGB 解析（调色板/主题）由渲染层负责。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// 默认前景/背景（随主题）。
    #[default]
    Default,
    /// 256 色索引（0-15 为 ANSI 基础色）。
    Indexed(u8),
    /// 24 位真彩。
    Rgb(u8, u8, u8),
}

bitflags! {
    /// 单元格样式标志。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct CellFlags: u8 {
        const BOLD        = 1 << 0;
        const ITALIC      = 1 << 1;
        const UNDERLINE   = 1 << 2;
        const INVERSE     = 1 << 3;
        const DIM         = 1 << 4;
        const STRIKE      = 1 << 5;
        /// 宽字符（东亚全角）主格，占两列。
        const WIDE        = 1 << 6;
        /// 宽字符的右半占位格，渲染时跳过。
        const WIDE_SPACER = 1 << 7;
    }
}

/// 一个屏幕单元格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
    /// OSC 8 显式超链接的内部 id（F10）：`Some(id)` 时本格属于一个由
    /// `ESC]8;;URI` 开启的超链接区段，id 映射到终端持有的 URI 表
    /// （见 [`crate::Terminal::hyperlink_at`]）。隐式链接（裸 URL /
    /// 文件路径）不写此字段，由上层在 hover 时按行文本扫描识别。
    /// 用 `NonZeroU32` 借空指针优化：`Option` 不额外占字节（id 从 1 起）。
    pub hyperlink_id: Option<NonZeroU32>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            flags: CellFlags::empty(),
            hyperlink_id: None,
        }
    }
}

impl Cell {
    /// 是否完全空白（用于渲染层跳过）。
    pub fn is_blank(&self) -> bool {
        self.ch == ' ' && self.bg == Color::Default && !self.flags.contains(CellFlags::INVERSE)
    }
}
