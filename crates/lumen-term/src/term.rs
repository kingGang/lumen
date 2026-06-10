//! 终端状态机：把 vte 解析事件应用到 Grid 上。

use log::trace;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

use crate::block::Block;
use crate::cell::{Cell, CellFlags, Color};
use crate::grid::Grid;

/// 终端模拟器核心。喂入 PTY 字节流，维护屏幕状态。
pub struct Terminal {
    parser: Parser,
    inner: TermInner,
}

impl Terminal {
    pub fn new(rows: usize, cols: usize, scrollback_limit: usize) -> Self {
        Self {
            parser: Parser::new(),
            inner: TermInner::new(rows, cols, scrollback_limit),
        }
    }

    /// 处理一段 PTY 输出字节。
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.inner, bytes);
    }

    /// 当前应渲染的网格（alt screen 激活时即 alt 网格）。
    pub fn grid(&self) -> &Grid {
        &self.inner.grid
    }

    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.inner.grid
    }

    /// 调整尺寸（窗口 resize 时由上层调用，同时应通知 PTY）。
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.inner.grid.resize(rows, cols);
        self.inner.scroll_top = 0;
        self.inner.scroll_bottom = rows - 1;
    }

    /// 窗口标题（OSC 0/2 设置）。
    pub fn title(&self) -> &str {
        &self.inner.title
    }

    /// 已采集的命令块（OSC 133）。
    pub fn blocks(&self) -> &[Block] {
        &self.inner.blocks
    }

    /// 取走终端要求回写给 PTY 的应答（DSR/DA 等）。
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.inner.responses)
    }

    /// 取走响铃标记。
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.inner.bell)
    }

    /// 是否处于备用屏幕（vim/less 等全屏程序）。
    pub fn is_alt_screen(&self) -> bool {
        self.inner.saved_main.is_some()
    }

    /// 是否处于同步更新区间（DEC 2026 BSU/ESU）。
    ///
    /// TUI 程序用它包住整帧重绘，期间上层不应渲染，
    /// 否则会把光标游走、半成品内容等中间状态画出来。
    pub fn is_synchronized(&self) -> bool {
        self.inner.sync_output
    }

    /// 是否开启 bracketed paste（DEC 2004）：粘贴需包 `ESC[200~`/`ESC[201~`。
    pub fn bracketed_paste(&self) -> bool {
        self.inner.bracketed_paste
    }

    /// ESU（同步帧结束）单调标记：每次 ESU 后递增。
    /// 上层对比两次取值即可知道期间是否完成过同步帧——完成的帧
    /// 应立即渲染（DEC 2026 的本意），无需再等静默合帧。
    pub fn esu_mark(&self) -> u64 {
        self.inner.last_esu_seq
    }

    /// 光标是否处于「帧尾未归位」状态：最近一次 ESU（同步帧结束）
    /// 发生在最近一次「显示光标」之后。
    ///
    /// 部分 TUI（如 codex）在 ESU 时光标停在重绘残留位、之后才另发
    /// 「隐藏→移动→显示」把光标归位——该窗口内的光标位置不可信，
    /// 上层应暂缓更新光标绘制位置，等下一次「显示光标」或超时。
    pub fn cursor_unsettled(&self) -> bool {
        self.inner.last_esu_seq > self.inner.last_cursor_show_seq
    }

    /// 提取选区覆盖的文本：行尾去空白、行间以 `\n` 连接、跳过宽字符占位格。
    pub fn selection_text(&self, sel: &crate::Selection) -> String {
        let (s, e) = sel.normalized();
        let grid = &self.inner.grid;
        let mut out = String::new();
        for line in s.line..=e.line {
            let Some(row) = grid.line_by_abs(line) else {
                continue;
            };
            let cells = row.cells();
            let from = if line == s.line { s.col } else { 0 };
            let to = if line == e.line {
                e.col.min(cells.len().saturating_sub(1))
            } else {
                cells.len().saturating_sub(1)
            };
            let mut text = String::new();
            for cell in cells.iter().take(to + 1).skip(from) {
                if !cell.flags.contains(CellFlags::WIDE_SPACER) {
                    text.push(cell.ch);
                }
            }
            if line != s.line {
                out.push('\n');
            }
            // 非选区末行去掉行尾填充空白。
            if line != e.line {
                out.push_str(text.trim_end());
            } else {
                out.push_str(&text);
            }
        }
        out
    }
}

