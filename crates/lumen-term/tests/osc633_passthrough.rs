//! OSC 633 透传实测（M4 P0 专项，真机手跑，CI 跳过）。
//!
//! **目的**：验证 ConPTY 不会吞掉 OSC 633 序列，确认 M4.2 的「633;E
//! 权威命令文本」方案技术上可行。若本测试无法通过，需降级为
//! 「OSC 133 私有参数位携带 base64」（已验证的备用通道）。
//!
//! **运行方式**（需要真实 ConPTY 环境，即 Windows 主机，非沙箱 CI）：
//! ```bash
//! cargo test -p lumen-term --test osc633_passthrough -- --ignored --nocapture
//! ```
//!
//! **验收标准**：
//! - `透传_OK`：PTY 原始字节流中出现 `\x1b]633;E;` 前缀 → ConPTY 不吞 → M4.2 方案可行。
//! - 若字节未出现，测试会打印详细的原始字节 hex 供降级方案参考。
//!
//! **入库理由**：仿 b3 取证设施先例——#[ignore] 标注使 CI 不跑，
//! 真机随时可重跑，结论永久留存代码库中便于日后查阅。

// IGNORE: 需要真实 Windows ConPTY 环境，沙箱 CI 中无 pwsh/ConPTY，手动运行。

use std::time::{Duration, Instant};

use lumen_pty::{PtyEvent, PtySession};

/// 从 PTY 事件接收端收集指定时长内的全部输出字节。
fn collect_output(rx: &crossbeam_channel::Receiver<PtyEvent>, timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(PtyEvent::Data(data)) => buf.extend_from_slice(&data),
            Ok(PtyEvent::Exited) => break,
            Err(_) => break,
        }
    }
    buf
}

/// 在字节切片中搜索子序列，返回首次出现的位置。
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// OSC 633;E 透传验证。
///
/// 让 pwsh 向自己的 stdout 写出 OSC 633;E;dGVzdA== 序列（「test」的 base64），
/// 从 PTY 读端捕获全部字节，在原始字节流中查找 `\x1b]633;E;` 前缀。
///
/// - 找到 → 透传 OK，M4.2「633;E 权威命令文本」方案可行。
/// - 未找到 → 打印原始字节 hex，记录降级备注。
///
/// # 降级备注
/// 若 ConPTY 吞掉 633;E，备用方案为：在 integration.ps1 的
/// `PSConsoleHostReadLine` 包装中改用 `OSC 133;B;<base64>` 私有参数位
/// 携带命令文本——133 通道 M2 已证透传，零风险。
#[test]
#[ignore = "需要真实 ConPTY 环境（pwsh.exe），CI 跳过，真机手跑：cargo test -p lumen-term --test osc633_passthrough -- --ignored --nocapture"]
fn osc633_e_透传验证() {
    // 「test」的 base64 = dGVzdA==，拼成 OSC 633;E;dGVzdA==（BEL 终止）。
    // 用 [Console]::Write 直接写 stdout，绕过 PSReadLine 的回显包装，
    // 确保字节一字不差地发往 PTY master。
    let osc633_cmd = r#"[Console]::Write([char]27 + "]633;E;dGVzdA==" + [char]7); exit 0"#;

    // 用较大终端尺寸避免 ConPTY 自动折行裁切字节。
    let (mut session, rx) = PtySession::spawn(
        Some("pwsh.exe"),
        &[
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            osc633_cmd.into(),
        ],
        24,
        220,
        None,
    )
    .expect("启动 pwsh.exe 失败——请确认 pwsh.exe 在 PATH 中");

    // 等待 shell 退出或超时（最多 10 秒）。
    let raw = collect_output(&rx, Duration::from_secs(10));

    // 等进程真正退出（兜底）。
    let _ = session.is_alive();

    // 搜索透传标志字节。
    // OSC = ESC + ]，即 0x1B 0x5D；后跟 633;E;（ASCII）。
    let marker = b"\x1b]633;E;";

    println!("── OSC 633;E 透传实测（M4 P0）──────────────────────");
    println!("原始字节长度: {} 字节", raw.len());

    if let Some(pos) = find_subsequence(&raw, marker) {
        // 截取 marker 起始后 32 字节展示内容（含 base64 payload）。
        let snippet_end = (pos + 32).min(raw.len());
        let snippet = &raw[pos..snippet_end];
        println!("透传 OK ✓");
        println!(
            "  位置: 字节偏移 {pos}，内容（hex）: {}",
            snippet
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!(
            "  位置: 字节偏移 {pos}，内容（utf8 lossy）: {:?}",
            String::from_utf8_lossy(snippet)
        );
        println!("→ M4.2「633;E 权威命令文本」方案技术可行，ConPTY 不吞 OSC 633。");
        println!("────────────────────────────────────────────────");
        // 透传 OK：测试正常返回（结论由上方 println 承载）。
    } else {
        // 透传失败：打印原始字节供分析后 panic。
        let hex: Vec<String> = raw.iter().map(|b| format!("{b:02X}")).collect();
        println!("透传 FAIL ✗ — ConPTY 吞掉了 OSC 633;E 序列");
        println!("  原始字节（hex）：{}", hex.join(" "));
        println!(
            "  原始字节（utf8 lossy）：{:?}",
            String::from_utf8_lossy(&raw)
        );
        println!("→ 降级方案：integration.ps1 PSConsoleHostReadLine 包装改用");
        println!("  OSC 133;B;<base64(cmdline)> 私有参数位携带命令文本。");
        println!("  （133 通道 M2 已证透传，零风险。）");
        println!("────────────────────────────────────────────────");
        // panic 记录结论，使测试结果可读；降级方案已在上方打印。
        panic!("OSC 633;E 透传失败：ConPTY 吞掉了序列，请参考降级方案（见上方打印）");
    }
}
