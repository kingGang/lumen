//! req6 真机诊断：真实 ConPTY（pwsh）字节流下，长行的 wrapped 标记与
//! `Terminal::resize` 的 scrollback reflow 行为（CI 跳过，真机手跑）。
//!
//! **目的**：坐实 req6 修复方案的前提假设——真实 ConPTY 把超过列宽的长行
//! 发给 Lumen 时，是让 Lumen 自己 autowrap（行带 `wrapped=true`，reflow 能
//! 解折合并），还是 ConPTY 自己插了硬 `\r\n`（行 `wrapped=false`，reflow
//! 无从合并）。后者会让「缩小再放大历史复原」彻底失效。
//!
//! 运行：
//! ```bash
//! cargo test -p lumen-term --test req6_reflow_conpty -- --ignored --nocapture
//! ```

use std::time::{Duration, Instant};

use lumen_pty::{PtyEvent, PtySession};
use lumen_term::Terminal;

/// 在指定时长内泵送 PTY：收数据 → advance → 把终端应答（DSR/CPR 等）
/// 写回 PTY。**必须回写应答**，否则 ConPTY 启动发的 `ESC[6n` 收不到答复
/// 会阻塞，命令输出永远发不出来（真机里 main 事件循环负责这个握手）。
/// 返回收到的总字节数。
fn pump(
    term: &mut Terminal,
    session: &PtySession,
    rx: &crossbeam_channel::Receiver<PtyEvent>,
    dur: Duration,
) -> usize {
    let deadline = Instant::now() + dur;
    let mut total = 0usize;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(150))) {
            Ok(PtyEvent::Data(data)) => {
                total += data.len();
                term.advance(&data);
                let resp = term.take_responses();
                if !resp.is_empty() {
                    let _ = session.write(&resp);
                }
            }
            Ok(PtyEvent::Exited) => break,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {} // 继续等到 deadline
            Err(_) => break,
        }
    }
    total
}

/// dump 当前 scrollback 每行：序号 / wrapped / 宽度 / 去尾空白内容前 48 字符。
fn dump_scrollback(term: &Terminal, tag: &str) {
    let g = term.grid();
    let sb = g.scrollback_len();
    println!(
        "── [{tag}] scrollback_len={sb} cols={} rows={} ─────────────",
        g.cols(),
        g.rows()
    );
    let dropped = g.absolute_cursor_line() - sb as u64 - g.cursor.row as u64;
    let mut wrapped_cnt = 0usize;
    for i in 0..sb as u64 {
        let abs = dropped + i;
        let Some(row) = g.line_by_abs(abs) else {
            continue;
        };
        let text: String = row.cells().iter().map(|c| c.ch).collect();
        let trimmed = text.trim_end();
        let w = row.is_wrapped();
        if w {
            wrapped_cnt += 1;
        }
        // 只打印「非纯空白」行，避免刷屏。
        if !trimmed.is_empty() {
            let shown: String = trimmed.chars().take(48).collect();
            println!(
                "  sb[{i:>3}] wrapped={:<5} width={:>3} | {shown}",
                w,
                row.cells().len()
            );
        }
    }
    println!("  （wrapped=true 行数={wrapped_cnt} / 共 {sb} 行）");
}