/// 实际的 Perform 实现，与 Parser 分离以满足借用规则。
struct TermInner {
    grid: Grid,
    /// alt screen 激活时保存的主屏（网格 + 待恢复光标）。
    saved_main: Option<Grid>,
    /// 当前书写属性（fg/bg/flags 生效，ch 无意义）。
    pen: Cell,
    /// DECSC/CSI s 保存的 (row, col, pen)。
    saved_cursor: Option<(usize, usize, Cell)>,
    /// 滚动区上下边界（含端点，0 基）。
    scroll_top: usize,
    scroll_bottom: usize,
    title: String,
    blocks: Vec<Block>,
    /// 需回写 PTY 的应答字节（DSR 等）。
    responses: Vec<u8>,
    bell: bool,
    /// DEC 2026 同步更新进行中。
    sync_output: bool,
    /// DEC 2004 bracketed paste 已开启。
    bracketed_paste: bool,
    /// 事件序号，用于判断「显示光标」与「ESU」的先后。
    event_seq: u64,
    last_cursor_show_seq: u64,
    last_esu_seq: u64,
}

impl TermInner {
    fn new(rows: usize, cols: usize, scrollback_limit: usize) -> Self {
        Self {
            grid: Grid::new(rows, cols, scrollback_limit),
            saved_main: None,
            pen: Cell::default(),
            saved_cursor: None,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            title: String::new(),
            blocks: Vec::new(),
            responses: Vec::new(),
            bell: false,
            sync_output: false,
            bracketed_paste: false,
            event_seq: 0,
            last_cursor_show_seq: 0,
            last_esu_seq: 0,
        }
    }

    /// 光标下移一行；到滚动区底部则滚动。
    fn linefeed(&mut self) {
        if self.grid.cursor.row == self.scroll_bottom {
            // 全屏滚动区且非备用屏时，滚出的行进入历史。
            let full = self.scroll_top == 0 && self.scroll_bottom == self.grid.rows() - 1;
            if full {
                self.grid.scroll_up_one(self.saved_main.is_none());
            } else {
                self.grid
                    .scroll_region_up(self.scroll_top, self.scroll_bottom, 1);
            }
        } else if self.grid.cursor.row + 1 < self.grid.rows() {
            self.grid.cursor.row += 1;
        }
        self.grid.mark_dirty();
    }

    /// 光标上移一行（RI）；到滚动区顶部则反向滚动。
    fn reverse_linefeed(&mut self) {
        if self.grid.cursor.row == self.scroll_top {
            self.grid
                .scroll_region_down(self.scroll_top, self.scroll_bottom, 1);
        } else if self.grid.cursor.row > 0 {
            self.grid.cursor.row -= 1;
        }
        self.grid.mark_dirty();
    }

