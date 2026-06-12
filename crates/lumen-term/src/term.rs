//! 终端状态机：把 vte 解析事件应用到 Grid 上。

use std::path::{Path, PathBuf};

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

    /// shell 上报的当前工作目录（OSC 9;9，ConEmu/Windows Terminal 约定，
    /// 由 integration.ps1 在每个提示符发射）。尚未上报时为 None；
    /// RIS 全重置后清空，下个提示符会重新上报。
    pub fn cwd(&self) -> Option<&Path> {
        self.inner.cwd.as_deref()
    }

    /// shell 是否正等待用户输入命令（文件树 cd 注入与 M4 输入编辑器的依据）。
    ///
    /// 判定（OSC 133 字段语义见 block.rs）：最后一个命令块已有「命令
    /// 输入开始」标记（133;B，提示符渲染完毕）但尚无「输出开始」标记
    /// （133;C，用户回车执行时才发）且未闭合。备用屏幕（vim 等全屏
    /// 程序）一律视为忙。
    ///
    /// 已知局限：用户敲了半行命令未回车时仍判定为等待输入——此时注入
    /// 命令会拼接在用户输入之后。调用方接受此局限，M4 输入编辑器接管
    /// 命令行后消除。
    pub fn shell_waiting_input(&self) -> bool {
        !self.is_alt_screen()
            && self
                .inner
                .blocks
                .last()
                .is_some_and(|b| b.cmd_line.is_some() && b.output_line.is_none() && !b.is_closed())
    }

    /// 已采集的命令块（OSC 133）。
    pub fn blocks(&self) -> &[Block] {
        &self.inner.blocks
    }

    /// 主屏网格：块行号永远以主屏坐标记录，alt screen 激活期间
    /// 块查询/提取必须用它而不是当前（alt）网格。
    fn main_grid(&self) -> &Grid {
        self.inner.saved_main.as_ref().unwrap_or(&self.inner.grid)
    }

    /// 查找覆盖指定绝对行的块。块范围为 `[prompt_line, end_line)`，
    /// 未闭合块只延伸到主屏光标行（不向下方空白区无限延伸）。
    pub fn block_at_line(&self, abs: u64) -> Option<&Block> {
        self.block_at_line_capped(abs, self.main_grid().absolute_cursor_line())
    }

    /// 同 [`Self::block_at_line`]，但未闭合块的下边界绝对行号（含）
    /// 由调用方给定，不读 live 光标行。
    ///
    /// 渲染侧用「防抖后的光标行」做下边界：codex 等 TUI 在 DEC 2026
    /// 同步帧尾常把光标停在重绘残留位（见 [`Self::cursor_unsettled`]），
    /// live 光标行帧间跨行大跳——运行中块的左缘状态条若跟着 live 行
    /// 伸缩，就是「蓝条闪烁」的直接来源（需求池 P1）。
    pub fn block_at_line_capped(&self, abs: u64, unclosed_end: u64) -> Option<&Block> {
        let blocks = &self.inner.blocks;
        // prompt_line 单调递增：找最后一个 prompt_line <= abs 的块。
        let idx = blocks.partition_point(|b| b.prompt_line <= abs);
        let b = blocks.get(idx.checked_sub(1)?)?;
        match b.end_line {
            Some(end) if abs >= end => None,
            Some(_) => Some(b),
            None if abs <= unclosed_end => Some(b),
            None => None,
        }
    }

    /// 按 id 查块。
    pub fn block_by_id(&self, id: u64) -> Option<&Block> {
        self.inner.blocks.iter().find(|b| b.id == id)
    }

    /// 提取块的输出文本（不含提示符与命令行）。
    /// 输出范围：`output_line ..= end_line-1`，外加 end_line 行的
    /// [0, end_col) 前缀（无结尾换行的输出，新提示符接在其后）；
    /// 无 C 标记时从命令行下一行起；未闭合块取到主屏内容末尾。
    pub fn block_output_text(&self, block: &Block) -> String {
        let grid = self.main_grid();
        let row_text = |line: u64, limit: Option<usize>| -> Option<String> {
            let row = grid.line_by_abs(line)?;
            let cells = row.cells();
            let take = limit.unwrap_or(cells.len()).min(cells.len());
            Some(
                cells[..take]
                    .iter()
                    .filter(|c| !c.flags.contains(CellFlags::WIDE_SPACER))
                    .map(|c| c.ch)
                    .collect(),
            )
        };
        let start = block
            .output_line
            .or(block.cmd_line.map(|l| l + 1))
            .unwrap_or(block.prompt_line + 1);
        let end = block
            .end_line
            .unwrap_or_else(|| grid.absolute_cursor_line() + 1);
        let mut out = String::new();
        for line in start..end {
            let Some(text) = row_text(line, None) else {
                continue;
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text.trim_end());
        }
        // 无结尾换行的最后一行输出：D 标记落在该行 end_col 列。
        if block.end_col > 0 {
            if let Some(end_line) = block.end_line {
                if let Some(text) = row_text(end_line, Some(block.end_col)) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text.trim_end());
                }
            }
        }
        // 去掉尾部空行。
        while out.ends_with('\n') {
            out.pop();
        }
        out
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

    /// 是否开启 ConPTY win32-input-mode（DEC 9001）。
    pub fn win32_input(&self) -> bool {
        self.inner.win32_input
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
    /// scrollback 容量（RIS 重建时需要原值，不可硬编码）。
    scrollback_limit: usize,
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
    /// shell 上报的当前工作目录（OSC 9;9）。
    cwd: Option<PathBuf>,
    blocks: Vec<Block>,
    next_block_id: u64,
    /// 需回写 PTY 的应答字节（DSR 等）。
    responses: Vec<u8>,
    bell: bool,
    /// DEC 2026 同步更新进行中。
    sync_output: bool,
    /// DEC 2004 bracketed paste 已开启。
    bracketed_paste: bool,
    /// ConPTY win32-input-mode（DEC 9001）已开启：键盘输入应编码为
    /// `CSI Vk;Sc;Uc;Kd;Cs;Rc _` 直达 conhost 输入队列。
    win32_input: bool,
    /// 事件序号，用于判断「显示光标」与「ESU」的先后。
    event_seq: u64,
    last_cursor_show_seq: u64,
    last_esu_seq: u64,
}

