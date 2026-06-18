//! M5.3 终端镜像辅助：被控端整屏「VT 快照」序列化（part3a） + 历史行按需序列化
//! （part3d）。
//!
//! 镜像方案（方案 B，见 `docs/M5远程控制设计.md` §part3）：被控端转发焦点窗格的
//! PTY 输出字节，控制端喂入一个无 PTY 的 [`Terminal`] 复现整状态。控制端**中途
//! 接入**时缺历史，故被控端会话起始先把当前可见屏序列化成一段等效 VT 字节
//! （[`screen_snapshot_vt`]）发一次，控制端 `advance` 重放即得起始整屏，之后接实时
//! 增量字节。
//!
//! **part3d 历史按需分页**：被控端的 scrollback 历史不预传；控制端上滚回看时按视口
//! 窗口请求绝对行区间，被控端用 [`history_rows_vt`] 按绝对行号（`Grid::line_by_abs`）
//! 序列化对应行回传。单行序列化 [`serialize_row_vt`] 为快照与历史两路复用。

use std::fmt::Write as _;

use lumen_term::{CellFlags, Color, Row, Terminal};

/// 把终端**当前可见屏**序列化为等效 VT 字节：喂给一个全新 [`Terminal::advance`]
/// 即复现该屏（颜色/属性/光标位置）。仅含可见区，不含 scrollback 历史。
#[must_use]
pub fn screen_snapshot_vt(term: &Terminal) -> Vec<u8> {
    let grid = term.grid();
    let cols = grid.cols();
    let mut out: Vec<u8> = Vec::new();
    // 清屏 + 光标归位。
    out.extend_from_slice(b"\x1b[2J\x1b[H");
    for (r, row) in grid.visible_rows().enumerate() {
        let line = serialize_row_vt(row, cols);
        if line.is_empty() {
            continue; // 空行：clear 已置空，无需定位重绘。
        }
        let mut head = String::new();
        let _ = write!(head, "\x1b[{};1H", r + 1);
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(&line);
    }
    // 还原光标到被控端当前位置（1-based），使后续实时字节从正确处续写。
    let cur = &grid.cursor;
    let mut tail = String::new();
    let _ = write!(tail, "\x1b[{};{}H", cur.row + 1, cur.col + 1);
    out.extend_from_slice(tail.as_bytes());
    out
}

/// 把**单行**序列化为「绝对 SGR + 字符」VT 字节（不含定位 / 换行；末尾 `\x1b[0m`
/// 复位）：行尾默认空白被 trim；空行返回空 `Vec`。供整屏快照（[`screen_snapshot_vt`]）
/// 与历史行（[`history_rows_vt`]）两路复用。喂给 `Terminal::advance` 时由调用方先发
/// 行定位序列再追加本段，即在目标行复现该行内容。
#[must_use]
pub fn serialize_row_vt(row: &Row, cols: usize) -> Vec<u8> {
    let cells = row.cells();
    // 行尾默认空白 trim：clear 已置空，无需重绘。
    let last = cells
        .iter()
        .take(cols)
        .rposition(|c| {
            c.ch != ' '
                || c.bg != Color::Default
                || c.flags
                    .intersects(CellFlags::INVERSE | CellFlags::UNDERLINE | CellFlags::STRIKE)
        })
        .map_or(0, |i| i + 1);
    let mut out = Vec::new();
    if last == 0 {
        return out;
    }
    let mut prev: Option<(Color, Color, CellFlags)> = None;
    let mut ci = 0;
    while ci < last {
        let cell = cells[ci];
        // 宽字符右半占位格：主格已输出该字符，跳过。
        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            ci += 1;
            continue;
        }
        let style = (
            cell.fg,
            cell.bg,
            cell.flags & !(CellFlags::WIDE | CellFlags::WIDE_SPACER),
        );
        if prev != Some(style) {
            out.extend_from_slice(&sgr_for(style.0, style.1, style.2));
            prev = Some(style);
        }
        let mut buf = [0u8; 4];
        out.extend_from_slice(cell.ch.encode_utf8(&mut buf).as_bytes());
        ci += 1;
    }
    out.extend_from_slice(b"\x1b[0m");
    out
}

/// part3d：被控端按**绝对行号**序列化历史行 `[top, top + count)`，回带当前历史边界。
///
/// 返回 `(lines, base, screen_top)`：`lines[i]` 是绝对行 `top + i` 的 VT 字节（空 `Vec`
/// = 该行空白或越界，控制端渲染为空白），长度恒为 `count` 以与请求对齐；`base` =
/// 最旧保留行绝对行号（`Grid::line_by_abs` 内部据 `dropped_lines` 换算），`screen_top`
/// = 可视区首行绝对行号。控制端据此夹紧回看范围。
#[must_use]
pub fn history_rows_vt(term: &Terminal, top: u64, count: usize) -> (Vec<Vec<u8>>, u64, u64) {
    let grid = term.grid();
    let cols = grid.cols();
    // screen_top = 光标绝对行 − 光标在屏内行号 = dropped_lines + scrollback.len()。
    let screen_top = grid
        .absolute_cursor_line()
        .saturating_sub(grid.cursor.row as u64);
    let base = screen_top.saturating_sub(grid.scrollback_len() as u64);
    let mut lines = Vec::with_capacity(count);
    for i in 0..count as u64 {
        let bytes = grid
            .line_by_abs(top + i)
            .map_or_else(Vec::new, |r| serialize_row_vt(r, cols));
        lines.push(bytes);
    }
    (lines, base, screen_top)
}