    /// 写入一个可见字符（处理延迟换行与宽字符）。
    fn put_char(&mut self, c: char) {
        let width = c.width().unwrap_or(0);
        if width == 0 {
            // 组合字符 M1 暂不支持，丢弃。
            return;
        }
        let cols = self.grid.cols();

        if self.grid.cursor.pending_wrap {
            self.grid.cursor.pending_wrap = false;
            self.grid.cursor.col = 0;
            self.linefeed();
        }
        // 宽字符在行尾放不下时，先补空格换行。
        if width == 2 && self.grid.cursor.col + 2 > cols {
            let (r, c0) = (self.grid.cursor.row, self.grid.cursor.col);
            for c in c0..cols {
                let cell = self.grid.cell_mut(r, c);
                *cell = self.pen;
                cell.ch = ' ';
                cell.flags = self.pen.flags;
            }
            self.grid.cursor.col = 0;
            self.linefeed();
        }

        let (r, col) = (self.grid.cursor.row, self.grid.cursor.col);
        let mut cell = self.pen;
        cell.ch = c;
        if width == 2 {
            cell.flags.insert(CellFlags::WIDE);
        }
        *self.grid.cell_mut(r, col) = cell;
        if width == 2 && col + 1 < cols {
            let spacer = self.grid.cell_mut(r, col + 1);
            *spacer = self.pen;
            spacer.ch = ' ';
            spacer.flags.insert(CellFlags::WIDE_SPACER);
        }

        let next = col + width;
        if next >= cols {
            self.grid.cursor.col = cols - 1;
            self.grid.cursor.pending_wrap = true;
        } else {
            self.grid.cursor.col = next;
        }
    }

    /// 擦除用的空白格：保留当前背景色（终端惯例）。
    fn blank(&self) -> Cell {
        Cell {
            bg: self.pen.bg,
            ..Cell::default()
        }
    }

    /// 应用 SGR 参数序列。
    fn apply_sgr(&mut self, params: &Params) {
        let mut iter = params.iter();
        while let Some(group) = iter.next() {
            let code = group.first().copied().unwrap_or(0);
            match code {
                0 => {
                    self.pen.fg = Color::Default;
                    self.pen.bg = Color::Default;
                    self.pen.flags = CellFlags::empty();
                }
                1 => self.pen.flags.insert(CellFlags::BOLD),
                2 => self.pen.flags.insert(CellFlags::DIM),
                3 => self.pen.flags.insert(CellFlags::ITALIC),
                4 => self.pen.flags.insert(CellFlags::UNDERLINE),
                7 => self.pen.flags.insert(CellFlags::INVERSE),
                9 => self.pen.flags.insert(CellFlags::STRIKE),
                22 => {
                    self.pen.flags.remove(CellFlags::BOLD);
                    self.pen.flags.remove(CellFlags::DIM);
                }
                23 => self.pen.flags.remove(CellFlags::ITALIC),
                24 => self.pen.flags.remove(CellFlags::UNDERLINE),
                27 => self.pen.flags.remove(CellFlags::INVERSE),
                29 => self.pen.flags.remove(CellFlags::STRIKE),
                30..=37 => self.pen.fg = Color::Indexed(code as u8 - 30),
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Indexed(code as u8 - 40),
                49 => self.pen.bg = Color::Default,
                90..=97 => self.pen.fg = Color::Indexed(code as u8 - 90 + 8),
                100..=107 => self.pen.bg = Color::Indexed(code as u8 - 100 + 8),
                38 | 48 => {
                    // 扩展色：冒号形式整组到达（[38,2,r,g,b] / [38,5,n]），
                    // 分号形式需要继续消费后续参数组。
                    let color = if group.len() > 1 {
                        Self::parse_extended_color(&group[1..])
                    } else {
                        let mode = iter.next().map(|g| g[0]);
                        match mode {
                            Some(5) => iter
                                .next()
                                .map(|g| Color::Indexed(g[0].min(255) as u8)),
                            Some(2) => {
                                let r = iter.next().map(|g| g[0]).unwrap_or(0);
                                let g = iter.next().map(|g| g[0]).unwrap_or(0);
                                let b = iter.next().map(|g| g[0]).unwrap_or(0);
                                Some(Color::Rgb(r as u8, g as u8, b as u8))
                            }
                            _ => None,
                        }
                    };
                    if let Some(color) = color {
                        if code == 38 {
                            self.pen.fg = color;
                        } else {
                            self.pen.bg = color;
                        }
                    }
                }
                _ => trace!("未实现的 SGR: {code}"),
            }
        }
    }

