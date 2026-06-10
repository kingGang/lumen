//! VT 流回放诊断工具：把 PTY 原始字节文件喂给 Terminal，
//! 打印最终光标位置与光标附近的屏幕内容。
//!
//! 用法：cargo run -p lumen-term --example replay -- <vt.log> [rows] [cols]

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("用法: replay <vt.log> [rows] [cols]");
    let rows: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(31);
    let cols: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(109);

    let bytes = std::fs::read(&path).expect("读取日志失败");
    let mut term = lumen_term::Terminal::new(rows, cols, 10_000);
    term.advance(&bytes);

    let grid = term.grid();
    let cur = grid.cursor;
    println!("光标: row={} col={} (0 基)", cur.row, cur.col);

    let row = grid.row(cur.row);
    let text: String = row.cells().iter().map(|c| c.ch).collect();
    println!("光标行内容: {:?}", text.trim_end());
    println!(
        "光标处字符: {:?}，前一格: {:?}",
        row[cur.col].ch,
        if cur.col > 0 {
            row[cur.col - 1].ch
        } else {
            ' '
        }
    );
}
