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
    /// **优先丢弃光标行以下的纯空行**，不足时从顶部裁行（**不进
    /// scrollback**）。
    ///
    /// # 两步缩行策略与根因背景
    ///
    /// 第一步（B2 修复①）：收割光标行以下的纯空行直接丢弃。
    /// 旧实现无条件从顶搬行进 scrollback，新鲜提示符窗格缩行后提示符
    /// 进历史、可视区全空，命令块状态条与光标仍按元数据绘制，形成
    /// 「两根竖条 + 正文全空」（海风哥截图对应场景）。
    ///
    /// 第二步（**B3 修复**）：空行收割不足仍超出时，从顶裁行但
    /// **不推入 scrollback**。根因：ConPTY 在收到 pty.resize 信号后
    /// 从自身的 screen buffer 重发整屏 repaint（CUP 绝对坐标 + 文本）；
    /// 旧实现把顶行推入 scrollback 会使 screen[0] 偏移——ConPTY 重发
    /// 的 `ESC[1;1H` 对我们来说是新的 screen[0]，但我们的 screen[0]
    /// 是原来第 N 行（已发生偏移）；PSReadLine 的差量重绘按错误坐标
    /// 写入，提示符开头丢字 + 输入混叠，且差量错误自我延续，cls 才能
    /// 重置（B3 症状全特征）。不进 scrollback 则 screen[0] 保持与
    /// ConPTY row-0 对齐，重发 repaint 落格正确。
    ///
    /// Windows/ConPTY 语境下「裁掉顶行不保留历史」是正确的：ConPTY
    /// 的 scrollback 由 conhost 自己维护，我们只持有可视区镜像；
    /// 裁掉的行由 ConPTY repaint 重新填入正确内容，用户不会看到丢失。
    ///
    /// # Errors（无，文档段）
    ///
    /// 此方法不返回 `Result`，维度夹紧在内部处理。
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
        // 第二步（B3 根治）：空行收割后仍超出时从顶裁行——**不推入
        // scrollback**（与 B2 修①的空行丢弃同语义：均不修改历史长度
        // 和 dropped_lines）。光标行同步上移（保持指向相同内容）；
        // 但不超过 rows-1，防止光标越界。
        // 核心约束：screen[0] 必须与 ConPTY 的 row-0 严格对齐——
        // ConPTY resize 后重发 repaint 的 CUP 行号是 ConPTY screen 的
        // 绝对行，pushback 到 scrollback 会让两者偏移、坐标失步。
        while self.screen.len() > rows {
            self.screen.pop_front();
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
    fn 缩行空行不足时顶行被裁弃不进历史() {
        // B3 根治回归测试。0..=4 行有内容、光标在第 4 行、第 5 行空：
        // 缩到 3 行时先收割末尾空行，剩余超出从顶裁行（**不进
        // scrollback**，B3 修复语义），光标行内容保留。
        // 旧实现（B3 前）会把 2 行推入 scrollback → ConPTY repaint 的
        // CUP 坐标与我们的 screen[0] 偏移 → 打字错乱。
        let mut g = Grid::new(6, 4, 100);
        for r in 0..5 {
            g.cell_mut(r, 0).ch = char::from(b'a' + r as u8);
        }
        g.cursor.row = 4;
        g.resize(3, 4);
        // B3 关键断言：scrollback 保持为零——顶行不再推入历史。
        assert_eq!(g.scrollback_len(), 0, "B3: 缩行不得把顶行推入 scrollback");
        assert_eq!(g.cursor.row, 2);
        assert_eq!(g.row(2)[0].ch, 'e');
        // dropped_lines 也不增（不入 scrollback = 不丢失计数基准）。
        assert_eq!(g.absolute_cursor_line(), 2);
    }

    #[test]
    fn 缩行不收割带背景的空行也不进历史() {
        // 末行无字符但有背景色（TUI 留下的色块）：is_blank 返回 false，
        // 不被第一步收割，进入第二步从顶裁行（B3 改语义：不入 scrollback）。
        let mut g = Grid::new(3, 2, 100);
        g.cell_mut(0, 0).ch = 'x';
        g.cursor.row = 0;
        g.cell_mut(2, 0).bg = Color::Indexed(1);
        g.resize(2, 2);
        // B3：不再进 scrollback；带背景的行被保留在可视区（顶行 'x' 被裁，
        // 带背景的末行留在 screen[1]）。
        assert_eq!(g.scrollback_len(), 0, "B3: 裁行不得推入 scrollback");
        assert_eq!(g.row(1)[0].bg, Color::Indexed(1));
    }

    #[test]
    fn 缩行顶行裁弃绝对行号从scrollback为零开始() {
        // B3 回归：B2 修前有 dropped_lines 漏计 bug；B3 改为不推入
        // scrollback——dropped_lines 全程为零（不入历史 = 无超限淘汰）。
        let mut g = Grid::new(4, 2, 1);
        for r in 0..4 {
            g.cell_mut(r, 0).ch = 'x';
        }
        g.cursor.row = 3;
        g.resize(1, 2);
        // B3 语义：全部超出行从顶裁弃，不入 scrollback，不触发超限丢弃。
        assert_eq!(g.scrollback_len(), 0, "B3: 不再推入 scrollback");
        assert_eq!(g.cursor.row, 0);
        // 绝对行号 = dropped_lines(0) + scrollback_len(0) + cursor.row(0) = 0。
        assert_eq!(g.absolute_cursor_line(), 0);
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