    /// 解析冒号形式扩展色参数（去掉前导 38/48 后的部分）。
    fn parse_extended_color(sub: &[u16]) -> Option<Color> {
        match sub.first()? {
            5 => Some(Color::Indexed((*sub.get(1)?).min(255) as u8)),
            2 => {
                // CSI 38:2:<colorspace>:r:g:b 与 38:2:r:g:b 两种都存在。
                let rgb = if sub.len() >= 5 { &sub[2..5] } else { sub.get(1..4)? };
                Some(Color::Rgb(
                    (*rgb.first()?) as u8,
                    (*rgb.get(1)?) as u8,
                    (*rgb.get(2)?) as u8,
                ))
            }
            _ => None,
        }
    }

    /// 切换备用屏幕（DECSET/DECRST 1049）。
    fn set_alt_screen(&mut self, on: bool) {
        if on && self.saved_main.is_none() {
            self.saved_cursor = Some((self.grid.cursor.row, self.grid.cursor.col, self.pen));
            let alt = Grid::new(self.grid.rows(), self.grid.cols(), 0);
            self.saved_main = Some(std::mem::replace(&mut self.grid, alt));
        } else if !on {
            if let Some(main) = self.saved_main.take() {
                self.grid = main;
                if let Some((r, c, pen)) = self.saved_cursor.take() {
                    self.grid.cursor.row = r.min(self.grid.rows() - 1);
                    self.grid.cursor.col = c.min(self.grid.cols() - 1);
                    self.pen = pen;
                }
                // 主屏行列可能在 alt 期间被 resize 过，由上层 resize 流程兜底。
                self.grid.mark_dirty();
            }
        }
        self.scroll_top = 0;
        self.scroll_bottom = self.grid.rows() - 1;
    }

    /// 处理 OSC 133 命令边界标记。
    fn handle_block_marker(&mut self, params: &[&[u8]]) {
        let marker = params.get(1).and_then(|p| p.first()).copied();
        let line = self.grid.absolute_cursor_line();
        match marker {
            Some(b'A') => self.blocks.push(Block {
                prompt_line: line,
                ..Block::default()
            }),
            Some(b'B') => {
                if let Some(b) = self.blocks.last_mut() {
                    b.cmd_line = Some(line);
                }
            }
            Some(b'C') => {
                if let Some(b) = self.blocks.last_mut() {
                    b.output_line = Some(line);
                }
            }
            Some(b'D') => {
                if let Some(b) = self.blocks.last_mut() {
                    b.end_line = Some(line);
                    b.exit_code = params
                        .get(2)
                        .and_then(|p| std::str::from_utf8(p).ok())
                        .and_then(|s| s.parse().ok());
                }
            }
            _ => {}
        }
    }
}

impl Perform for TermInner {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => {
                self.grid.cursor.col = 0;
                self.grid.cursor.pending_wrap = false;
                self.grid.mark_dirty();
            }
            b'\n' | 0x0b | 0x0c => {
                self.grid.cursor.pending_wrap = false;
                self.linefeed();
            }
            0x08 => {
                // BS：左移一格，不擦除。
                self.grid.cursor.col = self.grid.cursor.col.saturating_sub(1);
                self.grid.cursor.pending_wrap = false;
                self.grid.mark_dirty();
            }
            b'\t' => {
                // 简化为固定 8 列制表位。
                let cols = self.grid.cols();
                let next = ((self.grid.cursor.col / 8) + 1) * 8;
                self.grid.cursor.col = next.min(cols - 1);
                self.grid.mark_dirty();
            }
            0x07 => self.bell = true,
            _ => trace!("未实现的控制字符: {byte:#04x}"),
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // 多数 CSI 第一个参数缺省为 1（移动类）；个别（如 ED/EL）缺省 0，单独处理。
        let p0 = params.iter().next().and_then(|g| g.first().copied());
        let n = p0.map(|v| v.max(1) as usize).unwrap_or(1);
        let rows = self.grid.rows();
        let cols = self.grid.cols();
        let cur = self.grid.cursor;
        let private = intermediates.first() == Some(&b'?');

