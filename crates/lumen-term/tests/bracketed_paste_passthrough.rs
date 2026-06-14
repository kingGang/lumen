//! bracketed paste 多行提交实测（M4.1 批D 专项，真机手跑，CI 跳过）。
//!
//! **目的**：验证设计稿 §3.2 第②步的多行提交编码（`ESC[200~ … ESC[201~ \r`）
//! 经 ConPTY 输入向送达交互式 pwsh + PSReadLine 后，多行块被**整体接收并
//! 一次执行**，不触发 `>>` 续行提示符（设计稿风险 2 专项）。
//!
//! **运行方式**（需真实 Windows ConPTY + pwsh）：
//! ```bash
//! cargo test -p lumen-term --test bracketed_paste_passthrough -- --ignored --nocapture
//! ```
//!
//! **验收标准**：
//! - 输出中出现独立的执行结果 `3`（`if ($true) { 1+2 }` 的求值输出——回显
//!   源文本里没有字符 3，只能来自真实执行）→ 多行整体执行 OK。
//! - 同时统计 `>>` 续行提示符出现情况辅助判读（仅 fail 时参考）。
//!
//! **降级预案**（若失败）：提交编码改「反引号续行拼单行」
//! （`if ($true) {` → `if ($true) { `…`` 拼接），见设计稿 §10 M3.1。

use std::time::{Duration, Instant};

use lumen_pty::{PtyEvent, PtySession};

/// 收集指定时长内 PTY 输出字节；遇到 DSR 光标位置查询（CSI 6n）即刻
/// 回写应答（CSI 1;1R）——PSReadLine 初始化会发 DSR 并**阻塞等待应答**，
/// 真实终端（含 lumen）都会应答，测试侧必须模拟，否则交互式 pwsh
/// 卡死在 PSReadLine 启动（首版测试 5 秒只收 4 字节即此坑）。
fn collect_output(
    session: &PtySession,
    rx: &crossbeam_channel::Receiver<PtyEvent>,
    timeout: Duration,
) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(PtyEvent::Data(data)) => {
                buf.extend_from_slice(&data);
                // DSR 应答（粗匹配本批数据即可——查询不会跨包断裂到值得处理的程度）。
                if data.windows(4).any(|w| w == b"\x1b[6n") {
                    let _ = session.write(b"\x1b[1;1R");
                }
            }
            Ok(PtyEvent::Exited) => break,
            Err(_) => break,
        }
    }
    buf
}

#[test]
#[ignore = "需要真实 ConPTY 环境（交互式 pwsh + PSReadLine），CI 跳过，真机手跑"]
fn bracketed_paste_多行整体执行验证() {
    // 交互式 pwsh（-NoProfile 排除用户 profile 干扰；PSReadLine 为内置模块
    // 交互式会话自动加载，bracketed paste 由它声明 DECSET 2004）。
    let (session, rx) = PtySession::spawn(
        Some("pwsh.exe"),
        &["-NoLogo".into(), "-NoProfile".into()],
        30,
        120,
        None,
        &[],
    )
    .expect("启动 pwsh.exe 失败——请确认 pwsh.exe 在 PATH 中");

    // 等 PSReadLine 起来并画出首个提示符。
    let startup = collect_output(&session, &rx, Duration::from_secs(8));
    println!(
        "── 启动期输出 {} 字节（应含提示符与 DECSET 2004 声明）──",
        startup.len()
    );
    let startup_text = String::from_utf8_lossy(&startup);
    let decset_2004 = startup_text.contains("[?2004h");
    println!("  PSReadLine 声明 bracketed paste（CSI ?2004h）: {decset_2004}");

    // 多行块：执行结果「3」在回显源文本中不存在，只能来自真实求值。
    let payload = "if ($true) {\r 1+2\r}";
    let mut input = Vec::new();
    input.extend_from_slice(b"\x1b[200~");
    input.extend_from_slice(payload.as_bytes());
    input.extend_from_slice(b"\x1b[201~");
    input.extend_from_slice(b"\r");
    session
        .write(&input)
        .expect("写入 bracketed paste 序列失败");

    let raw = collect_output(&session, &rx, Duration::from_secs(6));
    let text = String::from_utf8_lossy(&raw);
    println!("── 提交后输出 {} 字节 ──", raw.len());

    // 执行结果判定：剥掉 VT 序列后找独立的「3」行。
    // 粗剥离：把 ESC 序列常见形态删掉再按行扫。
    let mut cleaned = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // 跳过 CSI/OSC 序列至终结符（粗略但对判定足够）。
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for t in chars.by_ref() {
                        if t.is_ascii_alphabetic() || t == '~' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut prev_esc = false;
                    for t in chars.by_ref() {
                        if t == '\u{7}' || (prev_esc && t == '\\') {
                            break;
                        }
                        prev_esc = t == '\u{1b}';
                    }
                }
                _ => {}
            }
        } else {
            cleaned.push(c);
        }
    }
    let result_line_3 = cleaned.lines().any(|l| l.trim() == "3");
    let continuation = cleaned.contains(">>");

    println!(
        "  清理后文本（utf8 lossy，截 600 字符）：{:?}",
        &cleaned.chars().take(600).collect::<String>()
    );
    println!("  独立结果行「3」出现: {result_line_3}");
    println!("  「>>」续行提示符出现: {continuation}");

    if result_line_3 {
        println!("→ 多行 bracketed paste 整体执行 OK ✓");
        println!("  M4.1 提交链路的多行编码方案（ESC[200~…ESC[201~\\r）可行。");
    } else {
        println!("→ 多行 bracketed paste 执行 FAIL ✗");
        println!("  降级预案：提交编码改「反引号续行拼单行」（设计稿 §10 M3.1）。");
        panic!("bracketed paste 多行整体执行失败，参考上方输出与降级预案");
    }
    drop(session);
}