#[test]
#[ignore = "需要真实 ConPTY 环境（pwsh.exe），CI 跳过，真机手跑：cargo test -p lumen-term --test req6_reflow_conpty -- --ignored --nocapture"]
fn req6_真实conpty长行的wrapped标记与reflow() {
    // 80 列下输出 20 行、每行 100 个字符（超列宽 → 应触发 autowrap）。
    // 用 [Console]::WriteLine 绕过 PSReadLine，让字节尽量原样到 PTY。
    // 末尾 Start-Sleep 让进程在 resize 期间保活，便于观察 ConPTY 重发。
    let cmd = r#"for($i=0;$i -lt 20;$i++){[Console]::Out.WriteLine([string]([char](65+($i%26)))*100)}; Start-Sleep -Seconds 4"#;

    let rows = 10u16;
    let cols0 = 80u16;
    let (session, rx) = PtySession::spawn(
        Some("pwsh.exe"),
        &[
            "-NoProfile".into(),
            "-NoLogo".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            cmd.into(),
        ],
        rows,
        cols0,
        None,
        &[],
    )
    .expect("启动 pwsh.exe 失败——请确认 pwsh.exe 在 PATH 中");

    let mut term = Terminal::new(rows as usize, cols0 as usize, 1000);

    // 收集首批输出（命令产出的 20 行宽行）——给足 pwsh 冷启动时间，
    // 且回写 DSR 应答让 ConPTY 不阻塞。
    let n = pump(&mut term, &session, &rx, Duration::from_secs(6));
    println!("首批字节 {n} 字节");

    // ① 真机字节流下，长行进 scrollback 时是否带 wrapped 标记？
    dump_scrollback(&term, "初始 80 列");

    // ② 缩小到 40 列：term 先 reflow scrollback（同步），再通知 ConPTY。
    term.resize(rows as usize, 40);
    let _ = session.resize(rows, 40);
    pump(&mut term, &session, &rx, Duration::from_millis(1000));
    dump_scrollback(&term, "缩到 40 列后");

    // ③ 放大回 80 列：term reflow 应把折行解折合并回 80 宽。
    term.resize(rows as usize, 80);
    let _ = session.resize(rows, 80);
    pump(&mut term, &session, &rx, Duration::from_millis(1000));
    dump_scrollback(&term, "放大回 80 列后");

    let _ = session.is_alive();

    // 判据：放大回 80 后，scrollback 里应出现「写满到接近 80 宽」的长行
    // （reflow 解折合并成功）。若全是 ≤40 宽的窄行，说明 reflow 没生效
    // （多半因真机长行 wrapped=false，解折前提不成立）。
    let g = term.grid();
    let sb = g.scrollback_len();
    let dropped = g.absolute_cursor_line() - sb as u64 - g.cursor.row as u64;
    let mut max_content_width = 0usize;
    for i in 0..sb as u64 {
        if let Some(row) = g.line_by_abs(dropped + i) {
            let w = row.cells().iter().map(|c| c.ch).collect::<String>().trim_end().chars().count();
            max_content_width = max_content_width.max(w);
        }
    }
    println!("放大回 80 后，scrollback 最宽内容行 = {max_content_width} 列");
    assert!(
        max_content_width > 50,
        "放大回 80 后历史仍是窄行（最宽 {max_content_width} 列）——reflow 未把折行解折合并，req6 未修复"
    );
}

