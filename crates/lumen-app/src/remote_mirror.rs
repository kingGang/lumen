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

use lumen_term::{Cell, CellFlags, Color, Grid, MouseEncoding, MouseProtocol, Row, Terminal};

/// 把终端**当前可见屏**序列化为等效 VT 字节：喂给一个全新 [`Terminal::advance`]
/// 即复现该屏（颜色/属性/光标位置）。仅含可见区，不含 scrollback 历史。
///
/// **备用屏（?1049）复现**：被控端处于备用屏（claude/vim 全屏）时，快照会依次
/// 序列化「主屏（`saved_main`）→ `?1049h` → alt 屏」，使中途接入的镜像 `Terminal`
/// 落在与被控端一致的**主/备两级**状态。否则镜像把 alt 内容画在主屏、其
/// `saved_main` 为空，被控端退出全屏发的 `?1049l` 在镜像上成 no-op → alt 内容
/// 全部残留（2026-07 修：本地/远程「进 claude 副屏再退出」残留）。
///
/// 除内容与光标行列外，还复现：光标可见性（`?25`，per-网格状态，claude/Ink 全屏
/// 运行期隐藏光标）、pending_wrap（DEC 延迟换行，见 [`push_cursor_state`]）、
/// 滚动区（DECSTBM，vim 设一次不重发）、画笔（SGR pen，主/备两级，见
/// [`push_pen`]）；均按「非默认才发」序列化，与 [`terminal_modes_vt`] 同一
/// 「喂入全新镜像 Terminal」假设。
#[must_use]
pub fn screen_snapshot_vt(term: &Terminal) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    // 先重发当前终端模式（鼠标上报协议/编码/焦点/win32），使控制端**中途接入**时
    // 镜像 `Terminal` 复现这些状态。否则订阅前已发的 DECSET（如 Claude 全屏的
    // ?1003h/?1006h）丢失，控制端无从判定该不该把滚轮上报转发给被控端
    // （2026-06-30 控制端滚轮路由依赖镜像 term 的 mouse_protocol）。
    out.extend_from_slice(&terminal_modes_vt(term));
    if term.is_alt_screen() {
        // ① 先铺主屏内容（`saved_main`）——成为镜像退出 alt 后的底屏，与被控端
        //    退出全屏后恢复的主屏一致（否则镜像退出后主屏空白）。
        let main = term.main_grid();
        serialize_grid_body(&mut out, main);
        // ② 主屏光标可见性 + 光标态（切 alt 前位置）——`?1049h` 时随主屏网格
        //    整体存入镜像的 saved_main，退出全屏后原样恢复（光标行列即取自
        //    saved_main 网格自带 cursor，见 term.rs set_alt_screen）。
        push_cursor_visibility(&mut out, main.cursor.visible);
        push_cursor_state(&mut out, main);
        // ②' 主屏画笔：镜像 `?1049h` 时把**当时画笔**存入保存槽、`?1049l` 恢复。
        //    行序列化已把画笔归零，不补则镜像退出全屏后画笔与被控端分叉（此后
        //    无 SGR 的输出/bce 擦除两端颜色不一致）。
        if let Some(pen) = term.saved_pen() {
            push_pen(&mut out, pen);
        }
        // ③ 进备用屏：镜像存下当前主屏 + 切到空 alt 网格（可见性/滚动区随之
        //    复位为默认，故 alt 段须按被控端现状重发）。
        out.extend_from_slice(b"\x1b[?1049h");
        // ④ 铺 alt 屏内容 + 可见性 + 滚动区 + 光标态 + 当前画笔（被控端当前
        //    可见屏）。CSI r 副作用是光标归位，必须发在 push_cursor_state 之前；
        //    画笔必须最后（前面各段都会归零/改写它）。
        let alt = term.grid();
        serialize_grid_body(&mut out, alt);
        push_cursor_visibility(&mut out, alt.cursor.visible);
        push_scroll_region(&mut out, term);
        push_cursor_state(&mut out, alt);
        push_pen(&mut out, term.pen());
    } else {
        let grid = term.grid();
        serialize_grid_body(&mut out, grid);
        push_cursor_visibility(&mut out, grid.cursor.visible);
        push_scroll_region(&mut out, term);
        // 还原光标态到被控端当前位置，使后续实时字节从正确处续写；画笔最后。
        push_cursor_state(&mut out, grid);
        push_pen(&mut out, term.pen());
    }
    out
}