        match action {
            'A' => self.grid.cursor.row = cur.row.saturating_sub(n),
            'B' | 'e' => self.grid.cursor.row = (cur.row + n).min(rows - 1),
            'C' | 'a' => self.grid.cursor.col = (cur.col + n).min(cols - 1),
            'D' => self.grid.cursor.col = cur.col.saturating_sub(n),
            'E' => {
                self.grid.cursor.row = (cur.row + n).min(rows - 1);
                self.grid.cursor.col = 0;
            }
            'F' => {
                self.grid.cursor.row = cur.row.saturating_sub(n);
                self.grid.cursor.col = 0;
            }
            'G' | '`' => self.grid.cursor.col = (n - 1).min(cols - 1),
            'd' => self.grid.cursor.row = (n - 1).min(rows - 1),
            'H' | 'f' => {
                let mut it = params.iter();
                let r = it.next().and_then(|g| g.first().copied()).unwrap_or(1).max(1) as usize;
                let c = it.next().and_then(|g| g.first().copied()).unwrap_or(1).max(1) as usize;
                self.grid.cursor.row = (r - 1).min(rows - 1);
                self.grid.cursor.col = (c - 1).min(cols - 1);
            }
            'J' => {
                let mode = p0.unwrap_or(0);
                let blank = self.blank();
                match mode {
                    0 => {
                        self.grid.fill_region(cur.row, cur.col, cur.row, cols - 1, blank);
                        if cur.row + 1 < rows {
                            self.grid.fill_region(cur.row + 1, 0, rows - 1, cols - 1, blank);
                        }
                    }
                    1 => {
                        if cur.row > 0 {
                            self.grid.fill_region(0, 0, cur.row - 1, cols - 1, blank);
                        }
                        self.grid.fill_region(cur.row, 0, cur.row, cur.col, blank);
                    }
                    2 | 3 => self.grid.fill_all(blank),
                    _ => {}
                }
            }
            'K' => {
                let mode = p0.unwrap_or(0);
                let blank = self.blank();
                match mode {
                    0 => self.grid.fill_region(cur.row, cur.col, cur.row, cols - 1, blank),
                    1 => self.grid.fill_region(cur.row, 0, cur.row, cur.col, blank),
                    2 => self.grid.fill_region(cur.row, 0, cur.row, cols - 1, blank),
                    _ => {}
                }
            }
            'L' => {
                if cur.row >= self.scroll_top && cur.row <= self.scroll_bottom {
                    self.grid.scroll_region_down(cur.row, self.scroll_bottom, n);
                }
            }
            'M' => {
                if cur.row >= self.scroll_top && cur.row <= self.scroll_bottom {
                    self.grid.scroll_region_up(cur.row, self.scroll_bottom, n);
                }
            }
            '@' => {
                // ICH：插入空白，行内右移。
                let row = self.grid.row_mut(cur.row);
                for _ in 0..n.min(cols - cur.col) {
                    for c in (cur.col + 1..cols).rev() {
                        row[c] = row[c - 1];
                    }
                    row[cur.col] = Cell::default();
                }
            }
            'P' => {
                // DCH：删除字符，行内左移补空白。
                let row = self.grid.row_mut(cur.row);
                for _ in 0..n.min(cols - cur.col) {
                    for c in cur.col..cols - 1 {
                        row[c] = row[c + 1];
                    }
                    row[cols - 1] = Cell::default();
                }
            }
            'X' => {
                let end = (cur.col + n - 1).min(cols - 1);
                let blank = self.blank();
                self.grid.fill_region(cur.row, cur.col, cur.row, end, blank);
            }
            'S' => self.grid.scroll_region_up(self.scroll_top, self.scroll_bottom, n),
            'T' => self.grid.scroll_region_down(self.scroll_top, self.scroll_bottom, n),
            'm' => self.apply_sgr(params),
            'r' => {
                let mut it = params.iter();
                let top = it.next().and_then(|g| g.first().copied()).unwrap_or(1).max(1) as usize;
                let bottom = it
                    .next()
                    .and_then(|g| g.first().copied())
                    .map(|v| v as usize)
                    .filter(|v| *v > 0)
                    .unwrap_or(rows);
                if top < bottom {
                    self.scroll_top = top - 1;
                    self.scroll_bottom = (bottom - 1).min(rows - 1);
                    self.grid.cursor.row = 0;
                    self.grid.cursor.col = 0;
                }
            }
            'h' | 'l' => {
                let on = action == 'h';
                if private {
                    for g in params.iter() {
                        match g.first().copied().unwrap_or(0) {
                            25 => {
                                self.grid.cursor.visible = on;
                                if on {
                                    self.event_seq += 1;
                                    self.last_cursor_show_seq = self.event_seq;
                                }
                            }
                            1049 | 1047 => self.set_alt_screen(on),
                            2004 => self.bracketed_paste = on,
                            2026 => {
                                self.sync_output = on;
                                if !on {
                                    self.event_seq += 1;
                                    self.last_esu_seq = self.event_seq;
                                }
                            }
                            1048 => {
                                if on {
                                    self.saved_cursor =
                                        Some((cur.row, cur.col, self.pen));
                                } else if let Some((r, c, pen)) = self.saved_cursor.take() {
                                    self.grid.cursor.row = r.min(rows - 1);
                                    self.grid.cursor.col = c.min(cols - 1);
                                    self.pen = pen;
                                }
                            }
                            m => trace!("未实现的私有模式: ?{m} {}", if on { "h" } else { "l" }),
                        }
                    }
                }
            }
            'n' => {
                // DSR：5 = 状态报告，6 = 光标位置报告（CPR）。
                match p0.unwrap_or(0) {
                    5 => self.responses.extend_from_slice(b"\x1b[0n"),
                    6 => {
                        let s = format!("\x1b[{};{}R", cur.row + 1, cur.col + 1);
                        self.responses.extend_from_slice(s.as_bytes());
                    }
                    _ => {}
                }
            }
            'c' => {
                // DA：宣告为 VT220 级别终端。
                self.responses.extend_from_slice(b"\x1b[?62;22c");
            }
            'p' if private && intermediates.contains(&b'$') => {
                // DECRQM 私有模式查询：TUI 库以此探测能力（尤其 2026
                // 同步更新，不应答就不会启用 BSU/ESU）。1=开 2=关 0=不支持。
                let mode = p0.unwrap_or(0);
                let value = match mode {
                    25 => {
                        if self.grid.cursor.visible {
                            1
                        } else {
                            2
                        }
                    }
                    1049 | 1047 => {
                        if self.saved_main.is_some() {
                            1
                        } else {
                            2
                        }
                    }
                    2004 => {
                        if self.bracketed_paste {
                            1
                        } else {
                            2
                        }
                    }
                    2026 => {
                        if self.sync_output {
                            1
                        } else {
                            2
                        }
                    }
                    _ => 0,
                };
                let s = format!("\x1b[?{mode};{value}$y");
                self.responses.extend_from_slice(s.as_bytes());
            }
            's' => self.saved_cursor = Some((cur.row, cur.col, self.pen)),
            'u' => {
                if let Some((r, c, pen)) = self.saved_cursor.take() {
                    self.grid.cursor.row = r.min(rows - 1);
                    self.grid.cursor.col = c.min(cols - 1);
                    self.pen = pen;
                }
            }
            _ => trace!("未实现的 CSI: {action} {params:?}"),
        }
        // 任何 CSI 都打断延迟换行状态。
        self.grid.cursor.pending_wrap = false;
        self.grid.mark_dirty();
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            // ESC ( B 等字符集切换 M1 忽略。
            return;
        }
        match byte {
            b'7' => {
                self.saved_cursor =
                    Some((self.grid.cursor.row, self.grid.cursor.col, self.pen));
            }
            b'8' => {
                if let Some((r, c, pen)) = self.saved_cursor.take() {
                    self.grid.cursor.row = r.min(self.grid.rows() - 1);
                    self.grid.cursor.col = c.min(self.grid.cols() - 1);
                    self.pen = pen;
                    self.grid.mark_dirty();
                }
            }
            b'D' => self.linefeed(),
            b'E' => {
                self.grid.cursor.col = 0;
                self.linefeed();
            }
            b'M' => self.reverse_linefeed(),
            b'c' => {
                // RIS：全终端重置。
                let (rows, cols) = (self.grid.rows(), self.grid.cols());
                *self = TermInner::new(rows, cols, 10_000);
            }
            _ => trace!("未实现的 ESC: {byte:#04x}"),
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let code = params.first().copied().unwrap_or(b"");
        match code {
            b"0" | b"2" => {
                if let Some(title) = params.get(1).and_then(|p| std::str::from_utf8(p).ok()) {
                    self.title = title.to_owned();
                }
            }
            b"133" => self.handle_block_marker(params),
            _ => trace!("未实现的 OSC: {:?}", std::str::from_utf8(code)),
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term() -> Terminal {
        Terminal::new(5, 10, 100)
    }

    fn screen_text(t: &Terminal) -> Vec<String> {
        t.grid()
            .visible_rows()
            .map(|r| r.cells().iter().map(|c| c.ch).collect::<String>())
            .collect()
    }

    #[test]
    fn 普通文本写入与换行() {
        let mut t = term();
        t.advance(b"ab\r\ncd");
        let s = screen_text(&t);
        assert!(s[0].starts_with("ab"));
        assert!(s[1].starts_with("cd"));
        assert_eq!(t.grid().cursor.row, 1);
        assert_eq!(t.grid().cursor.col, 2);
    }

    #[test]
    fn 行尾延迟换行() {
        let mut t = term();
        t.advance(b"0123456789"); // 恰好写满一行
        assert_eq!(t.grid().cursor.row, 0); // 尚未换行
        t.advance(b"x");
        assert_eq!(t.grid().cursor.row, 1);
        assert!(screen_text(&t)[1].starts_with('x'));
    }

    #[test]
    fn 光标定位与擦除() {
        let mut t = term();
        t.advance(b"hello\x1b[1;1Hx\x1b[K");
        let s = screen_text(&t);
        assert_eq!(&s[0][..2], "x "); // x 后整行被 EL 擦掉
    }

    #[test]
    fn sgr_基础色与重置() {
        let mut t = term();
        t.advance(b"\x1b[31mr\x1b[0mn");
        let row = t.grid().row(0);
        assert_eq!(row[0].fg, Color::Indexed(1));
        assert_eq!(row[1].fg, Color::Default);
    }

    #[test]
    fn sgr_256色与真彩() {
        let mut t = term();
        t.advance(b"\x1b[38;5;208mA\x1b[38;2;10;20;30mB");
        let row = t.grid().row(0);
        assert_eq!(row[0].fg, Color::Indexed(208));
        assert_eq!(row[1].fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn 宽字符占两列() {
        let mut t = term();
        t.advance("中".as_bytes());
        let row = t.grid().row(0);
        assert_eq!(row[0].ch, '中');
        assert!(row[0].flags.contains(CellFlags::WIDE));
        assert!(row[1].flags.contains(CellFlags::WIDE_SPACER));
        assert_eq!(t.grid().cursor.col, 2);
    }

    #[test]
    fn 满屏后滚动进历史() {
        let mut t = term();
        t.advance(b"1\r\n2\r\n3\r\n4\r\n5\r\n6");
        assert_eq!(t.grid().scrollback_len(), 1);
        assert!(screen_text(&t)[0].starts_with('2'));
    }

    #[test]
    fn 备用屏幕切换与恢复() {
        let mut t = term();
        t.advance(b"main\x1b[?1049h");
        assert!(t.is_alt_screen());
        t.advance(b"alt");
        assert!(screen_text(&t)[0].starts_with("alt"));
        t.advance(b"\x1b[?1049l");
        assert!(!t.is_alt_screen());
        assert!(screen_text(&t)[0].starts_with("main"));
    }

    #[test]
    fn dsr_光标位置应答() {
        let mut t = term();
        t.advance(b"\x1b[3;4H\x1b[6n");
        assert_eq!(t.take_responses(), b"\x1b[3;4R".to_vec());
    }

    #[test]
    fn osc133_块边界采集() {
        let mut t = term();
        t.advance(b"\x1b]133;A\x07$ ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07");
        let blocks = t.blocks();
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].is_closed());
        assert_eq!(blocks[0].exit_code, Some(0));
    }

    #[test]
    fn 选区文本提取_跨行与宽字符() {
        use crate::{SelPoint, Selection};
        let mut t = Terminal::new(5, 10, 100);
        t.advance("ab中\r\ncdef".as_bytes());
        // 从 (0,0) 选到 (1,2)：应得 "ab中\ncde"。
        let sel = Selection {
            anchor: SelPoint { line: 0, col: 0 },
            head: SelPoint { line: 1, col: 2 },
        };
        assert_eq!(t.selection_text(&sel), "ab中\ncde");
        // 反向拖动结果一致。
        let rev = Selection {
            anchor: sel.head,
            head: sel.anchor,
        };
        assert_eq!(t.selection_text(&rev), "ab中\ncde");
    }

    #[test]
    fn bracketed_paste_模式记录() {
        let mut t = term();
        assert!(!t.bracketed_paste());
        t.advance(b"\x1b[?2004h");
        assert!(t.bracketed_paste());
        t.advance(b"\x1b[?2004l");
        assert!(!t.bracketed_paste());
    }

    #[test]
    fn 帧尾未归位判定() {
        let mut t = term();
        // codex 式帧：重绘后 show，再 ESU——帧尾未归位。
        t.advance(b"\x1b[?2026h\x1b[?25l\x1b[1;1Hx\x1b[?25h\x1b[?2026l");
        assert!(t.cursor_unsettled());
        // 归位序列到达（hide + move + show）后恢复可信。
        t.advance(b"\x1b[?25l\x1b[2;1H\x1b[?25h");
        assert!(!t.cursor_unsettled());
        // 普通打字流（无 ESU）从不进入未归位态。
        t.advance(b"\x1b[?25labc\x1b[?25h");
        assert!(!t.cursor_unsettled());
    }

    #[test]
    fn 同步更新模式切换与查询应答() {
        let mut t = term();
        assert!(!t.is_synchronized());
        t.advance(b"\x1b[?2026h");
        assert!(t.is_synchronized());
        // DECRQM 查询应在同步中应答 1。
        t.advance(b"\x1b[?2026$p");
        assert_eq!(t.take_responses(), b"\x1b[?2026;1$y".to_vec());
        t.advance(b"\x1b[?2026l");
        assert!(!t.is_synchronized());
        // 未知模式应答 0。
        t.advance(b"\x1b[?9999$p");
        assert_eq!(t.take_responses(), b"\x1b[?9999;0$y".to_vec());
    }

    #[test]
    fn 滚动区内滚动不进历史() {
        let mut t = term();
        t.advance(b"\x1b[2;4r"); // 滚动区 2-4 行
        t.advance(b"\x1b[4;1Ha\r\nb"); // 在底部触发区内滚动
        assert_eq!(t.grid().scrollback_len(), 0);
    }
}