/// 复刻海风哥真机场景：**交互式 pwsh**，在「宽列下产出 → 缩到窄列（reflow
/// 与 ConPTY 重发）→ 窄列下再产出 → 放大回宽列」全过程后，检查窄列期间
/// ConPTY 输出/重发进 scrollback 的行能否在放大时解折合并回宽行。
///
/// 关键怀疑：ConPTY 在窄列下输出（或 resize 重发）若用 CUP 逐行定位写入
/// 而非 autowrap，行会是 `wrapped=false`，放大时不解折 → 历史仍窄。
/// 真机 CJK e2e：真实 pwsh 输出中文长行（autowrap），缩到奇数窄列（产生
/// 宽字符末列 pad）再放大，验证 CJK 历史**解折合并回宽行、不碎裂、无多余
/// 空格**（海风哥中文 engram dump 缩放后碎裂的真机根因回归）。
#[test]
#[ignore = "需要真实 ConPTY 环境（pwsh.exe），CI 跳过：cargo test -p lumen-term --test req6_reflow_conpty req6_真机_cjk -- --ignored --nocapture"]
fn req6_真机_cjk长行缩放往返不碎裂() {
    let rows = 10u16;
    let (session, rx) = PtySession::spawn(
        Some("pwsh.exe"),
        &["-NoProfile".into(), "-NoLogo".into()],
        rows,
        80,
        None,
        &[],
    )
    .expect("启动 pwsh.exe 失败");
    let mut term = Terminal::new(rows as usize, 80, 2000);
    pump(&mut term, &session, &rx, Duration::from_secs(3));

    // UTF-8 输出 + 6 行很长的中文（每行 ~96 CJK 字 = 192 列，80 列下 autowrap）。
    let _ = session.write(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; 1..6 | ForEach-Object { '中文测试内容数据样例' * 12 }\r"
            .as_bytes(),
    );
    pump(&mut term, &session, &rx, Duration::from_secs(2));
    dump_scrollback(&term, "80 列 CJK 产出后");

    // 缩到 41 列（奇数 → 宽字符折行末列必留 1 格 pad），再放大回 80。
    term.resize(rows as usize, 41);
    let _ = session.resize(rows, 41);
    pump(&mut term, &session, &rx, Duration::from_millis(1200));
    term.resize(rows as usize, 80);
    let _ = session.resize(rows, 80);
    pump(&mut term, &session, &rx, Duration::from_millis(1200));
    dump_scrollback(&term, "放大回 80 后（CJK 应合并回宽行、无碎裂/无多余空格）");

    let _ = session.write(b"exit\r");
    // 检查：含「中文」的历史行最宽应接近 80（合并成功），而非碎成 ~41 短行。
    let g = term.grid();
    let sb = g.scrollback_len();
    let dropped = g.absolute_cursor_line() - sb as u64 - g.cursor.row as u64;
    let mut max_cjk = 0usize;
    for i in 0..sb as u64 {
        if let Some(row) = g.line_by_abs(dropped + i) {
            let t: String = row.cells().iter().map(|c| c.ch).collect();
            let t = t.trim_end();
            if t.contains('中') {
                max_cjk = max_cjk.max(t.chars().count());
            }
        }
    }
    println!("放大回 80 后，含中文的历史行最宽 = {max_cjk} 字符（期望接近 ~40 字=80 列；若 ~20 则碎裂）");
    assert!(
        max_cjk > 30,
        "CJK 历史行放大后仍碎裂（最宽 {max_cjk} 字符）——宽字符 pad 未被正确处理"
    );
}

/// dump 可视区（screen）每行：序号 / wrapped / 宽度 / 内容前 40 字符。
fn dump_screen(term: &Terminal, tag: &str) {
    let g = term.grid();
    println!("── [{tag}] SCREEN rows={} cols={} ──", g.rows(), g.cols());
    for r in 0..g.rows() {
        let row = g.row(r);
        let text: String = row.cells().iter().map(|c| c.ch).collect();
        let trimmed = text.trim_end();
        if !trimmed.is_empty() {
            let shown: String = trimmed.chars().take(40).collect();
            println!(
                "  scr[{r:>2}] wrapped={:<5} w={:>3} | {shown}",
                row.is_wrapped(),
                trimmed.chars().count()
            );
        }
    }
}

/// 复刻图3/图4：普通 `cat`-style 输出在屏幕区，缩小再放大后**可见区**是否
/// 变窄（ConPTY 重发是否把可见内容解折回宽行）。
#[test]
#[ignore = "需要真实 ConPTY 环境（pwsh.exe），CI 跳过：cargo test -p lumen-term --test req6_reflow_conpty req6_屏幕区 -- --ignored --nocapture"]
fn req6_屏幕区_缩放后可见区是否变窄() {
    let rows = 10u16;
    let (session, rx) = PtySession::spawn(
        Some("pwsh.exe"),
        &["-NoProfile".into(), "-NoLogo".into()],
        rows,
        120,
        None,
        &[],
    )
    .expect("启动 pwsh.exe 失败");
    let mut term = Terminal::new(rows as usize, 120, 2000);
    pump(&mut term, &session, &rx, Duration::from_secs(3));

    // 4 行 90 字符（< 120 不折、> 60 在 60 列下折）。
    let _ = session.write(b"1..4 | ForEach-Object { '=' * 90 }\r");
    pump(&mut term, &session, &rx, Duration::from_secs(2));
    dump_screen(&term, "初始 120");

    term.resize(rows as usize, 60);
    let _ = session.resize(rows, 60);
    pump(&mut term, &session, &rx, Duration::from_millis(1500));
    dump_screen(&term, "缩到 60");

    term.resize(rows as usize, 120);
    let _ = session.resize(rows, 120);
    pump(&mut term, &session, &rx, Duration::from_millis(1500));
    dump_screen(&term, "放大回 120（关键：'=' 行是否回到 90 宽）");

    let _ = session.write(b"exit\r");
    let g = term.grid();
    let mut maxw = 0usize;
    for r in 0..g.rows() {
        let t: String = g.row(r).cells().iter().map(|c| c.ch).collect();
        let tt = t.trim_end();
        if tt.starts_with('=') {
            maxw = maxw.max(tt.chars().count());
        }
    }
    println!("放大回 120 后，屏幕 '=' 行最宽 = {maxw} 列（期望 ~90；若 ~60 则可见区未解折=bug）");
}

