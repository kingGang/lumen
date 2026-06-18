//! M5.3 part3a 终端镜像辅助：被控端整屏「VT 快照」序列化 + 控制端镜像可见区
//! 文本提取。
//!
//! 镜像方案（方案 B，见 `docs/M5远程控制设计.md` §part3）：被控端转发焦点窗格的
//! PTY 输出字节，控制端喂入一个无 PTY 的 [`Terminal`] 复现整状态。控制端**中途
//! 接入**时缺历史，故被控端会话起始先把当前可见屏序列化成一段等效 VT 字节
//! （[`screen_snapshot_vt`]）发一次，控制端 `advance` 重放即得起始整屏，之后接实时
//! 增量字节。控制端渲染读 [`mirror_view`]（part3a 先出纯文本视图，part3b 上 wgpu 上色）。

use std::fmt::Write as _;

use lumen_term::{CellFlags, Color, Terminal};

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
        if last == 0 {
            continue;
        }
        let mut head = String::new();
        let _ = write!(head, "\x1b[{};1H", r + 1);
        out.extend_from_slice(head.as_bytes());
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
    }
    // 还原光标到被控端当前位置（1-based），使后续实时字节从正确处续写。
    let cur = &grid.cursor;
    let mut tail = String::new();
    let _ = write!(tail, "\x1b[{};{}H", cur.row + 1, cur.col + 1);
    out.extend_from_slice(tail.as_bytes());
    out
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

/// 控制端镜像可见区的纯文本视图（part3a 渲染用；part3b 升级为带色 wgpu 渲染）。
pub struct MirrorView {
    /// 每个可见行的文本（已 trim 行尾空白；宽字符占位格已跳过）。
    pub lines: Vec<String>,
    /// 光标 (行, 列)（可见区内 0-based）。
    pub cursor: (usize, usize),
}

/// 从镜像 [`Terminal`] 提取可见区文本视图。
#[must_use]
pub fn mirror_view(term: &Terminal) -> MirrorView {
    let grid = term.grid();
    let cols = grid.cols();
    let lines = grid
        .visible_rows()
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
        .collect();
    MirrorView {
        lines,
        cursor: (grid.cursor.row, grid.cursor.col),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let src_lines = mirror_view(&src).lines;
        let dst_lines = mirror_view(&dst).lines;
        assert_eq!(src_lines, dst_lines, "镜像可见文本应与源一致");
        // 光标列应落在 "prompt> " 之后（第 3 行）。
        let dv = mirror_view(&dst);
        assert_eq!(dv.cursor, (2, 8));
    }

    #[test]
    fn 空屏快照只清屏() {
        let src = Terminal::new(4, 10, 50);
        let snap = screen_snapshot_vt(&src);
        let mut dst = Terminal::new(4, 10, 50);
        dst.advance(&snap);
        assert!(mirror_view(&dst).lines.iter().all(String::is_empty));
    }
}