/// 把一张网格的**真实屏幕区**序列化为「清屏 + 逐行定位重绘」VT 字节（不含光标
/// 复位，由调用方按主/备屏各自补 [`push_cursor`]）。空行跳过（`2J` 已置空）。
///
/// 逐行取 `grid.row(r)`（物理屏幕行），**不经** `visible_rows()`：后者按
/// `display_offset` 拼「历史尾 N 行 + 屏幕前 rows-N 行」，被控端用户上滚回看时
/// （或回看态被程序自行进 alt 冻结进 saved_main 时）会把回看**视图**固化成镜像
/// 底屏，而光标又按屏幕坐标定位，两者错位、此后镜像永久错行（2026-07 修）。
fn serialize_grid_body(out: &mut Vec<u8>, grid: &Grid) {
    let cols = grid.cols();
    // 清屏 + 光标归位。
    out.extend_from_slice(b"\x1b[2J\x1b[H");
    for r in 0..grid.rows() {
        let line = serialize_row_vt(grid.row(r), cols);
        if line.is_empty() {
            continue; // 空行：clear 已置空，无需定位重绘。
        }
        let mut head = String::new();
        let _ = write!(head, "\x1b[{};1H", r + 1);
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(&line);
    }
}

/// 追加把光标移到 `(row, col)`（0-based 入参，输出 1-based CUP）的 VT 字节。
fn push_cursor(out: &mut Vec<u8>, row: usize, col: usize) {
    let mut tail = String::new();
    let _ = write!(tail, "\x1b[{};{}H", row + 1, col + 1);
    out.extend_from_slice(tail.as_bytes());
}

/// 复现光标的「行列 + pending_wrap（DEC 延迟换行）」两维（可见性另由
/// [`push_cursor_visibility`]）。CUP 无法表达 pending_wrap，且两个方向都会失真：
/// 镜像逐行重绘满宽行会把它**误置** true、随后 CUP 目标恰为现位时又不清（VT 语义
/// 仅「实际移动」才清）；反之源端恰在折行边界（true）时 CUP 只能给 false——
/// 失真后快照末的第一个可见字符一端折行、一端覆写行尾，整行永久错位。
/// 两分支都先 `\x1b[H` 强制一次移动清掉重绘残留的置位，然后：
/// - 源端 `false`：CUP 到目标即可；
/// - 源端 `true`：CUP 到行尾字符**主格**并按原属性重放该格字符——写满末列自然
///   置位，重放同字节得同终态（宽字符主格在 cols-2，占位格跳过）；末尾
///   `\x1b[0m` 归零，画笔由调用方随后的 [`push_pen`] 统一复现。
fn push_cursor_state(out: &mut Vec<u8>, grid: &Grid) {
    // 两个分支都先归位强制一次「实际移动」，清掉镜像逐行重绘残留的 pending_wrap
    // （重绘满宽行后镜像必处于置位态：true 分支若不清，随后 CUP 到现位无移动、
    // 重放的行尾字符会被折行写到下一行；false 分支若不清则直接残留）。镜像已在
    // 原点时该次无移动，但那只发生在空屏重绘，本就未置位。
    out.extend_from_slice(b"\x1b[H");
    let cur = &grid.cursor;
    if cur.pending_wrap {
        let cols = grid.cols();
        let cells = grid.row(cur.row).cells();
        // 行尾若是宽字符占位格，主格在前一列。
        let main_col = if cols >= 2
            && cells
                .get(cols - 1)
                .is_some_and(|c| c.flags.contains(CellFlags::WIDE_SPACER))
        {
            cols - 2
        } else {
            cols - 1
        };
        if let Some(cell) = cells.get(main_col) {
            push_cursor(out, cur.row, main_col);
            out.extend_from_slice(&sgr_for(
                cell.fg,
                cell.bg,
                cell.flags & !(CellFlags::WIDE | CellFlags::WIDE_SPACER),
            ));
            let mut buf = [0u8; 4];
            out.extend_from_slice(cell.ch.encode_utf8(&mut buf).as_bytes());
            out.extend_from_slice(b"\x1b[0m");
        }
    } else {
        push_cursor(out, cur.row, cur.col);
    }
}

