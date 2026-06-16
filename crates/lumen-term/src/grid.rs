//! 终端网格：可视区 cell 矩阵 + scrollback 历史。

use std::collections::VecDeque;

use crate::cell::{Cell, CellFlags, Color};

/// 一行单元格。
#[derive(Debug, Clone)]
pub struct Row {
    cells: Vec<Cell>,
    /// 本行是否以 autowrap（软折行）续接下一行：`true` = 写满后光标自动
    /// 折到下一行、与下一行同属一条逻辑行；`false` = 本行以硬换行
    /// （LF/IND/NEL）或屏幕底部结束。仅供 [`Grid::resize`] 的 scrollback
    /// reflow 用于「解折行—按新宽重排」（req6）。可视区行的此标记随其
    /// 滚入 scrollback 时一并带入；屏幕区本身不读它。
    wrapped: bool,
}

impl Row {
    pub fn new(cols: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cols],
            wrapped: false,
        }
    }

    /// 由给定单元格构造一行（reflow 重排专用），并指定软折行标记。
    fn from_cells(cells: Vec<Cell>, wrapped: bool) -> Self {
        Self { cells, wrapped }
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// 本行是否软折行续接下一行（reflow 解折行判据）。
    pub fn is_wrapped(&self) -> bool {
        self.wrapped
    }

    /// 该行是否「真的写满到末列」——reflow 解折行的**内容判据**，与 `wrapped`
    /// 标记联合防御陈旧标记：被擦除/改短（EL/ED/DCH 等）的行尾留默认空白，
    /// 即便 `wrapped` 残留陈旧，`ends_full()==false` 也让 reflow 不把它当续接
    /// 行、不与下一条无关历史行错误合并（对抗审查 critical 项的兜底防线）。
    ///
    /// **容忍 1 格末列 pad（CJK 关键修复）**：宽字符（CJK）占 2 列，在奇数
    /// 列宽下折行时末列会留 1 格空白 pad（宽字符放不下被折到下一行）。若只
    /// 看「末列非空」，这格 pad 会让每条 CJK 折行都被误判「未写满」→ reflow
    /// 每次都把 CJK 逻辑行断开、反复碎裂成长短不一的短行（中文 engram dump
    /// 缩放后碎裂的真机根因）。故放宽为「末列**或**倒数第二列非空」：
    /// 真折行（含 CJK pad）末两列必有内容 → 判满、解折合并；被擦短的行尾留
    /// ≥2 格空白 → 仍判 false、不误合并。
    fn ends_full(&self) -> bool {
        let n = self.cells.len();
        n > 0
            && (self.cells[n - 1] != Cell::default()
                || (n >= 2 && self.cells[n - 2] != Cell::default()))
    }

    /// 调整列数：截断或以空白格扩展。软折行标记不变（屏幕区列宽变化由
    /// ConPTY 重发流对齐，此标记仅在该行滚入 scrollback 后参与 reflow）。
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

/// scrollback reflow（列宽变化）后的**绝对行号重映射**。
///
/// reflow 会改变 scrollback 的行数，使命令块（OSC 133）记录的绝对行号
/// 整体漂移。[`Grid::resize`] 返回本结构，[`crate::Terminal::resize`] 用
/// [`LineRemap::apply`] 把每个块的行号字段映射到 reflow 后的新绝对行号。
///
/// 映射分三段（旧绝对行号 → 新绝对行号）：
/// - `< old_dropped`：reflow 前就已滚出容量、不可达的内容，保持原值
///   （仍不可达，仅需保单调）。
/// - scrollback 区 `[old_dropped, old_screen_base)`：经 `p[i]`（旧第 i 行
///   → 重排后起始行号）映射。
/// - 屏幕区 `>= old_screen_base`：屏幕内容不 reflow，仅随 scrollback 行数
///   变化整体平移到新的屏幕基准 `new_screen_base`。
///
/// 三段均单调非减，保住 `blocks` 按 `prompt_line` 二分（partition_point）
/// 的不变量。
pub struct LineRemap {
    /// reflow 前的 `dropped_lines`。
    old_dropped: u64,
    /// reflow 前的屏幕基准绝对行号（`old_dropped + 旧 scrollback 行数`）。
    old_screen_base: u64,
    /// reflow 后的屏幕基准绝对行号（`old_dropped + 重排后总行数 R`）。
    new_screen_base: u64,
    /// 旧 scrollback 第 i 行 → 重排后完整序列中的起始行号（0..R）。
    p: Vec<usize>,
}

impl LineRemap {
    /// 把旧绝对行号映射到 reflow 后的新绝对行号（单调非减）。
    pub fn apply(&self, line: u64) -> u64 {
        if line < self.old_dropped {
            line
        } else if line < self.old_screen_base {
            // i ∈ 0..p.len()（= 旧 scrollback 行数），p[i] ∈ 0..R。
            let i = (line - self.old_dropped) as usize;
            self.old_dropped + self.p[i] as u64
        } else {
            self.new_screen_base + (line - self.old_screen_base)
        }
    }
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
    ///
    /// 擦除触及行尾（`c1` 覆盖到最后一列）时复位该行 `wrapped`：擦到行尾
    /// 意味着该行不再写满末列、不再有 autowrap 续接的语义基础。否则陈旧的
    /// `wrapped=true` 会随被擦短的行滚入 scrollback，让 reflow 解折行时把它
    /// 与下一条无关历史行错误合并（内容损坏）。与 [`Grid::fill_all`] 复位
    /// 语义统一，覆盖 EL 0/2、ED 0/1 等所有「擦到行尾」的路径；EL/ED 只擦
    /// 行首段（不触末列）时保留 `wrapped`（折行边界单元仍在，语义未变）。
    pub fn fill_region(&mut self, r0: usize, c0: usize, r1: usize, c1: usize, cell: Cell) {
        let reaches_eol = c1 >= self.cols - 1;
        for r in r0..=r1.min(self.rows - 1) {
            let row = &mut self.screen[r];
            for c in c0..=c1.min(self.cols - 1) {
                row[c] = cell;
            }
            if reaches_eol {
                row.wrapped = false;
            }
        }
        self.dirty = true;
    }

    /// 整屏填充。清屏（ED 2/3）后所有行不再续接，软折行标记一并复位，
    /// 防止陈旧的 `wrapped` 随这些行滚入 scrollback 后误导 reflow 解折行。
    pub fn fill_all(&mut self, cell: Cell) {
        for row in &mut self.screen {
            row.fill(cell);
            row.wrapped = false;
        }
        self.dirty = true;
    }

    /// 标记可视区某行的软折行状态（autowrap 续行 = `true` / 硬换行结束 =
    /// `false`）。终端状态机在写字符触发 autowrap、或处理硬换行时调用；
    /// scrollback reflow（req6）据此解折行重排。越界静默忽略（防御）。
    pub fn set_line_wrapped(&mut self, row: usize, wrapped: bool) {
        if let Some(r) = self.screen.get_mut(row) {
            r.wrapped = wrapped;
        }
    }

    /// 调整可视区尺寸。
    ///
    /// # 列宽变化 → scrollback reflow（req6）
    ///
    /// **列数变化时对 scrollback 历史做 reflow**：把软折行（autowrap）续接
    /// 的相邻历史行还原成逻辑行，再按新列宽重排（解折行—重折行，参考
    /// alacritty/wezterm）。这修复了「窗口缩小再放大后回看历史仍是窄宽
    /// （右侧被旧截断丢失）」——旧实现只对屏幕行 `row.resize` 截断/扩展、
    /// **完全不碰 scrollback**，被推入历史的窄行永不重排。
    ///
    /// reflow 改变 scrollback 行数 → 命令块（OSC 133）的**绝对行号**会整体
    /// 漂移，故本方法返回 [`LineRemap`]，由 [`crate::Terminal::resize`] 据此
    /// 重映射 `blocks` 的各行号字段（绝对行号是命令块/选区/跳转的基准，
    /// 漏映射会让块边界整体错位）。`None` = 列宽未变、无需重映射。
    ///
    /// **屏幕区不 reflow**：可视区列宽由 ConPTY resize 后的整屏重发流对齐
    /// （B3-5 已证其重发完整正确）；grid 层若也重排屏幕、或把历史拉回屏幕，
    /// 就会撞上 ConPTY 重发的 `ESC[K` 逐行清空把内容清成空行（B3-6 蒸发
    /// 根因，见下）。故 reflow **只动 scrollback、绝不跨屏幕/历史边界搬行**。
    ///
    /// # 行数变化策略
    ///
    /// 列直接截断/扩展屏幕行；缩小行数时**优先丢弃光标行以下的纯空行**，
    /// 不足时才把顶行滚入历史（alacritty/wezterm 同语义）。行数变化保持
    /// 绝对行号不漂移（顶行滚入历史时 scrollback.len 增、screen 基准减，
    /// 净绝对行号守恒；超限丢弃同步推进 dropped_lines），故**不**进入
    /// [`LineRemap`]。
    ///
    /// # 扩行策略（B3-6 撤销 B3-3 拉回逻辑）
    ///
    /// 扩大行数时**仅在底部 push_back 空行**，scrollback 保持不动。
    ///
    /// 撤销理由（B3-6 根因分析）：
    ///
    /// B3-3 引入的「从 scrollback 拉回历史行」在理论上与 ConPTY 大缓冲
    /// 视口语义对齐，但实测产生两个严重缺陷：
    ///
    /// 1. **历史逐步蒸发**：ConPTY resize 后会发完整重绘流（`ESC[8;r;ct`
    ///    调整大小 → `ESC[H` 归位 → `ESC[K` 逐行清空 → CUP 重新定位
    ///    光标）。拉回的历史行被这些清空序列覆盖成空行；随后缩行时 B2
    ///    修①「收割光标下空行不入历史」把这些被清空的行**丢弃**（正确
    ///    地丢弃空行），但丢掉的是曾是历史内容的行——反复缩放一轮轮把
    ///    真实历史蒸发殆尽，滚不上去。
    ///
    /// 2. **提示符行堆积**：每次 resize 若检测到 shell_waiting_input
    ///    都会计划注入 `\r`，即便提示符根本没有折行（不需要修复锚点）。
    ///    连续放大缩小注入多次，窗格里堆出 4+ 行 `PS F:\...>`。
    ///    （B3-6 修复二在 main.rs 加折行判定，本条是联动根因。）
    ///
    /// 正确行为：scrollback 从此只进不出，历史守恒；可视区内容由
    /// ConPTY 重发流负责对齐（B3-5 字节流已证其重发完整正确：
    /// `ESC[H` 重画 + 光标 CUP 归位正确）。
    ///
    /// # 两步缩行策略
    ///
    /// 第一步（B2 修复①）：收割光标行以下的纯空行直接丢弃（不入历史）。
    /// 旧实现无条件从顶搬行进 scrollback，新鲜提示符窗格缩行后提示符
    /// 进历史、可视区全空，命令块状态条与光标仍按元数据绘制，形成
    /// 「两根竖条 + 正文全空」（海风哥截图对应场景）。
    ///
    /// 第二步：空行收割不足仍超出时，顶行推入 scrollback（alacritty /
    /// wezterm 同语义，保留历史可回看）；超出 scrollback_limit 时
    /// dropped_lines 同步递增（绝对行号基准，选区/命令块标记不漂移）。
    /// 光标行上移以指向相同内容。
    ///
    /// # Errors（无，文档段）
    ///
    /// 此方法不返回 `Result`，维度夹紧在内部处理。
    pub fn resize(&mut self, rows: usize, cols: usize) -> Option<LineRemap> {
        if rows == self.rows && cols == self.cols {
            return None;
        }
        // 列宽变化：先 reflow scrollback（解折行重排），并算出绝对行号
        // 重映射。必须在屏幕行 resize（截断/扩展）**之前**做——scrollback
        // 行保留完整内容、不被截断，逻辑行得以无损解折重排。
        let remap = if cols != self.cols {
            Some(self.reflow_scrollback(cols))
        } else {
            None
        };
        for row in &mut self.screen {
            row.resize(cols);
        }
        // 扩行：底部 push_back 空行，光标位置不动。
        // scrollback 只进不出——历史守恒，可视区由 ConPTY 重发流对齐。
        // （B3-3 的「从 scrollback 拉回历史行」已在 B3-6 撤销，
        //   撤销理由见本方法 rustdoc。）
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
        remap
    }

    /// 对 scrollback 历史做 reflow（列宽变化时由 [`Grid::resize`] 调用）：
    /// 把软折行续接的相邻历史行还原成逻辑行，再按 `new_cols` 重排，原地
    /// 替换 `self.scrollback`，并返回绝对行号 [`LineRemap`]。
    ///
    /// # 算法
    /// 1. 取出全部 scrollback 行（按实际 `cells().len()` 读，**不假设旧列宽**，
    ///    故能自愈历次缩放残留的混合宽度历史行）。
    /// 2. 按 `wrapped` 标记切分逻辑行（末行外每行 `wrapped`）；逐单元格收集
    ///    为「单元」（普通格宽 1；WIDE 主格 + 其 WIDE_SPACER 合为宽 2 的单元，
    ///    重排时整体不拆）；裁掉逻辑行尾部纯空白填充（`== Cell::default()`，
    ///    带背景色的空格不裁）。
    /// 3. 贪心按 `new_cols` 重折：行内放不下的单元换行（前行标 `wrapped`），
    ///    宽字符在仅剩 1 列时整体折到下一行。
    /// 4. 记录每个旧行号 → 重排后起始行号（`p`），供 [`LineRemap`] 映射。
    /// 5. 超 `scrollback_limit` 时丢弃最旧重排行，`dropped_lines` 同步推进。
    fn reflow_scrollback(&mut self, new_cols: usize) -> LineRemap {
        let new_cols = new_cols.max(1);
        let old_dropped = self.dropped_lines;
        let old_sb_len = self.scrollback.len();
        let old_screen_base = old_dropped + old_sb_len as u64;

        // p[i] = 旧 scrollback 第 i 行内容在「重排后完整序列」中的起始行号。
        let mut p = vec![0usize; old_sb_len];
        let old_rows: Vec<Row> = self.scrollback.drain(..).collect();
        let mut reflowed: Vec<Row> = Vec::new();

        let mut i = 0usize;
        while i < old_rows.len() {
            // 逻辑行 = [i..=j]：续接判据 = 标记 wrapped **且**真的写满末列
            // （ends_full）。后者防御陈旧 wrapped——被擦除/改短的行尾留空白，
            // 即便标记残留也不解折合并（见 ends_full / fill_region 注释、对抗
            // 审查 critical 项）。
            let mut j = i;
            while old_rows[j].wrapped && old_rows[j].ends_full() && j + 1 < old_rows.len() {
                j += 1;
            }
            // 收集单元：(单元格, 来源旧行号, 宽度)。WIDE_SPACER 随 WIDE 重建，
            // 落单的 WIDE_SPACER（理论不出现）直接丢弃。
            let mut units: Vec<(Cell, usize, usize)> = Vec::new();
            for (off, row) in old_rows[i..=j].iter().enumerate() {
                let src = i + off;
                let cells = row.cells();
                // 逐行裁掉**该行**尾部纯空白填充再收集单元：折行（续接）行
                // 的末列可能是宽字符放不下留的 pad，行尾填充也是 pad——它们
                // 都不是内容，必须在拼接前去掉，否则会被夹进合并后逻辑行的
                // 中间（如 "中文字 符号" 多出空格）。带背景色的空格
                // != Cell::default()，会被保留（与 alacritty/wezterm 同语义）。
                let mut end = cells.len();
                while end > 0 && cells[end - 1] == Cell::default() {
                    end -= 1;
                }
                let mut c = 0;
                while c < end {
                    let cell = cells[c];
                    if cell.flags.contains(CellFlags::WIDE) {
                        units.push((cell, src, 2));
                        c += 2;
                    } else if cell.flags.contains(CellFlags::WIDE_SPACER) {
                        c += 1;
                    } else {
                        units.push((cell, src, 1));
                        c += 1;
                    }
                }
            }

            let line_start = reflowed.len();
            if units.is_empty() {
                // 整条逻辑行皆空 → 保留一行空行（历史里的空行是有意义的间隔）。
                for slot in p.iter_mut().take(j + 1).skip(i) {
                    *slot = line_start;
                }
                reflowed.push(Row::new(new_cols));
            } else {
                let mut cur: Vec<Cell> = Vec::with_capacity(new_cols);
                let mut marked = vec![false; j - i + 1];
                for (cell, src, w) in units {
                    // 当前行放不下该单元（且非空）：补足到 new_cols 后作为软折行推出。
                    if !cur.is_empty() && cur.len() + w > new_cols {
                        while cur.len() < new_cols {
                            cur.push(Cell::default());
                        }
                        reflowed.push(Row::from_cells(std::mem::take(&mut cur), true));
                    }
                    if !marked[src - i] {
                        p[src] = reflowed.len();
                        marked[src - i] = true;
                    }
                    if w == 2 && cur.len() + 2 <= new_cols {
                        cur.push(cell); // WIDE 主格（已带 WIDE 标志）
                        let mut spacer = cell;
                        spacer.ch = ' ';
                        spacer.flags = (cell.flags - CellFlags::WIDE) | CellFlags::WIDE_SPACER;
                        cur.push(spacer);
                    } else if w == 2 {
                        // 退化：new_cols < 2，宽字符放不下占位 → 仅放主格占 1 列。
                        let mut main = cell;
                        main.flags.remove(CellFlags::WIDE);
                        cur.push(main);
                    } else {
                        cur.push(cell);
                    }
                }
                // 末行（硬换行结束，wrapped=false）：补足并推出。
                while cur.len() < new_cols {
                    cur.push(Cell::default());
                }
                reflowed.push(Row::from_cells(cur, false));
                // 逻辑行内未落到任何单元的旧行（极少：全空续行）映射到行首。
                for off in 0..=(j - i) {
                    if !marked[off] {
                        p[i + off] = line_start;
                    }
                }
            }
            i = j + 1;
        }

        // 容量封顶：超出 scrollback_limit 时丢弃最旧重排行（dropped_lines
        // 同步推进，绝对行号基准守恒）。new_screen_base = old_dropped + R：
        // 无论是否封顶都成立（dropped 增量 + 留存行数 = R）。
        let r_total = reflowed.len();
        let new_sb_len = r_total.min(self.scrollback_limit);
        let drop = r_total - new_sb_len;
        self.dropped_lines += drop as u64;
        self.scrollback = reflowed.into_iter().skip(drop).collect();

        LineRemap {
            old_dropped,
            old_screen_base,
            new_screen_base: old_dropped + r_total as u64,
            p,
        }
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
        // 绝对行号会整体错位）。修复：每次 pop_front 超限时
        // dropped_lines += 1，保证 absolute_cursor_line 不漂移。
        let mut g = Grid::new(4, 2, 1);
        for r in 0..4 {
            g.cell_mut(r, 0).ch = 'x';
        }
        g.cursor.row = 3;
        let before = g.absolute_cursor_line();
        g.resize(1, 2);
        assert_eq!(g.scrollback_len(), 1);
        assert_eq!(g.cursor.row, 0);
        // 绝对行号 = dropped_lines(2) + scrollback_len(1) + cursor.row(0)。
        // 与缩行前 absolute_cursor_line（dropped_lines=0 + sb=0 + row=3=3）相比：
        // 缩行后 = 2+1+0=3，与缩行前一致（光标仍指向原来的第 3 行）。
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

    // ── B3-6 专项单测：扩行底部补空行语义（B3-3 已撤销）──

    #[test]
    fn b36_有历史扩行时历史保留光标不动() {
        // B3-6 撤销 B3-3：扩行时不从 scrollback 拉回历史行，仅底部补
        // 空行，cursor.row 不变，scrollback 维持不动（历史守恒）。
        // ConPTY 重发流负责可视区内容对齐，不需要 grid 层干预。
        let mut g = Grid::new(3, 4, 100);
        // 构造 3 行历史（字符 'a','b','c'）
        for ch in ['a', 'b', 'c'] {
            g.cell_mut(0, 0).ch = ch;
            g.scroll_up_one(true);
        }
        // screen 顶行写提示符 'P'，光标在 screen[0]
        g.cell_mut(0, 0).ch = 'P';
        g.cursor.row = 0;
        let abs_before = g.absolute_cursor_line();

        g.resize(5, 4);

        // scrollback 3 行全部保留（不拉回）
        assert_eq!(g.scrollback_len(), 3, "scrollback 应仍有 3 行（历史守恒）");
        // screen 现在 5 行：顶行仍是提示符，底部补 2 个空行
        assert_eq!(g.row(0)[0].ch, 'P', "screen[0] 仍是提示符 P");
        assert_eq!(g.row(3)[0].ch, ' ', "screen[3] 底部补空行");
        assert_eq!(g.row(4)[0].ch, ' ', "screen[4] 底部补空行");
        // 光标行不动（底部补空行不影响 cursor.row）
        assert_eq!(g.cursor.row, 0, "cursor.row 不变（底部补空行）");
        // 绝对行号不变（dropped_lines 未动，scrollback.len() 未动，row 未动）
        assert_eq!(g.absolute_cursor_line(), abs_before, "绝对行号不变");
    }

    #[test]
    fn b36_无历史扩行底部补空行光标不动() {
        // 场景：新鲜窗格，无历史，screen 只有提示符（顶行有 'P'），
        // 光标在 screen[0]。窗口放大：rows 3→5，扩 2 行。
        // 期望：底部补 2 个空行，cursor.row 不动，绝对行号不变。
        let mut g = Grid::new(3, 4, 100);
        g.cell_mut(0, 0).ch = 'P';
        g.cursor.row = 0;
        let abs_before = g.absolute_cursor_line();

        g.resize(5, 4);

        assert_eq!(g.scrollback_len(), 0, "无历史 scrollback 应仍为 0");
        assert_eq!(g.row(0)[0].ch, 'P', "screen[0] 仍是提示符");
        assert_eq!(g.row(3)[0].ch, ' ', "底部补的空行应为空白");
        assert_eq!(g.cursor.row, 0, "cursor.row 不动");
        assert_eq!(g.absolute_cursor_line(), abs_before, "绝对行号不变");
    }

    #[test]
    fn b36_缩行再扩行历史守恒() {
        // 场景：先缩行（历史部分进 scrollback），再扩回原大小。
        // B3-6 语义：扩行不拉回历史，scrollback 守恒；
        // 缩行时收割空行/推历史语义不变（B2 修复①保留）。
        let mut g = Grid::new(6, 4, 100);
        // 前 4 行写入内容 'a'..'d'，光标在第 4 行，第 5 行空
        for r in 0..4 {
            g.cell_mut(r, 0).ch = char::from(b'a' + r as u8);
        }
        g.cursor.row = 3;

        // 缩到 4 行（丢弃 1 个底部空行后仍需搬 1 行进历史）
        g.resize(4, 4);
        assert_eq!(
            g.scrollback_len(),
            0,
            "底部空行丢弃应足够，无需进 scrollback"
        );
        // 缩到 3 行
        g.resize(3, 4);
        assert_eq!(g.scrollback_len(), 1, "缩到 3 行后应有 1 行在 scrollback");
        let sb_before = g.scrollback_len();
        let abs_after_shrink = g.absolute_cursor_line();

        // 扩回 4 行：底部补空行，scrollback 不变（B3-6 语义）
        g.resize(4, 4);
        // B3-6 撤销拉回：scrollback 仍有 1 行（历史守恒）
        assert_eq!(
            g.scrollback_len(),
            sb_before,
            "扩行后 scrollback 行数不减（历史守恒）"
        );
        // 绝对行号：扩行不改 cursor.row 也不改 scrollback.len()，所以不变
        assert_eq!(
            g.absolute_cursor_line(),
            abs_after_shrink,
            "缩→扩往返后绝对行号不变"
        );
    }

    #[test]
    fn b36_扩行后conpty重发流写入落点正确() {
        // 端到端场景：模拟 ConPTY resize 后的重绘流行为。
        // 步骤：
        //  1. 制造 2 行历史（scrollback 含 'H','I' 两行）。
        //  2. screen 顶行是提示符 'P'，光标 row=0（可视区 3 行）。
        //  3. resize 到 5 行（放大窗口）→ 底部补 2 空行，cursor.row=0 不变。
        //  4. ConPTY 按「新可视区」语义（ESC[H + 逐行清空 + CUP 归位）
        //     重绘：row=0 写 'X'（提示符行被 ConPTY 重写），cursor 移到 row=0。
        // 验证 CUP(row=0) 命中 screen[0]（提示符行仍在顶部）。
        let mut g = Grid::new(3, 4, 100);
        // 制造 2 行历史
        g.cell_mut(0, 0).ch = 'H';
        g.scroll_up_one(true);
        g.cell_mut(0, 0).ch = 'I';
        g.scroll_up_one(true);
        // screen[0] 写提示符，光标在 row=0
        g.cell_mut(0, 0).ch = 'P';
        g.cursor.row = 0;

        // 放大：3 → 5 行
        g.resize(5, 4);

        // scrollback 仍有 2 行（历史守恒）
        assert_eq!(g.scrollback_len(), 2, "scrollback 守恒 2 行");
        // screen[0] 仍是提示符（ConPTY 会用 CUP+ESC[K 重写，但 grid 层
        // 不干预；grid 视角：扩行只补空行）
        assert_eq!(g.row(0)[0].ch, 'P', "screen[0] 仍是原提示符行");
        // cursor.row 不变
        assert_eq!(g.cursor.row, 0, "cursor.row=0 不变");

        // 模拟 ConPTY 重绘：CUP(row=0) 写入 'X'（重写提示符行）
        g.cell_mut(0, 0).ch = 'X';
        assert_eq!(g.row(0)[0].ch, 'X', "CUP row=0 命中 screen[0]");
    }

    // ── B3-4 回归测试：列数变化时 screen 行必须 resize ──

    #[test]
    fn b34_扩行扩列时screen行宽度同步更新() {
        // B3-4 根因的 B3-6 等价验证：扩行+扩列后全部 screen 行（包含
        // 新补入的空行）宽度必须等于新 cols，后续 fill_region 不越界。
        let mut g = Grid::new(11, 14, 1000);
        // 构造 3 行历史（提示符行进 scrollback）
        for _ in 0..3 {
            g.cell_mut(0, 0).ch = 'P';
            g.scroll_up_one(true);
        }
        g.cell_mut(0, 0).ch = '>'; // 当前提示符行
        g.cursor.row = 0;
        assert_eq!(g.scrollback_len(), 3);

        // 扩行+扩列（模拟最大化 11x14 → 18x55）
        g.resize(18, 55);

        // B3-6：scrollback 仍保有 3 行（不拉回）
        assert_eq!(g.scrollback_len(), 3, "scrollback 守恒 3 行（不拉回）");
        assert_eq!(g.cols(), 55, "新列数为 55");
        // 所有 screen 行宽度必须是 55
        for r in 0..g.rows() {
            assert_eq!(
                g.row(r).cells().len(),
                55,
                "screen[{r}] cell 向量长度应为 55"
            );
        }

        // fill_region 全屏不越界（B3-4 核心断言，此处 screen 行全是 55 列）
        g.fill_region(0, 0, 17, 54, crate::cell::Cell::default());
        assert_eq!(g.row(0)[0].ch, ' ', "fill_region 后 screen[0][0] 为空格");
    }

    #[test]
    fn b34_仅扩行不改列时行宽不变() {
        // 边界：仅扩行（cols 不变），screen 行宽度应与 cols 一致。
        let mut g = Grid::new(4, 10, 100);
        // 构造 2 行历史
        for ch in ['A', 'B'] {
            g.cell_mut(0, 0).ch = ch;
            g.scroll_up_one(true);
        }
        g.cursor.row = 0;

        // 仅扩行（4 → 6），cols 不变（10）
        g.resize(6, 10);

        // B3-6：scrollback 仍有 2 行（不拉回），cursor.row=0 不变
        assert_eq!(g.scrollback_len(), 2, "scrollback 守恒 2 行");
        assert_eq!(g.cursor.row, 0, "cursor.row 不变");
        // screen 行宽度全为 10
        for r in 0..6 {
            assert_eq!(g.row(r).cells().len(), 10, "row[{r}] 宽度应为 10");
        }
    }

    // ── req6 专项：列宽变化时 scrollback reflow（解折行—按新宽重排）──

    fn write_row(g: &mut Grid, r: usize, s: &str) {
        for (i, ch) in s.chars().enumerate() {
            g.cell_mut(r, i).ch = ch;
        }
    }

    /// 把一行写好（带 wrapped 标记）滚入 scrollback。
    fn push_history(g: &mut Grid, s: &str, wrapped: bool) {
        write_row(g, 0, s);
        g.set_line_wrapped(0, wrapped);
        g.scroll_up_one(true);
    }

    /// 取某绝对行的文本（去尾部空白）。
    fn sb_text(g: &Grid, abs: u64) -> String {
        g.line_by_abs(abs)
            .unwrap()
            .cells()
            .iter()
            .map(|c| c.ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn reflow_放大列宽时历史解折行重排() {
        // "ABCDEFGH" 在 4 列下折成 ["ABCD"(wrapped),"EFGH"]；放大到 8 列应
        // 解折行重排回一行 "ABCDEFGH"。
        let mut g = Grid::new(2, 4, 100);
        push_history(&mut g, "ABCD", true);
        push_history(&mut g, "EFGH", false);
        assert_eq!(g.scrollback_len(), 2);
        let remap = g.resize(2, 8);
        assert!(remap.is_some(), "列宽变化应产出 LineRemap");
        assert_eq!(g.scrollback_len(), 1, "两折行应合并为一行");
        assert_eq!(sb_text(&g, 0), "ABCDEFGH");
        assert!(
            !g.line_by_abs(0).unwrap().is_wrapped(),
            "合并后单行不再 wrapped"
        );
    }

    #[test]
    fn reflow_缩小列宽时长行重折并标记wrapped() {
        let mut g = Grid::new(2, 8, 100);
        push_history(&mut g, "ABCDEFGH", false);
        assert_eq!(g.scrollback_len(), 1);
        g.resize(2, 4);
        assert_eq!(g.scrollback_len(), 2, "8 列行在 4 列下折成两行");
        assert_eq!(sb_text(&g, 0), "ABCD");
        assert_eq!(sb_text(&g, 1), "EFGH");
        assert!(g.line_by_abs(0).unwrap().is_wrapped(), "首折行标 wrapped");
        assert!(!g.line_by_abs(1).unwrap().is_wrapped(), "末折行不 wrapped");
    }

    #[test]
    fn reflow_缩放往返历史内容守恒不截断() {
        // req6 核心：缩小再放大，历史内容必须完整复原——旧实现 row.resize
        // 在缩列时截断右侧内容、放大永不重排，回看历史是窄且被截断的。
        let mut g = Grid::new(2, 8, 100);
        push_history(&mut g, "ABCDEFGH", false);
        g.resize(2, 4); // 缩
        assert_eq!(g.scrollback_len(), 2);
        g.resize(2, 8); // 放
        assert_eq!(g.scrollback_len(), 1, "重新解折行回一行");
        assert_eq!(sb_text(&g, 0), "ABCDEFGH", "内容完整复原，无截断丢失");
    }

    #[test]
    fn reflow_宽字符不跨折行边界拆分() {
        // "中文"（各占 2 列）填满 4 列一行；缩到 3 列：文 需 2 列、仅剩 1
        // → 整体折到下一行，中/文 都不被拆。
        let mut g = Grid::new(2, 4, 100);
        g.cell_mut(0, 0).ch = '中';
        g.cell_mut(0, 0).flags.insert(CellFlags::WIDE);
        g.cell_mut(0, 1).flags.insert(CellFlags::WIDE_SPACER);
        g.cell_mut(0, 2).ch = '文';
        g.cell_mut(0, 2).flags.insert(CellFlags::WIDE);
        g.cell_mut(0, 3).flags.insert(CellFlags::WIDE_SPACER);
        g.scroll_up_one(true);
        g.resize(2, 3);
        assert_eq!(g.scrollback_len(), 2, "宽字符不拆 → 折成两行");
        let r0 = g.line_by_abs(0).unwrap();
        assert_eq!(r0[0].ch, '中');
        assert!(r0[0].flags.contains(CellFlags::WIDE));
        assert!(r0[1].flags.contains(CellFlags::WIDE_SPACER));
        assert_eq!(r0[2].ch, ' ', "第 3 列容不下宽字符，留空并折行");
        assert!(r0.is_wrapped());
        let r1 = g.line_by_abs(1).unwrap();
        assert_eq!(r1[0].ch, '文');
        assert!(r1[0].flags.contains(CellFlags::WIDE));
    }

    #[test]
    fn reflow_超容量封顶时丢弃最旧行且绝对行号守恒() {
        // limit=2：一条 8 列行缩到 2 列折成 4 行，超限丢最旧 2 行，
        // dropped_lines 推进 2，留存内容仍按绝对行号可取、丢弃的取不到。
        let mut g = Grid::new(2, 8, 2);
        push_history(&mut g, "ABCDEFGH", false);
        g.resize(2, 2); // "AB""CD""EF""GH" → 留 "EF""GH"
        assert_eq!(g.scrollback_len(), 2);
        assert_eq!(sb_text(&g, 2), "EF", "留存首行绝对行号=2（dropped 推进 2）");
        assert_eq!(sb_text(&g, 3), "GH");
        assert!(g.line_by_abs(0).is_none(), "被丢弃的旧绝对行号取不到");
        assert!(g.line_by_abs(1).is_none());
    }

    #[test]
    fn reflow_裁尾部空白且保留空行() {
        let mut g = Grid::new(3, 8, 100);
        push_history(&mut g, "AB", false); // "AB" + 尾部空白
        push_history(&mut g, "", false); // 整行空白
        push_history(&mut g, "CD", false);
        g.resize(3, 4);
        assert_eq!(g.scrollback_len(), 3);
        assert_eq!(sb_text(&g, 0), "AB");
        assert_eq!(sb_text(&g, 1), "", "空行保留为一行空行");
        assert_eq!(sb_text(&g, 2), "CD");
    }

    #[test]
    fn reflow_后所有历史行宽度等于新列宽() {
        // req6 视觉根因：旧实现 scrollback 行宽不随放大更新，渲染器按各行
        // cells 长度画 → 历史显窄。reflow 后每行宽度都 = 新列宽。
        let mut g = Grid::new(2, 4, 100);
        push_history(&mut g, "ABCD", true);
        push_history(&mut g, "EFGH", false);
        push_history(&mut g, "XY", false);
        g.resize(2, 10);
        for abs in 0..g.scrollback_len() as u64 {
            assert_eq!(
                g.line_by_abs(abs).unwrap().cells().len(),
                10,
                "reflow 后历史行 {abs} 宽度应为新列宽 10"
            );
        }
    }

    fn write_cjk(g: &mut Grid, r: usize, start_col: usize, s: &str) {
        let mut c = start_col;
        for ch in s.chars() {
            g.cell_mut(r, c).ch = ch;
            g.cell_mut(r, c).flags.insert(CellFlags::WIDE);
            g.cell_mut(r, c + 1).flags.insert(CellFlags::WIDE_SPACER);
            c += 2;
        }
    }

    #[test]
    fn reflow_cjk宽字符折行往返不碎裂() {
        // req6 真机根因：CJK 宽字符占 2 列，奇数列宽下折行末列留 1 格 pad，
        // ends_full 若不容忍这 1 格，会把 CJK 逻辑行每次 reflow 都误判断开、
        // 反复碎成长短不一的短行（海风哥中文 engram dump 缩放后碎裂）。
        // 构造逻辑行 "中文字符号"（5 宽字符=10 列）在 7 列下折成两行：
        //   row0 = 中文字 + 末列 pad（wrapped），row1 = 符号。
        let mut g = Grid::new(3, 7, 100);
        write_cjk(&mut g, 0, 0, "中文字"); // 占 0..6，col6 留 pad
        g.set_line_wrapped(0, true);
        g.scroll_up_one(true);
        write_cjk(&mut g, 0, 0, "符号"); // 续接末行
        g.set_line_wrapped(0, false);
        g.scroll_up_one(true);
        assert_eq!(g.scrollback_len(), 2);

        // 放大到 14 列：应解折合并回一行，而非保持碎裂。
        g.resize(3, 14);
        assert_eq!(g.scrollback_len(), 1, "CJK 折行应合并回一行，不碎裂");
        let row = g.line_by_abs(0).unwrap();
        let text: String = row
            .cells()
            .iter()
            .filter(|c| !c.flags.contains(CellFlags::WIDE_SPACER))
            .map(|c| c.ch)
            .collect::<String>()
            .trim_end()
            .to_string();
        assert_eq!(text, "中文字符号", "CJK 内容完整解折合并");
    }

    #[test]
    fn reflow_防御_陈旧wrapped但未写满末列不误合并() {
        // 对抗审查 critical 兜底防线：即便某历史行带陈旧 wrapped=true，但其
        // 内容未写满末列（ends_full=false，如被擦短的 "AB"），reflow 也不得
        // 把它与下一条无关历史行解折合并。
        let mut g = Grid::new(2, 6, 100);
        push_history(&mut g, "AB", true); // 陈旧 wrapped=true，但 "AB" 未写满 6 列
        push_history(&mut g, "EFGH", false);
        g.resize(2, 12);
        assert_eq!(
            g.scrollback_len(),
            2,
            "未写满末列的陈旧 wrapped 行不解折合并"
        );
        assert_eq!(sb_text(&g, 0), "AB", "保持独立，非 \"AB    EFGH\"");
        assert_eq!(sb_text(&g, 1), "EFGH", "下一历史行保持独立");
    }

    #[test]
    fn reflow_仅行数变化不触发reflow无remap() {
        // 列宽不变、仅行数变化（如 footer 高度变化）：不 reflow、不重映射，
        // 走原行数调整路径（绝对行号守恒，不进 LineRemap）。
        let mut g = Grid::new(4, 6, 100);
        push_history(&mut g, "AB", false);
        let remap = g.resize(6, 6); // 仅扩行
        assert!(remap.is_none(), "列宽不变不应产出 LineRemap");
    }
}