impl TermInner {
    fn new(rows: usize, cols: usize, scrollback_limit: usize) -> Self {
        Self {
            grid: Grid::new(rows, cols, scrollback_limit),
            scrollback_limit,
            saved_main: None,
            pen: Cell::default(),
            saved_cursor: None,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            title: String::new(),
            cwd: None,
            blocks: Vec::new(),
            next_block_id: 0,
            responses: Vec::new(),
            bell: false,
            sync_output: false,
            bracketed_paste: false,
            win32_input: false,
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
                            Some(5) => iter.next().map(|g| Color::Indexed(g[0].min(255) as u8)),
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
                let rgb = if sub.len() >= 5 {
                    &sub[2..5]
                } else {
                    sub.get(1..4)?
                };
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
            Some(b'A') => {
                // 行号回退（cls/clear 清屏后光标归零）：旧块指向的内容
                // 已被擦除且会破坏 prompt_line 的单调性，全部失效。
                if self
                    .blocks
                    .last()
                    .is_some_and(|last| line < last.prompt_line)
                {
                    self.blocks.clear();
                }
                // 上限保护：批量丢弃最旧块，避免长会话无限增长。
                if self.blocks.len() >= 1200 {
                    self.blocks.drain(..200);
                }
                self.next_block_id += 1;
                self.blocks.push(Block {
                    id: self.next_block_id,
                    prompt_line: line,
                    ..Block::default()
                });
            }
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
                // 只闭合未结束的块：会话首个提示符也会发 D（无前置命令），
                // 不能改写已闭合的历史块。
                let end_col = self.grid.cursor.col;
                if let Some(b) = self.blocks.last_mut().filter(|b| !b.is_closed()) {
                    b.end_line = Some(line);
                    // 光标列 >0 = 最后一行输出无结尾换行，记录前缀边界。
                    b.end_col = end_col;
                    b.exit_code = params
                        .get(2)
                        .and_then(|p| std::str::from_utf8(p).ok())
                        .and_then(|s| s.parse().ok());
                }
            }
            _ => {}
        }
    }

    /// 处理 OSC 9 扩展（ConEmu/Windows Terminal 系）。
    /// 目前只认 `9;9;<path>` cwd 上报，其余子命令（9;4 进度条等）忽略。
    fn handle_osc9(&mut self, params: &[&[u8]]) {
        if params.get(1).copied() != Some(b"9".as_ref()) {
            return;
        }
        // 路径里可能含分号（被 vte 按 ; 切开成多段），重新拼回。
        let raw: Vec<u8> = params[2..].join(&b';');
        let Ok(s) = std::str::from_utf8(&raw) else {
            trace!("OSC 9;9 路径非 UTF-8，忽略");
            return;
        };
        // Windows Terminal 官方脚本带双引号发送（ConEmu 规范引号可选）：
        // 两端都有才剥除，单边引号视为路径本身的一部分。
        let path = s
            .strip_prefix('"')
            .and_then(|x| x.strip_suffix('"'))
            .unwrap_or(s);
        if !path.is_empty() {
            self.cwd = Some(PathBuf::from(path));
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
                let r = it
                    .next()
                    .and_then(|g| g.first().copied())
                    .unwrap_or(1)
                    .max(1) as usize;
                let c = it
                    .next()
                    .and_then(|g| g.first().copied())
                    .unwrap_or(1)
                    .max(1) as usize;
                self.grid.cursor.row = (r - 1).min(rows - 1);
                self.grid.cursor.col = (c - 1).min(cols - 1);
            }
            'J' => {
                let mode = p0.unwrap_or(0);
                let blank = self.blank();
                match mode {
                    0 => {
                        self.grid
                            .fill_region(cur.row, cur.col, cur.row, cols - 1, blank);
                        if cur.row + 1 < rows {
                            self.grid
                                .fill_region(cur.row + 1, 0, rows - 1, cols - 1, blank);
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
                    0 => self
                        .grid
                        .fill_region(cur.row, cur.col, cur.row, cols - 1, blank),
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
            'S' => self
                .grid
                .scroll_region_up(self.scroll_top, self.scroll_bottom, n),
            'T' => self
                .grid
                .scroll_region_down(self.scroll_top, self.scroll_bottom, n),
            'm' => self.apply_sgr(params),
            'r' => {
                let mut it = params.iter();
                let top = it
                    .next()
                    .and_then(|g| g.first().copied())
                    .unwrap_or(1)
                    .max(1) as usize;
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
                            9001 => self.win32_input = on,
                            2026 => {
                                self.sync_output = on;
                                if !on {
                                    self.event_seq += 1;
                                    self.last_esu_seq = self.event_seq;
                                }
                            }
                            1048 => {
                                if on {
                                    self.saved_cursor = Some((cur.row, cur.col, self.pen));
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
                self.saved_cursor = Some((self.grid.cursor.row, self.grid.cursor.col, self.pen));
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
                // RIS：全终端重置。块 id 序列必须延续——上层可能持有
                // 旧块 id，归零会让新块复用旧 id 被误选中。
                let (rows, cols) = (self.grid.rows(), self.grid.cols());
                let limit = self.scrollback_limit;
                let next_id = self.next_block_id;
                *self = TermInner::new(rows, cols, limit);
                self.next_block_id = next_id;
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
            b"9" => self.handle_osc9(params),
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
    fn 缩行后提示符仍在可视区() {
        // B2 症状① 回归测试：spawn 大网格 → shell 打印提示符 →
        // 首帧布局按真实窗格矩形缩行。提示符必须留在可视区首行
        // （旧 Grid::resize 把顶行无条件搬进历史，可视区只剩命令块
        // 状态条与光标「两根竖条」）。
        let mut t = Terminal::new(30, 80, 100);
        t.advance(b"PS C:\\ClaudeWorkspaces\\engram>");
        t.resize(10, 40);
        let s = screen_text(&t);
        assert!(
            s[0].starts_with("PS C:\\ClaudeWorkspaces\\engram>"),
            "提示符应留在首行，实际: {:?}",
            s[0]
        );
        assert_eq!(t.grid().cursor.row, 0);
        assert_eq!(t.grid().scrollback_len(), 0);
    }

    #[test]
    fn 缩行后_cup_落在可视区首行() {
        // VT CUP（ESC[row;colH）是可视区（screen）内坐标，与 scrollback
        // 无关——无论缩行后历史有多少行，ESC[1;1H 始终落在 screen[0]。
        // 此测试验证该不变量：缩行触发顶行进 scrollback 之后，ConPTY
        // 重发的 ESC[1;1H 仍命中 screen[0]，字符不错位。
        let mut t = Terminal::new(5, 20, 100);
        // 写入 5 行内容（行0..4 各有文字，恰好填满可视区）
        t.advance(b"line0\r\n");
        t.advance(b"line1\r\n");
        t.advance(b"line2\r\n");
        t.advance(b"line3\r\n");
        t.advance(b"line4");
        assert_eq!(t.grid().cursor.row, 4);
        // 缩到 3 行：第一步无空行可收割，第二步顶行进 scrollback。
        t.resize(3, 20);
        // 缩行后历史有 2 行（原 line0/line1 进 scrollback）。
        assert_eq!(t.grid().scrollback_len(), 2);
        // 模拟 ConPTY 重发 repaint：ESC[1;1H 定位到新屏幕 row-0，
        // 写入「重绘的提示符」，验证落在我们的 screen[0]。
        t.advance(b"\x1b[1;1HPS> ");
        let rows: Vec<String> = t
            .grid()
            .visible_rows()
            .map(|r| r.cells().iter().map(|c| c.ch).collect::<String>())
            .collect();
        assert!(
            rows[0].starts_with("PS> "),
            "ESC[1;1H 应落在 screen[0]，实际: {:?}",
            rows[0]
        );
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
    fn osc133_d不改写已闭合块() {
        let mut t = term();
        // 完整块（exit=0）之后游离的 D;5 不应改写它。
        t.advance(b"\x1b]133;A\x07$ ok\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07\x1b]133;D;5\x07");
        assert_eq!(t.blocks().len(), 1);
        assert_eq!(t.blocks()[0].exit_code, Some(0));
    }

    #[test]
    fn 按绝对行查块与块文本提取() {
        let mut t = Terminal::new(8, 20, 100);
        // 块1：prompt 行0，命令行0，输出行1-2，D 在行3。
        t.advance(b"\x1b]133;A\x07$ ");
        t.advance(b"\x1b]133;B\x07cmd\r\n");
        t.advance(b"\x1b]133;C\x07line1\r\nline2\r\n");
        t.advance(b"\x1b]133;D;3\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        let blocks = t.blocks();
        assert_eq!(blocks.len(), 2);
        let b1 = &blocks[0];
        assert_eq!(b1.exit_code, Some(3));
        // 行 0-2 属于块1；行 3（新提示符行）属于块2。
        assert_eq!(t.block_at_line(0).map(|b| b.id), Some(b1.id));
        assert_eq!(t.block_at_line(2).map(|b| b.id), Some(b1.id));
        assert_eq!(t.block_at_line(3).map(|b| b.id), Some(blocks[1].id));
        // 输出文本 = 行1..行2。
        assert_eq!(t.block_output_text(b1), "line1\nline2");
    }

    #[test]
    fn 未闭合块不向光标下方空白区延伸() {
        let mut t = Terminal::new(10, 20, 100);
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        // 光标在行 0；行 0 命中、行 5（空白区）不命中。
        assert!(t.block_at_line(0).is_some());
        assert!(t.block_at_line(5).is_none());
    }

    #[test]
    fn 未闭合块下边界按调用方上限截断() {
        let mut t = Terminal::new(10, 20, 100);
        // 未闭合块：prompt 行0，输出 a(行1) b(行2)，光标 live 在行3。
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07cmd\r\n\x1b]133;C\x07a\r\nb\r\n");
        assert_eq!(t.grid().cursor.row, 3);
        // 渲染侧给「防抖光标行」做上限：行1 命中，行2/3 截掉。
        assert!(t.block_at_line_capped(1, 1).is_some());
        assert!(t.block_at_line_capped(2, 1).is_none());
        assert!(t.block_at_line_capped(3, 1).is_none());
        // 上限取 live 光标行时与 block_at_line 等价。
        assert_eq!(
            t.block_at_line(3).map(|b| b.id),
            t.block_at_line_capped(3, 3).map(|b| b.id)
        );
        // 已闭合块不受上限影响（上限只约束未闭合块的下边界）。
        t.advance(b"\x1b]133;D;0\x07");
        assert!(t.block_at_line_capped(1, 0).is_some());
    }

    #[test]
    fn cls后行号回退使旧块失效() {
        let mut t = Terminal::new(5, 20, 100);
        t.advance(b"a\r\nb\r\nc\r\n\x1b]133;A\x07$ \x1b]133;D;0\x07");
        assert_eq!(t.blocks().len(), 1);
        // cls：ED2 + CUP home，光标绝对行回退到 0。
        t.advance(b"\x1b[2J\x1b[H\x1b]133;A\x07$ ");
        assert_eq!(t.blocks().len(), 1, "旧块应被清除，仅剩新块");
        assert_eq!(t.blocks()[0].prompt_line, 0);
    }

    #[test]
    fn 无结尾换行的输出不丢最后一行() {
        let mut t = Terminal::new(8, 30, 100);
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07cmd\r\n");
        t.advance(b"\x1b]133;C\x07partial"); // 无结尾换行
        t.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07PS> ");
        let b = &t.blocks()[0];
        assert_eq!(b.end_col, 7); // "partial".len()
        assert_eq!(t.block_output_text(b), "partial");
    }

    #[test]
    fn ris后块id不复用() {
        let mut t = term();
        t.advance(b"\x1b]133;A\x07$ ");
        let first_id = t.blocks()[0].id;
        t.advance(b"\x1bc"); // RIS
        assert!(t.blocks().is_empty());
        t.advance(b"\x1b]133;A\x07$ ");
        assert!(t.blocks()[0].id > first_id, "重置后 id 必须延续不复用");
    }

    #[test]
    fn alt_screen下块查询用主屏坐标() {
        let mut t = Terminal::new(5, 20, 100);
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07cmd\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07");
        let id = t.blocks()[0].id;
        let text_before = t.block_output_text(t.block_by_id(id).unwrap());
        // 进入备用屏幕并写满不同内容。
        t.advance(b"\x1b[?1049h\x1b[HALT-CONTENT");
        assert!(t.is_alt_screen());
        let text_during = t.block_output_text(t.block_by_id(id).unwrap());
        assert_eq!(
            text_before, text_during,
            "alt screen 期间块文本必须取自主屏"
        );
    }

    #[test]
    fn 滚动到绝对行() {
        let mut t = Terminal::new(3, 10, 100);
        for i in 0..10u32 {
            t.advance(format!("{i}\r\n").as_bytes());
        }
        // 10 行内容 + 光标行：scrollback 应有 8 行（行0-7 滚出）。
        let sb = t.grid().scrollback_len();
        assert_eq!(sb, 8);
        t.grid_mut().scroll_to_abs_line(0);
        assert_eq!(t.grid().display_offset(), 8);
        t.grid_mut().scroll_to_abs_line(5);
        assert_eq!(t.grid().display_offset(), 3);
        // 已在可视区内的行：回到底部。
        t.grid_mut().scroll_to_abs_line(9);
        assert_eq!(t.grid().display_offset(), 0);
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

    #[test]
    fn osc9_9_cwd上报_基础与覆盖() {
        let mut t = term();
        assert_eq!(t.cwd(), None);
        t.advance(b"\x1b]9;9;C:\\Users\\dev\x07");
        assert_eq!(t.cwd(), Some(Path::new("C:\\Users\\dev")));
        // cd 后再次上报覆盖旧值。
        t.advance(b"\x1b]9;9;D:\\proj\x07");
        assert_eq!(t.cwd(), Some(Path::new("D:\\proj")));
    }

    #[test]
    fn osc9_9_带引号与空格路径() {
        let mut t = term();
        // Windows Terminal 官方脚本风格：双引号包裹 + ST（ESC \）终止。
        t.advance(b"\x1b]9;9;\"C:\\Program Files\\My App\"\x1b\\");
        assert_eq!(t.cwd(), Some(Path::new("C:\\Program Files\\My App")));
    }

    #[test]
    fn osc9_9_中文路径() {
        let mut t = term();
        t.advance("\x1b]9;9;C:\\用户\\海风 哥\x07".as_bytes());
        assert_eq!(t.cwd(), Some(Path::new("C:\\用户\\海风 哥")));
    }

    #[test]
    fn osc9_9_含分号路径重新拼接() {
        let mut t = term();
        // vte 把 OSC 参数按 ; 切开，路径里的分号需要拼回。
        t.advance(b"\x1b]9;9;C:\\a;b\\c\x07");
        assert_eq!(t.cwd(), Some(Path::new("C:\\a;b\\c")));
    }

    #[test]
    fn osc9_其他子命令与空路径忽略() {
        let mut t = term();
        // OSC 9;4 是进度条（ConEmu/WT），不得误吞成 cwd。
        t.advance(b"\x1b]9;4;1;50\x07");
        assert_eq!(t.cwd(), None);
        // 空路径忽略，不覆盖已有值。
        t.advance(b"\x1b]9;9;C:\\ok\x07\x1b]9;9;\x07");
        assert_eq!(t.cwd(), Some(Path::new("C:\\ok")));
    }

    #[test]
    fn shell等待输入判定_完整命令周期() {
        let mut t = term();
        // 无块：未注入 integration 时不可判定为空闲。
        assert!(!t.shell_waiting_input());
        // A：提示符开始渲染，尚未到输入区。
        t.advance(b"\x1b]133;A\x07$ ");
        assert!(!t.shell_waiting_input());
        // B：提示符结束，shell 等待输入。
        t.advance(b"\x1b]133;B\x07");
        assert!(t.shell_waiting_input());
        // C：用户回车，命令开始执行（忙）。
        t.advance(b"ls\r\n\x1b]133;C\x07out\r\n");
        assert!(!t.shell_waiting_input());
        // D + 新提示符 A/B：回到等待输入。
        t.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        assert!(t.shell_waiting_input());
    }

    #[test]
    fn shell等待输入判定_备用屏幕视为忙() {
        let mut t = term();
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        assert!(t.shell_waiting_input());
        // 进入备用屏幕（理论上 C 标记已先到，这里是双保险路径）。
        t.advance(b"\x1b[?1049h");
        assert!(!t.shell_waiting_input());
        t.advance(b"\x1b[?1049l");
        assert!(t.shell_waiting_input());
    }

    // ── M4 P0 补测 ──────────────────────────────────────────────────────────

    /// 场景①：prompt 态——A 到达后 B 未到，不算等待输入；B 到达后 C 未到，算等待。
    #[test]
    fn shell等待输入_场景1_prompt态_a后b前不等待_b后c前等待() {
        let mut t = term();
        // A 到：提示符开始渲染，未到输入区——不算等待。
        t.advance(b"\x1b]133;A\x07PS> ");
        assert!(
            !t.shell_waiting_input(),
            "A 到但 B 未到：不算等待输入（提示符还在渲染）"
        );
        // B 到，无 C：shell 等待用户输入——算等待。
        t.advance(b"\x1b]133;B\x07");
        assert!(t.shell_waiting_input(), "B 到且 C 未到：应判定为等待输入");
    }

    /// 场景②：运行态——C 到达后 D 未到，shell 正在执行命令，不等待输入。
    #[test]
    fn shell等待输入_场景2_运行态_c后d前不等待() {
        let mut t = term();
        t.advance(b"\x1b]133;A\x07PS> \x1b]133;B\x07");
        assert!(t.shell_waiting_input(), "前置条件：B 到时应为等待态");
        // 用户回车：C 标记到达，命令开始执行。
        t.advance(b"ls\r\n\x1b]133;C\x07");
        assert!(!t.shell_waiting_input(), "C 到达（命令执行中）：不等待输入");
        // 输出进来中仍不等待。
        t.advance(b"file.txt\r\n");
        assert!(!t.shell_waiting_input(), "输出期间（D 未到）：不等待输入");
    }

    /// 场景③：alt screen 态——即便块处于等待状态，alt screen 一律视为忙。
    #[test]
    fn shell等待输入_场景3_alt_screen态_一律视为忙() {
        let mut t = term();
        // 先建立 prompt 等待态（B 到，C 未到）。
        t.advance(b"\x1b]133;A\x07PS> \x1b]133;B\x07");
        assert!(t.shell_waiting_input(), "前置：B 到时等待输入");
        // 进入 alt screen：不管块状态，均不等待输入。
        t.advance(b"\x1b[?1049h");
        assert!(
            !t.shell_waiting_input(),
            "alt screen 激活时：一律视为忙（全屏程序占用终端）"
        );
    }

    /// 场景④：无块（从未见 133）——integration 注入失败或 SSH 裸环境，不可判断。
    #[test]
    fn shell等待输入_场景4_无块_从未见133_视为未知() {
        let mut t = term();
        // 写入普通文字但无任何 OSC 133 标记。
        t.advance(b"some output without any 133 marker\r\n");
        assert!(
            !t.shell_waiting_input(),
            "无块（未见 OSC 133）：不可判定为等待输入（Fallback 场景）"
        );
    }

    /// 场景⑤：D 闭合后新 prompt 周期——D 之后再来 A+B，应再次判定为等待输入。
    #[test]
    fn shell等待输入_场景5_d闭合后新prompt周期再次等待() {
        let mut t = term();
        // 第一个完整命令周期。
        t.advance(b"\x1b]133;A\x07PS> \x1b]133;B\x07");
        assert!(t.shell_waiting_input(), "第一周期 B 到：等待输入");
        t.advance(b"ls\r\n\x1b]133;C\x07output\r\n\x1b]133;D;0\x07");
        assert!(
            !t.shell_waiting_input(),
            "第一周期 D 到（块已闭合）：不等待输入"
        );
        // 新 prompt 周期：下一个 A+B 到达。
        t.advance(b"\x1b]133;A\x07PS> \x1b]133;B\x07");
        assert!(
            t.shell_waiting_input(),
            "D 闭合后新 prompt 周期（新块 B 到）：应再次判定为等待输入"
        );
    }
}