/// 追加把镜像画笔设为 `pen`（前景/背景/属性绝对值 SGR）的字节；默认画笔（镜像
/// 重放行尾 `\x1b[0m` 后的状态）时不发。快照后实时字节里依赖 back-color-erase
/// 的擦除（EL/ED 用 pen.bg 填充）在两端须同底色，vim 类全屏程序局部重绘才不
/// 出现底色错块。须在快照**最末**调用（其余各段都会归零/改写画笔）。
fn push_pen(out: &mut Vec<u8>, pen: Cell) {
    let flags = pen.flags & !(CellFlags::WIDE | CellFlags::WIDE_SPACER);
    if pen.fg == Color::Default && pen.bg == Color::Default && flags.is_empty() {
        return; // 默认画笔：行序列化末尾的 \x1b[0m 已是该状态。
    }
    out.extend_from_slice(&sgr_for(pen.fg, pen.bg, flags));
}

/// 追加光标可见性（DECTCEM `?25l`）VT 字节；可见（镜像默认）时不发。
/// per-网格状态：claude/Ink 类全屏程序运行期隐藏光标、只在退出时恢复——快照
/// 不带则中途接入的镜像按默认「可见」在被控端并不显示光标的界面上画出幽灵
/// 光标，且整个 alt 会话不自愈。主/备两段各发一次（`?1049h` 新建的 alt 网格
/// 复位为可见）。
fn push_cursor_visibility(out: &mut Vec<u8>, visible: bool) {
    if !visible {
        out.extend_from_slice(b"\x1b[?25l");
    }
}

/// 追加滚动区（DECSTBM）VT 字节；默认全屏区（镜像默认）时不发。滚动区是终端级
/// 单份活状态且镜像 `?1049h` 时复位为全屏：vim 类应用进 alt 设一次 CSI r 后不再
/// 重发，快照不带则中途接入的镜像按全屏滚动、把状态行等区外行一起搬动。
/// CSI r 副作用是光标归位，调用方须先发本段再 [`push_cursor`]。主屏段无需补发：
/// 退出 alt 时被控端与镜像的 set_alt_screen 都会重置为全屏，两端一致。
fn push_scroll_region(out: &mut Vec<u8>, term: &Terminal) {
    let (top, bottom) = term.scroll_region();
    if top == 0 && bottom == term.grid().rows() - 1 {
        return; // 默认全屏区。
    }
    let mut s = String::new();
    let _ = write!(s, "\x1b[{};{}r", top + 1, bottom + 1);
    out.extend_from_slice(s.as_bytes());
}

