//! 四态输入模式机（M4.1 批B）——设计稿 §2。
//!
//! # 核心纪律（铁律）
//! **禁止任何地方缓存模式副本。** 模式是推导值，由 [`input_mode`] 从终端
//! 状态单点求值；每次按键处理与每批 PTY 数据消化后实时调用，不在
//! `AppState` 或任何结构体字段里保存 `InputMode` 的副本——防止状态漂移
//! 与「模式锁死」类 bug（设计稿 §2 铁律，M4 远程控制同一路径）。
//!
//! # 使用方式
//! ```no_run
//! # use lumen_term::Terminal;
//! # use crate::mode::{input_mode, effective_mode};
//! // 直接按需求值，禁止保存结果到字段
//! let mode = effective_mode(&term, force_fallback);
//! ```
//!
//! # 设计稿对应章节
//! 设计稿 §2「输入模式机（四态，纯推导）」。

use lumen_term::Terminal;

/// 四态输入模式。
///
/// 模式是**推导值**，由 [`input_mode`] 从终端状态单点求值。
/// **禁止在任何结构体字段里缓存此枚举副本**（设计稿 §2 铁律）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// 等待用户输入命令（OSC 133;B 到、133;C 未到）。
    ///
    /// 按键路由：M4.1 批B 暂与 Running 相同（直通 PTY），批D 开闸时
    /// 切换为本地编辑（keymap 表内 Compose 态条目到时更新）。
    Compose,
    /// 命令运行中（133;C 到、133;D 未到；或 y/n 确认 / REPL / 密码）。
    ///
    /// 按键路由：逐键直通 PTY，不做本地缓冲。
    Running,
    /// 备用屏幕（vim / htop / codex TUI；`is_alt_screen()` == true）。
    ///
    /// 按键路由：完全直通（含 IME、Ctrl+C/V）。
    AltScreen,
    /// 降级直通（shell integration 未生效 / 注入失败，blocks 为空）。
    ///
    /// 按键路由：永久直通 = M2 现状。
    Fallback,
}

/// 从终端状态纯函数推导输入模式（设计稿 §2 原文实现）。
///
/// **禁止在任何地方缓存此函数的返回值到字段**。
///
/// # 推导规则
/// 1. `blocks` 为空 → [`InputMode::Fallback`]（从未见 OSC 133 标记，降级直通）
/// 2. `is_alt_screen()` → [`InputMode::AltScreen`]（全屏 TUI 让位）
/// 3. 最后一块 `cmd_line.is_some()` && `output_line.is_none()` → [`InputMode::Compose`]
///    （133;B 到、133;C 未到 = PSReadLine 正在等输入）
/// 4. 其余 → [`InputMode::Running`]（命令执行中 / REPL / 密码输入等）
pub fn input_mode(term: &Terminal) -> InputMode {
    // 规则 1：从未见 133 标记 → 降级直通
    if term.blocks().is_empty() {
        return InputMode::Fallback;
    }
    // 规则 2：备用屏幕（vim / htop / codex）
    if term.is_alt_screen() {
        return InputMode::AltScreen;
    }
    // 规则 3：133;B 到、133;C 未到 = 等待输入
    match term.blocks().last() {
        Some(b) if b.cmd_line.is_some() && b.output_line.is_none() => InputMode::Compose,
        // 规则 4：其余均为运行中（含命令已发、退出码未到、REPL 等）
        _ => InputMode::Running,
    }
}

