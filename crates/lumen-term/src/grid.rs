//! 终端网格：可视区 cell 矩阵 + scrollback 历史。

use std::collections::VecDeque;

use crate::cell::{Cell, CellFlags, Color};

/// 一行单元格。
#[derive(Debug, Clone)]
pub struct Row {
    cells: Vec<Cell>,
}

impl Row {
    pub fn new(cols: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cols],
        }
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// 调整列数：截断或以空白格扩展。
    fn resize(&mut self, cols: usize) {
        self.cells.resize(cols, Cell::default());
    }

    fn fill(&mut self, cell: Cell) {
        self.cells.fill(cell);
    }

    /// 整行是否纯空白（resize 缩行的收割判定）：所有格子无字符、无
    /// 背景色、无会上屏的样式（反显/下划线/删除线）。空格带前景色
    /// 视为空白——空格不渲染字形，前景色无可视效果。
    fn is_blank(&self) -> bool {
        self.cells.iter().all(|c| {
            c.ch == ' '
                && c.bg == Color::Default
                && !c
                    .flags
                    .intersects(CellFlags::INVERSE | CellFlags::UNDERLINE | CellFlags::STRIKE)
        })
    }
}

impl std::ops::Index<usize> for Row {
    type Output = Cell;
    fn index(&self, col: usize) -> &Cell {
        &self.cells[col]
    }
}

impl std::ops::IndexMut<usize> for Row {
    fn index_mut(&mut self, col: usize) -> &mut Cell {
        &mut self.cells[col]
    }
}

/// 光标位置与状态。
#[derive(Debug, Clone, Copy, Default)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    /// 写满最后一列后置位；下个可见字符到来时才真正换行（DEC 延迟换行语义）。
    pub pending_wrap: bool,
    pub visible: bool,
}

/// 终端网格：`rows x cols` 可视区 + scrollback。
///
/// 行号约定：可视区内 0 = 顶行；scrollback 单独存放，
/// 渲染时通过 [`Grid::visible_rows`] 按用户滚动偏移取行。
#[derive(Debug)]
pub struct Grid {
    rows: usize,
    cols: usize,
    /// 可视区，长度恒等于 rows。
    screen: VecDeque<Row>,
    /// 滚出可视区的历史行，前端最旧。
    scrollback: VecDeque<Row>,
    scrollback_limit: usize,
    pub cursor: Cursor,
    /// 用户向上滚动的行数（0 = 跟随底部）。
    display_offset: usize,
    /// scrollback 超限被丢弃的行数（绝对行号基准）。
    dropped_lines: u64,
    /// 自上次渲染以来内容是否变化。
    dirty: bool,
}

impl Grid {
    pub fn new(rows: usize, cols: usize, scrollback_limit: usize) -> Self {
        let screen = (0..rows).map(|_| Row::new(cols)).collect();
        Self {
            rows,
            cols,
            screen,
            scrollback: VecDeque::new(),
            scrollback_limit,
            cursor: Cursor {
                visible: true,
                ..Cursor::default()
            },
            display_offset: 0,
            dropped_lines: 0,
            dirty: true,
        }
    }

    /// 光标所在行的绝对行号（含全部历史，跨滚动稳定），Block 边界用。
    pub fn absolute_cursor_line(&self) -> u64 {
        self.dropped_lines + self.scrollback.len() as u64 + self.cursor.row as u64
    }

    /// 当前视图首行的绝对行号（选区渲染用）。
    pub fn view_top_abs_line(&self) -> u64 {
        let n = self.display_offset.min(self.scrollback.len());
        self.dropped_lines + (self.scrollback.len() - n) as u64
    }