/// 把终端**当前鼠标/输入相关私有模式**序列化为等效 DECSET 字节，供 [`screen_snapshot_vt`]
/// 前置——使控制端中途接入时镜像 `Terminal` 重放即复现这些状态。含鼠标上报协议/
/// SGR 编码/焦点事件/win32 输入，以及括号粘贴（?2004）：控制端粘贴按**镜像** term
/// 的 `bracketed_paste` 决定是否包 200~/201~（remote_ws.rs `send_paste`），订阅前
/// 被控端应用（claude/Ink 等）已发的 `?2004h` 若不重放，中途接入后的多行粘贴不
/// 包裹、被被控端逐行执行（2026-07 修；此前注释「镜像不粘贴」与 send_paste 实现
/// 矛盾）。不含备用屏（?1049）：它由 [`screen_snapshot_vt`] 在铺完主屏后**显式发**
/// `?1049h` 并铺 alt 屏来复现（放这里会在主屏内容前就切 alt、次序错）。
#[must_use]
fn terminal_modes_vt(term: &Terminal) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    if term.bracketed_paste() {
        out.extend_from_slice(b"\x1b[?2004h");
    }
    match term.mouse_protocol() {
        MouseProtocol::Off => {}
        MouseProtocol::X10 => out.extend_from_slice(b"\x1b[?9h"),
        MouseProtocol::Normal => out.extend_from_slice(b"\x1b[?1000h"),
        MouseProtocol::Button => out.extend_from_slice(b"\x1b[?1002h"),
        MouseProtocol::Any => out.extend_from_slice(b"\x1b[?1003h"),
    }
    if term.mouse_encoding() == MouseEncoding::Sgr {
        out.extend_from_slice(b"\x1b[?1006h");
    }
    if term.focus_event() {
        out.extend_from_slice(b"\x1b[?1004h");
    }
    if term.win32_input() {
        out.extend_from_slice(b"\x1b[?9001h");
    }
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

/// 单个 HistoryRows 帧的**原始字节预算**：`lines` 是 `Vec<Vec<u8>>`、serde 序列化成 JSON 数字
/// 数组约 3.6x 膨胀，故按原始字节夹在 ~800 KiB → JSON 约 2.9 MiB，稳在中继/QUIC 4 MiB 单帧上限内。
/// 防超宽终端满色彩（每 cell 都换 SGR）下单帧击穿 4 MiB（中继会断整条 WS、QUIC 会静默丢帧）。
const HISTORY_VT_BYTES_BUDGET: usize = 800 * 1024;