/// 有效输入模式（含 `force_fallback` 手动逃生舱覆盖层）。
///
/// `Ctrl+Shift+E` 置位 `force_fallback` 后，无论底层 [`input_mode`] 推导
/// 结果如何，均强制返回 [`InputMode::Fallback`]。
///
/// **模式机纯函数 [`input_mode`] 本身不变；此函数是唯一的「逃生舱包装层」。**
///
/// # Arguments
/// * `term` - 终端状态机引用（按需求值，不缓存）。
/// * `force_fallback` - `AppState::force_fallback` 字段（Ctrl+Shift+E 开关）。
pub fn effective_mode(term: &Terminal, force_fallback: bool) -> InputMode {
    if force_fallback {
        return InputMode::Fallback;
    }
    input_mode(term)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_term::Terminal;

    /// 构造一个指定行列的终端，并用 VT 序列注入 OSC 133 标记。
    fn make_term(rows: usize, cols: usize) -> Terminal {
        Terminal::new(rows, cols, 100)
    }

    /// 向终端注入原始字节序列。
    fn feed(term: &mut Terminal, data: &[u8]) {
        term.advance(data);
    }

    // ── 规则 1：blocks 为空 → Fallback ──────────────────────────────────

    #[test]
    fn fallback_when_no_blocks() {
        let term = make_term(24, 80);
        assert_eq!(
            input_mode(&term),
            InputMode::Fallback,
            "无 block 应为 Fallback"
        );
    }

    // ── 规则 2：is_alt_screen → AltScreen ───────────────────────────────

    #[test]
    fn alt_screen_when_smcup() {
        let mut term = make_term(24, 80);
        // 先注入 133;A（Prompt Start）+ 133;B（Command Start）让 blocks 非空
        feed(&mut term, b"\x1b]133;A\x07\x1b]133;B\x07");
        // 进入备用屏幕
        feed(&mut term, b"\x1b[?1049h");
        assert_eq!(
            input_mode(&term),
            InputMode::AltScreen,
            "进入备用屏后应为 AltScreen"
        );
    }

    // ── 规则 3：133;B 到、133;C 未到 → Compose ──────────────────────────

    #[test]
    fn compose_when_b_without_c() {
        let mut term = make_term(24, 80);
        // A 标记（Prompt Start）
        feed(&mut term, b"\x1b]133;A\x07");
        // B 标记（Command Start = 提示符渲染完毕，等待输入）
        feed(&mut term, b"\x1b]133;B\x07");
        assert_eq!(
            input_mode(&term),
            InputMode::Compose,
            "133;B 后未 133;C 应为 Compose"
        );
    }

    // ── 规则 4：133;C 到 → Running ──────────────────────────────────────

    #[test]
    fn running_when_c_received() {
        let mut term = make_term(24, 80);
        feed(&mut term, b"\x1b]133;A\x07");
        feed(&mut term, b"\x1b]133;B\x07");
        // C 标记（Output Start = 用户回车执行命令）
        feed(&mut term, b"\x1b]133;C\x07");
        assert_eq!(
            input_mode(&term),
            InputMode::Running,
            "133;C 后应为 Running"
        );
    }

    // ── clear 后回到 Fallback 检验 ───────────────────────────────────────

    #[test]
    fn fallback_after_ris_reset() {
        let mut term = make_term(24, 80);
        // 先建立一个 Compose 态
        feed(&mut term, b"\x1b]133;A\x07\x1b]133;B\x07");
        assert_eq!(input_mode(&term), InputMode::Compose);
        // RIS 全重置（clear 命令走 Shell 侧发 \x1bc 或 ESC c）
        feed(&mut term, b"\x1bc");
        // 重置后 blocks 清空，回到 Fallback
        assert_eq!(
            input_mode(&term),
            InputMode::Fallback,
            "RIS 重置后应回到 Fallback"
        );
    }

    // ── 嵌套 shell（外层块停 Running）────────────────────────────────────

    #[test]
    fn running_when_nested_shell_no_integration() {
        // 嵌套裸 shell：外层已有 133;C（运行中），内层无 integration
        // 不发新 A/B，外层块持续 Running。
        let mut term = make_term(24, 80);
        feed(&mut term, b"\x1b]133;A\x07");
        feed(&mut term, b"\x1b]133;B\x07");
        feed(&mut term, b"\x1b]133;C\x07");
        // 模拟嵌套 shell 的一些输出（无 OSC 133 标记）
        feed(&mut term, b"$ echo hi\r\nhi\r\n");
        assert_eq!(
            input_mode(&term),
            InputMode::Running,
            "嵌套 shell 无 integration 时外层块应维持 Running"
        );
    }

    // ── force_fallback 覆盖层 ────────────────────────────────────────────

    #[test]
    fn effective_mode_force_fallback_overrides_compose() {
        let mut term = make_term(24, 80);
        feed(&mut term, b"\x1b]133;A\x07\x1b]133;B\x07");
        // 底层是 Compose，但 force_fallback=true 强制 Fallback
        assert_eq!(
            effective_mode(&term, true),
            InputMode::Fallback,
            "force_fallback=true 应覆盖 Compose 为 Fallback"
        );
    }

    #[test]
    fn effective_mode_no_force_returns_compose() {
        let mut term = make_term(24, 80);
        feed(&mut term, b"\x1b]133;A\x07\x1b]133;B\x07");
        assert_eq!(
            effective_mode(&term, false),
            InputMode::Compose,
            "force_fallback=false 应正常返回 Compose"
        );
    }

    #[test]
    fn effective_mode_force_fallback_overrides_running() {
        let mut term = make_term(24, 80);
        feed(&mut term, b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        assert_eq!(
            effective_mode(&term, true),
            InputMode::Fallback,
            "force_fallback=true 应覆盖 Running 为 Fallback"
        );
    }

    #[test]
    fn effective_mode_force_fallback_overrides_alt_screen() {
        let mut term = make_term(24, 80);
        feed(&mut term, b"\x1b]133;A\x07\x1b]133;B\x07\x1b[?1049h");
        assert_eq!(
            effective_mode(&term, true),
            InputMode::Fallback,
            "force_fallback=true 应覆盖 AltScreen 为 Fallback"
        );
    }
}