#[test]
#[ignore = "需要真实 ConPTY 环境（pwsh.exe），CI 跳过：cargo test -p lumen-term --test req6_reflow_conpty req6_真机场景 -- --ignored --nocapture"]
fn req6_真机场景_窄列输出再放大是否合并() {
    let rows = 12u16;
    let (session, rx) = PtySession::spawn(
        Some("pwsh.exe"),
        &["-NoProfile".into(), "-NoLogo".into()],
        rows,
        134,
        None,
        &[],
    )
    .expect("启动 pwsh.exe 失败");
    let mut term = Terminal::new(rows as usize, 134, 2000);

    // 等提示符、回写 DSR。
    pump(&mut term, &session, &rx, Duration::from_secs(3));

    // 宽列(134)下产出 8 行 200 字符（'=' 行）→ autowrap 成 2 行/条。
    let _ = session.write(b"1..8 | ForEach-Object { '=' * 200 }\r");
    pump(&mut term, &session, &rx, Duration::from_secs(2));
    dump_scrollback(&term, "134 列产出 '=' 后");

    // 缩到 65 列：reflow + ConPTY 重发。
    term.resize(rows as usize, 65);
    let _ = session.resize(rows, 65);
    pump(&mut term, &session, &rx, Duration::from_millis(1500));

    // 窄列(65)下再产出 8 行 200 字符（'#' 行）→ ConPTY 在 65 列下输出。
    let _ = session.write(b"1..8 | ForEach-Object { '#' * 200 }\r");
    pump(&mut term, &session, &rx, Duration::from_secs(2));
    dump_scrollback(&term, "65 列产出 '#' 后（关键：'#' 行是否 wrapped=true）");

    // 放大回 134：两批内容都应解折合并回 ~134 宽。
    term.resize(rows as usize, 134);
    let _ = session.resize(rows, 134);
    pump(&mut term, &session, &rx, Duration::from_millis(1500));
    dump_scrollback(&term, "放大回 134 后（'=' 与 '#' 行是否都合并回宽行）");

    let _ = session.write(b"exit\r");
    let _ = session.is_alive();

    // 分别统计 '=' 行与 '#' 行放大后的最大内容宽度。
    let g = term.grid();
    let sb = g.scrollback_len();
    let dropped = g.absolute_cursor_line() - sb as u64 - g.cursor.row as u64;
    let (mut max_eq, mut max_hash) = (0usize, 0usize);
    for i in 0..sb as u64 {
        if let Some(row) = g.line_by_abs(dropped + i) {
            let t: String = row.cells().iter().map(|c| c.ch).collect();
            let t = t.trim_end();
            let w = t.chars().count();
            if t.starts_with('=') {
                max_eq = max_eq.max(w);
            }
            if t.starts_with('#') {
                max_hash = max_hash.max(w);
            }
        }
    }
    println!("放大回 134 后：'=' 行最宽={max_eq} 列，'#' 行最宽={max_hash} 列");
    assert!(
        max_eq > 100,
        "宽列产出的 '=' 历史行放大后仍窄（{max_eq}）——这批本应由我方 reflow 合并"
    );
    assert!(
        max_hash > 100,
        "窄列(ConPTY)产出的 '#' 历史行放大后仍窄（{max_hash}）——ConPTY 窄列输出行未带 wrapped，reflow 不解折（疑似 req6 真机根因）"
    );
}