    /// 按绝对行号取行（历史或可视区）；已被丢弃或越界返回 None。
    pub fn line_by_abs(&self, abs: u64) -> Option<&Row> {
        let idx = abs.checked_sub(self.dropped_lines)? as usize;
        if idx < self.scrollback.len() {
            self.scrollback.get(idx)
        } else {
            self.screen.get(idx - self.scrollback.len())
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    /// 用户滚动：正数向上（看历史），负数向下。会自动夹紧范围。
    pub fn scroll_display(&mut self, delta: isize) {
        let max = self.scrollback.len();
        let new = (self.display_offset as isize + delta).clamp(0, max as isize) as usize;
        if new != self.display_offset {
            self.display_offset = new;
            self.dirty = true;
        }
    }

    /// 回到底部（有新输出时调用）。
    pub fn scroll_to_bottom(&mut self) {
        if self.display_offset != 0 {
            self.display_offset = 0;
            self.dirty = true;
        }
    }

    /// 滚动视图使指定绝对行位于视口顶部（块跳转用）。
    /// 目标行已被丢弃时滚到最旧历史；在可视区内或以下时回到底部。
    pub fn scroll_to_abs_line(&mut self, line: u64) {
        let top_of_screen = self.dropped_lines + self.scrollback.len() as u64;
        let offset = top_of_screen.saturating_sub(line) as usize;
        let new = offset.min(self.scrollback.len());
        if new != self.display_offset {
            self.display_offset = new;
            self.dirty = true;
        }
    }

    /// 渲染用：按当前滚动偏移返回应显示的 rows 行。
    ///
    /// 偏移 N 表示视口顶部往上 N 行历史，视口由
    /// 「scrollback 尾部 N 行 + 可视区前 rows-N 行」拼成。
    pub fn visible_rows(&self) -> impl Iterator<Item = &Row> {
        let n = self.display_offset.min(self.scrollback.len());
        let from_history = self.scrollback.iter().skip(self.scrollback.len() - n);
        let from_screen = self.screen.iter().take(self.rows - n.min(self.rows));
        from_history.chain(from_screen).take(self.rows)
    }

    pub fn row(&self, r: usize) -> &Row {
        &self.screen[r]
    }

    pub fn row_mut(&mut self, r: usize) -> &mut Row {
        self.dirty = true;
        &mut self.screen[r]
    }

    pub fn cell_mut(&mut self, r: usize, c: usize) -> &mut Cell {
        self.dirty = true;
        &mut self.screen[r][c]
    }

    /// 可视区整体上滚一行（顶行进 scrollback，底部补空行）。
    pub fn scroll_up_one(&mut self, keep_history: bool) {
        if let Some(top) = self.screen.pop_front() {
            if keep_history {
                self.scrollback.push_back(top);
                if self.scrollback.len() > self.scrollback_limit {
                    self.scrollback.pop_front();
                    self.dropped_lines += 1;
                }
                // 用户正在回看历史时锚定内容：偏移随历史增长同步
                // +1，视图不会被新输出推着走。
                if self.display_offset > 0 {
                    self.display_offset = (self.display_offset + 1).min(self.scrollback.len());
                }
            } else {
                self.dropped_lines += 1;
            }
        }
        self.screen.push_back(Row::new(self.cols));
        self.dirty = true;
    }

    /// 滚动区 `[top, bottom]`（含端点）内上滚 n 行，区外不动，不进历史。
    pub fn scroll_region_up(&mut self, top: usize, bottom: usize, n: usize) {
        let bottom = bottom.min(self.rows - 1);
        if top > bottom {
            return;
        }
        for _ in 0..n {
            self.screen.remove(top);
            self.screen.insert(bottom, Row::new(self.cols));
        }
        self.dirty = true;
    }

    /// 滚动区 `[top, bottom]` 内下滚 n 行（顶部插入空行）。
    pub fn scroll_region_down(&mut self, top: usize, bottom: usize, n: usize) {
        let bottom = bottom.min(self.rows - 1);
        if top > bottom {
            return;
        }
        for _ in 0..n {
            self.screen.remove(bottom);
            self.screen.insert(top, Row::new(self.cols));
        }
        self.dirty = true;
    }

    /// 用指定 cell 填充矩形区域（含端点），用于 ED/EL 擦除。
    pub fn fill_region(&mut self, r0: usize, c0: usize, r1: usize, c1: usize, cell: Cell) {
        for r in r0..=r1.min(self.rows - 1) {
            let row = &mut self.screen[r];
            for c in c0..=c1.min(self.cols - 1) {
                row[c] = cell;
            }
        }
        self.dirty = true;
    }

    /// 整屏填充。
    pub fn fill_all(&mut self, cell: Cell) {
        for row in &mut self.screen {
            row.fill(cell);
        }
        self.dirty = true;
    }

    /// 调整可视区尺寸。列直接截断/扩展，不做 reflow；缩小行数时
    /// **优先丢弃光标行以下的纯空行**，不足时才把顶行滚入历史
    /// （alacritty/wezterm 同语义）。
    ///
    /// 收割空行这一步是 B2 多窗格回归（症状①「窗格只剩左缘两根竖
    /// 条、提示符消失」）的根治：旧实现无条件从顶搬行进 scrollback，
    /// 新鲜提示符窗格（内容只有顶部一行、下方全空）缩行后提示符行
    /// 被搬进历史、可视区只剩空行——而命令块状态条与光标是按元数据
    /// 另行绘制的，画面就退化成「两根竖条 + 正文全空」；shell 空闲
    /// 不重印提示符，若 ConPTY 的 resize 重绘缺失/被后续缩行打断，
    /// 空屏便永久定格。M3.7c/d 的窗格标题栏占高、恢复路径整区估算
    /// spawn、分隔条逐帧 resize 把缩行从罕见事件变成常态，引爆了该
    /// M1 时代的潜伏缺陷。
    pub fn resize(&mut self, rows: usize, cols: usize) {
        if rows == self.rows && cols == self.cols {
            return;
        }
        for row in &mut self.screen {
            row.resize(cols);
        }
        while self.screen.len() < rows {
            self.screen.push_back(Row::new(cols));
        }
        // 缩小行数，第一步：收割光标行**以下**的纯空行（直接丢弃、
        // 不进历史——空行入历史会让 scrollback 凭空多出空白段）。
        while self.screen.len() > rows
            && self.screen.len() - 1 > self.cursor.row
            && self.screen.back().is_some_and(Row::is_blank)
        {
            self.screen.pop_back();
        }
        // 第二步：仍超出时顶行滚入历史，尽量保留光标附近内容。超限
        // 丢弃必须同步推进 dropped_lines（绝对行号基准，与
        // scroll_up_one 一致）——漏计会让命令块/选区的绝对行号整体
        // 错位一行。
        while self.screen.len() > rows {
            if let Some(top) = self.screen.pop_front() {
                self.scrollback.push_back(top);
                if self.scrollback.len() > self.scrollback_limit {
                    self.scrollback.pop_front();
                    self.dropped_lines += 1;
                }
            }
            if self.cursor.row > 0 {
                self.cursor.row -= 1;
            }
        }
        self.rows = rows;
        self.cols = cols;
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.cursor.pending_wrap = false;
        self.display_offset = 0;
        self.dirty = true;
    }

    /// 取走脏标记（渲染前调用，返回是否需要重绘）。
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 滚动一行后顶行进入历史() {
        let mut g = Grid::new(3, 4, 100);
        g.cell_mut(0, 0).ch = 'a';
        g.scroll_up_one(true);
        assert_eq!(g.scrollback_len(), 1);
        assert_eq!(g.row(0)[0].ch, ' ');
    }

    #[test]
    fn 历史超限后丢弃最旧行() {
        let mut g = Grid::new(2, 2, 3);
        for _ in 0..10 {
            g.scroll_up_one(true);
        }
        assert_eq!(g.scrollback_len(), 3);
    }

    #[test]
    fn 用户滚动偏移夹紧在历史范围内() {
        let mut g = Grid::new(2, 2, 100);
        g.scroll_up_one(true);
        g.scroll_up_one(true);
        g.scroll_display(99);
        assert_eq!(g.display_offset(), 2);
        g.scroll_display(-99);
        assert_eq!(g.display_offset(), 0);
    }

    #[test]
    fn 缩行优先丢弃光标下方空行() {
        // B2 症状① 回归测试。模拟新鲜提示符窗格：提示符在顶行、
        // 光标停在同行、下方全空——缩行必须收割下方空行，而不是
        // 把提示符行搬进历史（旧实现的行为，可视区只剩空行）。
        let mut g = Grid::new(30, 20, 100);
        for (i, ch) in "PS>".chars().enumerate() {
            g.cell_mut(0, i).ch = ch;
        }
        g.cursor.row = 0;
        g.cursor.col = 4;
        g.resize(12, 20);
        assert_eq!(g.scrollback_len(), 0);
        assert_eq!(g.row(0)[0].ch, 'P');
        assert_eq!((g.cursor.row, g.cursor.col), (0, 4));
        // 绝对行号不漂移（命令块标记按绝对行号定位）。
        assert_eq!(g.absolute_cursor_line(), 0);
    }

    #[test]
    fn 缩行空行不足时顶行进历史() {
        // 0..=4 行有内容、光标在第 4 行、第 5 行空：缩到 3 行时先
        // 收割末尾空行，剩余超出从顶滚入历史，光标行内容保留。
        let mut g = Grid::new(6, 4, 100);
        for r in 0..5 {
            g.cell_mut(r, 0).ch = char::from(b'a' + r as u8);
        }
        g.cursor.row = 4;
        g.resize(3, 4);
        assert_eq!(g.scrollback_len(), 2);
        assert_eq!(g.cursor.row, 2);
        assert_eq!(g.row(2)[0].ch, 'e');
        // 绝对行号不漂移：2(历史) + 2(行) = 缩行前的第 4 行。
        assert_eq!(g.absolute_cursor_line(), 4);
    }

    #[test]
    fn 缩行不收割带背景的空行() {
        // 末行无字符但有背景色（TUI 留下的色块）：不算空行、不可
        // 丢弃，照旧从顶搬行进历史。
        let mut g = Grid::new(3, 2, 100);
        g.cell_mut(0, 0).ch = 'x';
        g.cursor.row = 0;
        g.cell_mut(2, 0).bg = Color::Indexed(1);
        g.resize(2, 2);
        assert_eq!(g.scrollback_len(), 1);
        assert_eq!(g.row(1)[0].bg, Color::Indexed(1));
    }

    #[test]
    fn 缩行历史超限时绝对行号不漂移() {
        // scrollback 容量 1：缩行搬入 3 行触发 2 次超限丢弃，
        // dropped_lines 必须同步递增（旧实现漏计，命令块/选区的
        // 绝对行号会整体错位）。
        let mut g = Grid::new(4, 2, 1);
        for r in 0..4 {
            g.cell_mut(r, 0).ch = 'x';
        }
        g.cursor.row = 3;
        let before = g.absolute_cursor_line();
        g.resize(1, 2);
        assert_eq!(g.scrollback_len(), 1);
        assert_eq!(g.cursor.row, 0);
        assert_eq!(g.absolute_cursor_line(), before);
    }

    #[test]
    fn visible_rows_拼接历史与可视区() {
        let mut g = Grid::new(2, 2, 100);
        g.cell_mut(0, 0).ch = 'x';
        g.scroll_up_one(true); // 'x' 行进入历史
        g.cell_mut(0, 0).ch = 'y';
        g.scroll_display(1);
        let first: Vec<char> = g.visible_rows().map(|r| r[0].ch).collect();
        assert_eq!(first, vec!['x', 'y']);
    }
}