/// part3d：被控端按**绝对行号**序列化历史行 `[top, top + count)`，回带当前历史边界。
///
/// 返回 `(lines, base, screen_top)`：`lines[i]` 是绝对行 `top + i` 的 VT 字节（空 `Vec`
/// = 该行空白或越界，控制端渲染为空白）；`base` = 最旧保留行绝对行号（`Grid::line_by_abs` 内部据
/// `dropped_lines` 换算），`screen_top` = 可视区首行绝对行号。控制端据此夹紧回看范围。
///
/// **字节预算夹紧**：累计 VT 字节超 [`HISTORY_VT_BYTES_BUDGET`] 即提前停（返回 < `count` 行），
/// 防单帧击穿 4 MiB 上限。未返的尾部行由控制端按缺口（`hist_inflight` 缺口清理）从 `top + 已返行数`
/// 续请求——故返回长度**不再恒等于 `count`**。至少返 1 行保证进度（极端单行超预算亦先返该行）。
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
    let mut total = 0usize;
    for i in 0..count as u64 {
        let bytes = grid
            .line_by_abs(top + i)
            .map_or_else(Vec::new, |r| serialize_row_vt(r, cols));
        // 至少返 1 行保进度；之后累计超预算即停（尾部行控制端续请求）。
        if !lines.is_empty() && total.saturating_add(bytes.len()) > HISTORY_VT_BYTES_BUDGET {
            break;
        }
        total += bytes.len();
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

    /// 备用屏快照往返：被控端在 alt 屏（claude 全屏）时快照须复现主/备两级——
    /// 喂入镜像后镜像应处于 alt 屏、显示 alt 内容；镜像收到 `?1049l`（被控端退出
    /// 全屏）后应恢复主屏内容而非残留 alt。回归 2026-07「进 claude 副屏退出后残留」。
    #[test]
    fn 备用屏快照复现主备两级并可正确退出() {
        let mut src = Terminal::new(4, 20, 100);
        src.advance(b"main-line-1\r\nmain-line-2\r\n"); // 主屏内容
        src.advance(b"\x1b[?1049h"); // 进备用屏
        src.advance(b"\x1b[HALT-FULLSCREEN"); // alt 屏内容（顶行）
        assert!(src.is_alt_screen(), "src 应处于备用屏");
        let snap = screen_snapshot_vt(&src);

        // 镜像重放快照：应落在备用屏、显示 alt 内容（不含主屏行）。
        let mut dst = Terminal::new(4, 20, 100);
        dst.advance(&snap);
        assert!(dst.is_alt_screen(), "镜像重放后应处于备用屏");
        assert_eq!(visible_text(&dst)[0], "ALT-FULLSCREEN", "镜像应显示 alt 内容");
        assert!(
            visible_text(&dst).iter().all(|l| !l.contains("main-line")),
            "备用屏期间镜像不应露出主屏内容"
        );
        let cur = &dst.grid().cursor;
        assert_eq!((cur.row, cur.col), (0, 14), "镜像 alt 光标应与被控端一致");

        // 被控端退出全屏 → 镜像收到 ?1049l：应恢复主屏内容、不残留 alt。
        dst.advance(b"\x1b[?1049l");
        assert!(!dst.is_alt_screen(), "镜像退出备用屏");
        let vt = visible_text(&dst);
        assert_eq!(vt[0], "main-line-1", "退出后恢复主屏首行");
        assert_eq!(vt[1], "main-line-2", "退出后恢复主屏次行");
        assert!(
            vt.iter().all(|l| !l.contains("ALT-FULLSCREEN")),
            "退出备用屏后不应残留 alt 内容"
        );
        let cur = &dst.grid().cursor;
        assert_eq!((cur.row, cur.col), (2, 0), "退出后主屏光标恢复到切 alt 前位置");
    }

    /// 被控端用户上滚回看（display_offset > 0）时接入：快照应序列化**真实屏幕行**
    /// 而非回看视图——否则镜像底屏混入历史行、与屏幕坐标的光标错位（2026-07 修）。
    #[test]
    fn 快照序列化真实屏幕行_不受回看态影响() {
        let mut src = Terminal::new(3, 20, 100);
        src.advance(b"H0\r\nH1\r\nS0\r\nS1\r\nS2"); // H0/H1 入历史，屏幕 S0/S1/S2
        src.grid_mut().scroll_display(2); // 用户上滚回看 2 行
        let snap = screen_snapshot_vt(&src);

        let mut dst = Terminal::new(3, 20, 100);
        dst.advance(&snap);
        assert_eq!(
            visible_text(&dst),
            vec!["S0", "S1", "S2"],
            "镜像应复现真实屏幕行，而非被控端的回看视图"
        );
    }

    /// claude/Ink 全屏运行期隐藏光标（?25l）且只在退出时恢复：中途接入的镜像
    /// 须复现主/备两级可见性，否则在被控端不显示光标的界面上画出幽灵光标。
    /// 两级都取「隐藏」态——镜像默认可见，只有隐藏态能区分「补发生效」与
    /// 「默认值碰巧对」（对抗审查变异实证：可见态断言永真、守不住补发调用）。
    #[test]
    fn 快照复现光标可见性_主备两级() {
        let mut src = Terminal::new(4, 20, 100);
        src.advance(b"shell> \x1b[?25l"); // 主屏即已隐藏（spinner 场景）
        src.advance(b"\x1b[?1049h\x1b[?25l\x1b[HFULL"); // alt 屏同样隐藏
        let snap = screen_snapshot_vt(&src);

        let mut dst = Terminal::new(4, 20, 100);
        dst.advance(&snap);
        assert!(!dst.grid().cursor.visible, "alt 段应复现 ?25l（隐藏）");
        dst.advance(b"\x1b[?1049l"); // 被控端退出全屏
        assert!(
            !dst.grid().cursor.visible,
            "主屏段应复现 ?25l——退出全屏恢复的主屏光标仍隐藏，而非镜像默认可见"
        );
    }

    /// 主屏 spinner 场景（claude 不进 alt 时也会 ?25l 藏光标）：非 alt 分支
    /// 同样须复现可见性（对抗审查变异实证：此前该分支零守护）。
    #[test]
    fn 快照复现主屏光标隐藏() {
        let mut src = Terminal::new(4, 20, 100);
        src.advance(b"working...\x1b[?25l");
        let snap = screen_snapshot_vt(&src);
        let mut dst = Terminal::new(4, 20, 100);
        dst.advance(&snap);
        assert!(!dst.grid().cursor.visible, "非 alt 快照应复现 ?25l（隐藏）");
    }

    /// 主屏程序不进 alt 直接设 DECSTBM（tput csr 固定标题行等）：非 alt 分支
    /// 同样须补发滚动区（对抗审查变异实证：此前该分支零守护）。
    #[test]
    fn 快照复现主屏滚动区() {
        let mut src = Terminal::new(6, 20, 100);
        src.advance(b"\x1b[2;5r");
        let snap = screen_snapshot_vt(&src);
        let mut dst = Terminal::new(6, 20, 100);
        dst.advance(&snap);
        assert_eq!(dst.scroll_region(), (1, 4), "非 alt 快照应复现 DECSTBM 滚动区");
    }

    /// 控制端粘贴按**镜像** term 的 bracketed_paste 决定是否包 200~/201~
    /// （remote_ws `send_paste`）：订阅前已发的 ?2004h 必须重放，否则中途接入
    /// 后的多行粘贴不包裹、被被控端逐行执行。
    #[test]
    fn 快照复现括号粘贴模式() {
        let mut src = Terminal::new(4, 20, 100);
        src.advance(b"\x1b[?2004h"); // claude/Ink 启动即开启
        let snap = screen_snapshot_vt(&src);
        let mut dst = Terminal::new(4, 20, 100);
        dst.advance(&snap);
        assert!(dst.bracketed_paste(), "镜像应复现 ?2004（括号粘贴）");
    }

    /// 画笔（SGR pen）两级复现：行序列化每行末尾 `\x1b[0m` 归零，须显式补——
    /// 否则镜像 ?1049l 恢复的画笔与被控端分叉、快照后 bce 式擦除底色错误。
    #[test]
    fn 快照复现画笔_主备两级() {
        let mut src = Terminal::new(4, 20, 100);
        src.advance(b"sh\x1b[31m"); // 主屏画笔=红字（?1049h 时被另存）
        src.advance(b"\x1b[?1049h\x1b[44mFULL"); // alt 内画笔=蓝底
        let snap = screen_snapshot_vt(&src);
        let mut dst = Terminal::new(4, 20, 100);
        dst.advance(&snap);

        // alt 段：快照后 bce 式擦除（EL 用 pen.bg 填充）两端底色应一致。
        src.advance(b"\x1b[2;1H\x1b[K");
        dst.advance(b"\x1b[2;1H\x1b[K");
        let (s_bg, d_bg) = (src.grid().row(1).cells()[0].bg, dst.grid().row(1).cells()[0].bg);
        assert_eq!(s_bg, Color::Indexed(4), "前提：被控端擦出蓝底");
        assert_eq!(d_bg, s_bg, "alt 段擦除底色应与被控端一致（当前画笔已复现）");

        // 退出全屏：两端恢复的主屏画笔应一致（红字），新写字符前景一致。
        src.advance(b"\x1b[?1049lZ");
        dst.advance(b"\x1b[?1049lZ");
        let sc = src.grid().row(0).cells()[2];
        let dc = dst.grid().row(0).cells()[2];
        assert_eq!(sc.fg, Color::Indexed(1), "前提：被控端恢复红字画笔");
        assert_eq!(dc.ch, 'Z');
        assert_eq!(dc.fg, sc.fg, "退出全屏后画笔应恢复为切屏前的红字，而非默认");
    }

    /// 快照恰截在折行边界（pending_wrap=true）：镜像须复现该态——否则快照后
    /// 第一个字符被控端折行、镜像却覆写行尾，此后整行错位。
    #[test]
    fn 快照复现行尾pending_wrap() {
        let mut src = Terminal::new(4, 10, 100);
        src.advance(b"AAAAAAAAAA"); // 写满 10 列，pending_wrap=true
        let snap = screen_snapshot_vt(&src);
        let mut dst = Terminal::new(4, 10, 100);
        dst.advance(&snap);
        src.advance(b"Z");
        dst.advance(b"Z");
        assert_eq!(visible_text(&src), visible_text(&dst), "后续字符两端应同样折行");
        let (s, d) = (&src.grid().cursor, &dst.grid().cursor);
        assert_eq!((d.row, d.col), (s.row, s.col), "光标终态应一致");
    }

    /// 反向：被控端光标被显式 CUP 停在满宽行行尾（pending_wrap=false），镜像
    /// 重绘满宽行残留的 pending_wrap 须被清掉——否则镜像折行、被控端覆写行尾。
    #[test]
    fn 快照清除重绘残留的pending_wrap() {
        let mut src = Terminal::new(4, 10, 100);
        src.advance(b"AAAAAAAAAA\x1b[3;1H\x1b[1;10H"); // 满行后显式回到行尾
        let snap = screen_snapshot_vt(&src);
        let mut dst = Terminal::new(4, 10, 100);
        dst.advance(&snap);
        src.advance(b"Z");
        dst.advance(b"Z");
        assert_eq!(visible_text(&src), visible_text(&dst), "后续字符两端应同样覆写行尾");
        let (s, d) = (&src.grid().cursor, &dst.grid().cursor);
        assert_eq!((d.row, d.col), (s.row, s.col), "光标终态应一致");
    }

    /// vim 类应用进 alt 后设一次滚动区（CSI r）便不再重发：中途接入的镜像须
    /// 复现，否则按全屏滚动、状态行被一起搬走。
    #[test]
    fn 快照复现备用屏滚动区() {
        let mut src = Terminal::new(6, 20, 100);
        src.advance(b"\x1b[?1049h\x1b[2;5r"); // alt 内设滚动区 [1,4]（0-based）
        src.advance(b"\x1b[6;1Hstatus"); // 区外底行写状态行
        let snap = screen_snapshot_vt(&src);

        let mut dst = Terminal::new(6, 20, 100);
        dst.advance(&snap);
        assert_eq!(dst.scroll_region(), (1, 4), "镜像应复现 DECSTBM 滚动区");
        assert_eq!(
            (dst.grid().cursor.row, dst.grid().cursor.col),
            (src.grid().cursor.row, src.grid().cursor.col),
            "CSI r 归位副作用应被随后的光标 CUP 纠正"
        );
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

    /// 快照应重发鼠标/输入模式，使中途接入的镜像复现 proto（控制端滚轮路由依赖）。
    #[test]
    fn 快照复现鼠标上报模式() {
        let mut src = Terminal::new(6, 20, 100);
        // Claude 全屏典型：Any 鼠标 + SGR 编码 + 焦点 + win32 输入。
        src.advance(b"\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?9001h");
        let snap = screen_snapshot_vt(&src);

        let mut dst = Terminal::new(6, 20, 100);
        dst.advance(&snap);
        assert_eq!(dst.mouse_protocol(), MouseProtocol::Any, "镜像应复现 Any 上报");
        assert_eq!(dst.mouse_encoding(), MouseEncoding::Sgr, "镜像应复现 SGR 编码");
        assert!(dst.focus_event(), "镜像应复现焦点事件");
        assert!(dst.win32_input(), "镜像应复现 win32 输入");
    }

    /// 未开鼠标上报时快照不应启用它（镜像保持 Off → 滚轮走本地回看）。
    #[test]
    fn 快照不误开鼠标上报() {
        let mut src = Terminal::new(4, 10, 50);
        src.advance(b"plain text\r\n");
        let snap = screen_snapshot_vt(&src);
        let mut dst = Terminal::new(4, 10, 50);
        dst.advance(&snap);
        assert_eq!(dst.mouse_protocol(), MouseProtocol::Off, "无上报时镜像应保持 Off");
    }
}