/// 构造「将 `fg`/`bg`/`flags` 设为绝对值」的 SGR 序列（以 `0` 复位起头，避免继承）。
fn sgr_for(fg: Color, bg: Color, flags: CellFlags) -> Vec<u8> {
    let mut s = String::from("\x1b[0");
    if flags.contains(CellFlags::BOLD) {
        s.push_str(";1");
    }
    if flags.contains(CellFlags::DIM) {
        s.push_str(";2");
    }
    if flags.contains(CellFlags::ITALIC) {
        s.push_str(";3");
    }
    if flags.contains(CellFlags::UNDERLINE) {
        s.push_str(";4");
    }
    if flags.contains(CellFlags::INVERSE) {
        s.push_str(";7");
    }
    if flags.contains(CellFlags::STRIKE) {
        s.push_str(";9");
    }
    push_color(&mut s, fg, true);
    push_color(&mut s, bg, false);
    s.push('m');
    s.into_bytes()
}

/// 追加前景（`fg=true`）或背景的 SGR 颜色参数。
fn push_color(s: &mut String, color: Color, fg: bool) {
    let (base, dflt) = if fg { (38, 39) } else { (48, 49) };
    match color {
        Color::Default => {
            let _ = write!(s, ";{dflt}");
        }
        Color::Indexed(n) => {
            let _ = write!(s, ";{base};5;{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(s, ";{base};2;{r};{g};{b}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 提取终端可见区文本（每行 trim 行尾、跳过宽字符占位格）——校验快照往返用。
    fn visible_text(term: &Terminal) -> Vec<String> {
        let grid = term.grid();
        let cols = grid.cols();
        grid.visible_rows()
            .map(|row| {
                let mut s = String::with_capacity(cols);
                for cell in row.cells().iter().take(cols) {
                    if cell.flags.contains(CellFlags::WIDE_SPACER) {
                        continue;
                    }
                    s.push(cell.ch);
                }
                s.trim_end().to_string()
            })
            .collect()
    }

    /// 快照往返：源终端打一屏内容 → 快照 → 喂入全新终端 → 两边可见文本一致。
    #[test]
    fn 快照往返复现可见屏() {
        let mut src = Terminal::new(6, 20, 100);
        src.advance(b"hello \x1b[31mred\x1b[0m world\r\n");
        src.advance(b"line2 \x1b[1mbold\x1b[0m\r\n");
        src.advance(b"prompt> ");
        let snap = screen_snapshot_vt(&src);

        let mut dst = Terminal::new(6, 20, 100);
        dst.advance(&snap);

        assert_eq!(visible_text(&src), visible_text(&dst), "镜像可见文本应与源一致");
        // 光标列应落在 "prompt> " 之后（第 3 行，列 8）。
        let cur = &dst.grid().cursor;
        assert_eq!((cur.row, cur.col), (2, 8));
    }

    #[test]
    fn 空屏快照只清屏() {
        let src = Terminal::new(4, 10, 50);
        let snap = screen_snapshot_vt(&src);
        let mut dst = Terminal::new(4, 10, 50);
        dst.advance(&snap);
        assert!(visible_text(&dst).iter().all(String::is_empty));
    }

    #[test]
    fn 历史行按绝对行号序列化() {
        // 3 行可视、打 6 行（无尾换行）：L0/L1/L2 入历史，L3/L4/L5 在可视区。
        let mut src = Terminal::new(3, 20, 100);
        src.advance(b"L0\r\nL1\r\nL2\r\nL3\r\nL4\r\nL5");
        let (lines, base, screen_top) = history_rows_vt(&src, 0, 3);
        assert_eq!(base, 0, "未触发丢弃，最旧绝对行=0");
        assert_eq!(screen_top, 3, "可视区首行 L3 的绝对行号=3");
        assert_eq!(lines.len(), 3, "返回长度恒等于请求 count");
        // 逐行喂入 1 行高终端复现，校验是历史行 L0/L1/L2。
        for (i, line) in lines.iter().enumerate() {
            let mut dst = Terminal::new(1, 20, 0);
            dst.advance(b"\x1b[2J\x1b[H");
            dst.advance(line);
            assert_eq!(visible_text(&dst), vec![format!("L{i}")], "绝对行 {i} 应复现 L{i}");
        }
        // 越界请求（screen_top + rows = 6 之后无保留行）：返回空行。
        let (oob, _b, _s) = history_rows_vt(&src, 6, 2);
        assert!(oob.iter().all(Vec::is_empty), "越界绝对行应序列化为空");
    }
}
