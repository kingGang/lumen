//! keymap 静态表（M4.1 批B）——设计稿 §4。
//!
//! # 设计原则
//! - **静态表**：表项 = 键位 + 修饰 + 模式列 + 守卫条件 + Action，先命中先赢。
//! - **安全规则置表头**：Ctrl+C Running/AltScreen 无条件 Interrupt 最高优先级
//!   （设计稿风险 3 硬规则：任何守卫组合下均不得拦截）。
//! - **行为 1:1 平移**：把 main.rs 现有八层 if-else 的全部终端快捷键语义
//!   完整平移进表，零行为变化（批B 是纯重构）。
//! - **Compose 态暂与 Running 相同**：本批两态均直通 PTY，批D 开闸时仅改
//!   Compose 态的 Action；Running / AltScreen / Fallback 列不动。
//!
//! # 使用方式
//! ```no_run
//! # use winit::keyboard::ModifiersState;
//! # use winit::event::KeyEvent;
//! # use crate::mode::InputMode;
//! # use crate::keymap::{lookup, GuardState};
//! let guard = GuardState { /* ... */ };
//! if let Some(action) = lookup(&event, mods, mode, &guard) {
//!     // 经由 dispatch 执行
//! }
//! ```
//!
//! # 设计稿对应章节
//! 设计稿 §4「控制键语义表（冻结评审，逐条单测）」。

use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey as WinitNamedKey};

use crate::action::{Action, ComposerAction, EditAction, Motion, ScrollDir, TermAction};
use crate::mode::InputMode;

// ─────────────────────────────────────────────────────────────────
// 内部键匹配辅助类型（测试友好，不依赖 KeyEvent 结构体字段）
// ─────────────────────────────────────────────────────────────────

/// 测试与生产代码共用的「逻辑键」提取结构（脱离 winit KeyEvent 类型）。
///
/// `lookup` 内部通过 `KeyInput::from_event` 提取，测试直接构造此类型——
/// 避免了 `KeyEvent::platform_specific` 是 `pub(crate)` 导致外部无法构造的问题。
#[derive(Debug, Clone)]
pub struct KeyInput {
    /// winit 逻辑键（Key::Character / Key::Named 等）。
    pub logical_key: Key,
}

impl KeyInput {
    /// 从 winit KeyEvent 提取（生产路径）。
    pub fn from_event(event: &KeyEvent) -> Self {
        Self {
            logical_key: event.logical_key.clone(),
        }
    }

    /// 字符键（测试构造用）。
    #[cfg(test)]
    pub fn char(ch: &str) -> Self {
        Self {
            logical_key: Key::Character(ch.into()),
        }
    }

    /// 具名键（测试构造用）。
    #[cfg(test)]
    pub fn named(named: WinitNamedKey) -> Self {
        Self {
            logical_key: Key::Named(named),
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// 守卫条件
// ─────────────────────────────────────────────────────────────────

/// lookup 时的守卫状态（由 main.rs 从 AppState 组装后传入）。
///
/// 守卫字段均为不可变引用或简单 bool，不持有 AppState 所有权。
#[derive(Debug, Clone, Copy, Default)]
pub struct GuardState {
    /// 终端当前有非空文本选区（Ctrl+C 第一级：复制选区）。
    pub has_selection: bool,
    /// 终端有选中的命令块（Ctrl+C 第二级：复制块输出）。
    pub has_selected_block: bool,
    /// 是否处于 alt screen（keymap 内部由 mode 推导，此字段供特殊守卫用）。
    pub is_alt_screen: bool,
    /// 覆盖层（设置 / 登录）打开中（非聚焦状态，键盘归 egui）。
    pub overlay_open: bool,
    /// 重命名编辑中（键盘归 egui 输入框；对应 main.rs `shell_state.renaming.is_some()`）。
    pub renaming: bool,
    /// 文件树对话框打开（键盘归 egui）。
    pub filetree_dialog_open: bool,
    /// 终端是否持有键盘焦点（非聚焦时按键不写 PTY）。
    pub terminal_focused: bool,
    /// win32-input-mode 已开启（LUMEN_WIN32_INPUT=1）。
    pub win32_input: bool,
    /// Compose 态编辑器缓冲是否为空（影响 Ctrl+C / Ctrl+D 的行为）。
    /// M4.1 批D1：仅在 input-editor feature 开启时有意义。
    pub compose_buf_empty: bool,
    /// Compose 态光标是否在首行（↑ 触发历史导航，而非行间移动）。
    /// M4.1 批D2：仅在 input-editor feature 开启时有意义。
    pub compose_cursor_at_first_line: bool,
    /// Compose 态光标是否在末行（↓ 触发历史导航，而非行间移动）。
    /// M4.1 批D2：仅在 input-editor feature 开启时有意义。
    pub compose_cursor_at_last_line: bool,
}

// ─────────────────────────────────────────────────────────────────
// 内部匹配辅助
// ─────────────────────────────────────────────────────────────────

/// 检查字符键（不区分大小写）。
fn is_char(input: &KeyInput, ch: char) -> bool {
    match &input.logical_key {
        Key::Character(s) => s
            .chars()
            .next()
            .map(|c| c.eq_ignore_ascii_case(&ch))
            .unwrap_or(false),
        _ => false,
    }
}

/// 检查具名键。
fn is_named(input: &KeyInput, named: WinitNamedKey) -> bool {
    matches!(&input.logical_key, Key::Named(n) if *n == named)
}

// ─────────────────────────────────────────────────────────────────
// 主查表入口
// ─────────────────────────────────────────────────────────────────

/// 查键表（便利包装，接受 winit `KeyEvent`）。
///
/// 内部将 `event` 转换为 [`KeyInput`] 后调用 [`lookup_input`]。
pub fn lookup(
    event: &KeyEvent,
    mods: ModifiersState,
    mode: InputMode,
    pressed: bool,
    guard: &GuardState,
) -> Option<LookupResult> {
    lookup_input(&KeyInput::from_event(event), mods, mode, pressed, guard)
}

/// 查键表核心逻辑（测试友好版，接受 [`KeyInput`] 而非 winit `KeyEvent`）。
///
/// 按「先命中先赢」顺序检查：
/// 1. 抬起事件路径（win32-input-mode）
/// 2. 外壳级快捷键（Ctrl+Shift+D/W/Enter，不受 alt screen 影响）
/// 3. 外壳级快捷键（Ctrl+T/W/B/,/Tab，alt screen 时让位终端）
/// 4. 终端聚焦闸（非聚焦直接返回 None）
/// 5. Ctrl+Shift+E（经典直通切换，全模式可用）
/// 6. 安全规则：Ctrl+C Running/AltScreen/Fallback 无条件 Interrupt（最高优先级）
/// 7. Shift+PgUp/PgDn 翻屏，Shift+Insert 粘贴（Compose 态 Shift+Insert 进编辑器）
/// 8. Ctrl+↑/↓ 块跳转（非 alt screen）
/// 9. **Compose 态开闸**（M4.1 批D1）：设计稿 §4 完整键位语义表
/// 10. Ctrl+C 三级逻辑（非 Compose 态兜底 / Compose 态已在层 9 处理）
/// 11. Ctrl+V 粘贴（非 Compose 态）
/// 12. 兜底直通编码
///
/// 返回 None 表示此键位无 keymap 命中（终端非聚焦等情况），调用方不写 PTY。
pub fn lookup_input(
    input: &KeyInput,
    mods: ModifiersState,
    mode: InputMode,
    pressed: bool,
    guard: &GuardState,
) -> Option<LookupResult> {
    let ctrl = mods.control_key();
    let shift = mods.shift_key();
    let alt = mods.alt_key();

    // ── 层 1：抬起事件（win32-input-mode）──────────────────────────────
    // 抬起事件（pressed=false）仅在 win32-input-mode 下投递。
    if !pressed {
        if guard.win32_input {
            return Some(LookupResult::Win32KeyUp);
        }
        return None;
    }

    // ── 层 2：外壳级 Ctrl+Shift+* ──────────────────────────────────────
    // 守卫：overlay / renaming / filetree_dialog 均不拦截
    // （全局拦截，alt screen 也生效，裁决见 main.rs 注释）
    if ctrl && shift && !guard.overlay_open && !guard.renaming && !guard.filetree_dialog_open {
        if is_char(input, 'd') {
            return Some(LookupResult::ShellAction(ShellAction::NewPane));
        }
        if is_char(input, 'w') {
            return Some(LookupResult::ShellAction(ShellAction::ClosePane));
        }
        if is_named(input, WinitNamedKey::Enter) {
            return Some(LookupResult::ShellAction(ShellAction::ToggleMaximizePane));
        }
        // Ctrl+Shift+E：全模式可用的经典直通切换（在此层处理，守卫与 D/W 一致）
        if is_char(input, 'e') {
            return Some(LookupResult::TerminalAction(Action::Term(
                TermAction::ToggleFallback,
            )));
        }
        // Ctrl+Shift+Tab：切换到上一 tab（在 overlay/renaming 检查之后）
        if is_named(input, WinitNamedKey::Tab) {
            return Some(LookupResult::ShellAction(ShellAction::CycleTab(-1isize)));
        }
    }

    // ── 层 3：外壳级 Ctrl+* ────────────────────────────────────────────
    // 守卫：非 alt screen 时生效（或 overlay 已打开，见 main.rs 注释）
    if ctrl
        && (guard.overlay_open || !guard.is_alt_screen)
        && !guard.renaming
        && !guard.filetree_dialog_open
    {
        // overlay 已开时仅 Ctrl+, 可关闭设置页，其余不响应
        if guard.overlay_open {
            if !shift && is_char(input, ',') {
                return Some(LookupResult::ShellAction(ShellAction::ToggleSettings));
            }
            // 其余外壳快捷键在 overlay 打开时静默（不写 PTY，不匹配）
            // 注意：这里不 return None，继续向后走（terminal_focused 闸会拦住）
        } else {
            // overlay 未开时完整外壳快捷键
            if !shift && is_char(input, ',') {
                return Some(LookupResult::ShellAction(ShellAction::ToggleSettings));
            }
            if !shift && is_char(input, 't') {
                return Some(LookupResult::ShellAction(ShellAction::NewTab));
            }
            if !shift && is_char(input, 'w') {
                return Some(LookupResult::ShellAction(ShellAction::CloseTab));
            }
            if !shift && is_char(input, 'b') {
                return Some(LookupResult::ShellAction(ShellAction::ToggleFiletree));
            }
            // Ctrl+Tab：切换到下一 tab（不含 Shift 的路径）
            if !shift && is_named(input, WinitNamedKey::Tab) {
                return Some(LookupResult::ShellAction(ShellAction::CycleTab(1isize)));
            }
        }
    }

    // ── 层 4：终端聚焦闸 ───────────────────────────────────────────────
    // 非聚焦时按键归 egui，不写 PTY（点了侧栏还往 shell 灌字节是事故）。
    if !guard.terminal_focused {
        return None;
    }

    // ── 层 5（安全规则最高优先）：Ctrl+C Running/AltScreen 无条件 Interrupt ──
    // 设计稿风险 3 硬规则：Running / AltScreen / Fallback 态下 Ctrl+C 一律
    // 直通 ETX，任何守卫组合不得吞。
    if ctrl && is_char(input, 'c') && !shift {
        match mode {
            InputMode::Running | InputMode::AltScreen | InputMode::Fallback => {
                return Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::Interrupt,
                )));
            }
            InputMode::Compose => {
                // Compose 态走三级优先级逻辑（见层 9）
            }
        }
    }

    // ── 层 6：Shift+PgUp / Shift+PgDn 翻屏，Shift+Insert 粘贴 ─────────
    // Compose 态 Shift+Insert → 进编辑器（PasteClipboard 走 dispatch）；其余态直通。
    if shift && !ctrl {
        if is_named(input, WinitNamedKey::PageUp) {
            return Some(LookupResult::TerminalAction(Action::Term(
                TermAction::Scroll(ScrollDir::Up),
            )));
        }
        if is_named(input, WinitNamedKey::PageDown) {
            return Some(LookupResult::TerminalAction(Action::Term(
                TermAction::Scroll(ScrollDir::Down),
            )));
        }
        if is_named(input, WinitNamedKey::Insert) {
            // Compose 态：粘贴进编辑器（dispatch 内按 bracketed_paste 状态包装）
            // Running/AltScreen/Fallback 态：与 Ctrl+V 同语义直通 PTY
            return Some(LookupResult::TerminalAction(Action::Term(
                TermAction::PasteClipboard,
            )));
        }
    }

    // ── 层 7：Ctrl+↑/↓ 块跳转（非 alt screen）──────────────────────────
    if ctrl && !guard.is_alt_screen {
        if is_named(input, WinitNamedKey::ArrowUp) {
            return Some(LookupResult::TerminalAction(Action::Term(
                TermAction::JumpBlock(-1),
            )));
        }
        if is_named(input, WinitNamedKey::ArrowDown) {
            return Some(LookupResult::TerminalAction(Action::Term(
                TermAction::JumpBlock(1),
            )));
        }
    }

    // ── 层 9：Compose 态开闸（M4.1 批D1）——设计稿 §4 ──────────────────
    // Running/AltScreen/Fallback 态跳过此层，走后续兜底直通。
    // E9 铁律：Running/AltScreen/Fallback 的 Ctrl+C 已在层 5 无条件 Interrupt，
    // 此层逻辑对非 Compose 态完全不可达（模式检查保证）。
    if mode == InputMode::Compose {
        // 9-a: Enter → Composer(Submit)
        if !ctrl && !shift && !alt && is_named(input, WinitNamedKey::Enter) {
            return Some(LookupResult::TerminalAction(Action::Composer(
                ComposerAction::Submit,
            )));
        }

        // 9-b: Shift+Enter / Alt+Enter → InsertNewline（多行编辑）
        if is_named(input, WinitNamedKey::Enter) && (shift || alt) && !ctrl {
            return Some(LookupResult::TerminalAction(Action::Edit(
                EditAction::InsertNewline,
            )));
        }

        // 9-c: Ctrl+A → 全选
        if ctrl && !shift && is_char(input, 'a') {
            return Some(LookupResult::TerminalAction(Action::Edit(
                EditAction::SelectAll,
            )));
        }

        // 9-d: Backspace → DeleteBackward
        if !ctrl && !shift && !alt && is_named(input, WinitNamedKey::Backspace) {
            return Some(LookupResult::TerminalAction(Action::Edit(
                EditAction::DeleteBackward,
            )));
        }

        // 9-e: Delete → DeleteForward
        if !ctrl && !shift && !alt && is_named(input, WinitNamedKey::Delete) {
            return Some(LookupResult::TerminalAction(Action::Edit(
                EditAction::DeleteForward,
            )));
        }

        // 9-f: Ctrl+Backspace → DeleteWordBackward
        if ctrl && !shift && is_named(input, WinitNamedKey::Backspace) {
            return Some(LookupResult::TerminalAction(Action::Edit(
                EditAction::DeleteWordBackward,
            )));
        }

        // 9-g: 方向键移动（Shift 扩选；Ctrl+↑/↓ 已在层 7 为块跳转，不在此处理）
        if !ctrl {
            if is_named(input, WinitNamedKey::ArrowLeft) {
                return Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::Move {
                        motion: Motion::GraphemeLeft,
                        extend: shift,
                    },
                )));
            }
            if is_named(input, WinitNamedKey::ArrowRight) {
                return Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::Move {
                        motion: Motion::GraphemeRight,
                        extend: shift,
                    },
                )));
            }
            // ↑：光标在首行且无选区 → 历史导航 HistoryPrev；否则行间移动。
            // M4.1 批D2 实现（守卫字段 compose_cursor_at_first_line）。
            if is_named(input, WinitNamedKey::ArrowUp) {
                if !shift && guard.compose_cursor_at_first_line {
                    return Some(LookupResult::TerminalAction(Action::Composer(
                        ComposerAction::HistoryPrev,
                    )));
                }
                return Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::Move {
                        motion: Motion::Up,
                        extend: shift,
                    },
                )));
            }
            // ↓：光标在末行且无选区 → 历史导航 HistoryNext；否则行间移动。
            if is_named(input, WinitNamedKey::ArrowDown) {
                if !shift && guard.compose_cursor_at_last_line {
                    return Some(LookupResult::TerminalAction(Action::Composer(
                        ComposerAction::HistoryNext,
                    )));
                }
                return Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::Move {
                        motion: Motion::Down,
                        extend: shift,
                    },
                )));
            }
        }

        // 9-h: Home / End（Shift 扩选）
        if !ctrl {
            if is_named(input, WinitNamedKey::Home) {
                return Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::Move {
                        motion: Motion::LineStart,
                        extend: shift,
                    },
                )));
            }
            if is_named(input, WinitNamedKey::End) {
                return Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::Move {
                        motion: Motion::LineEnd,
                        extend: shift,
                    },
                )));
            }
        }

        // 9-i: Tab → 无操作 + 状态条提示（M3.4 补全占位）
        if !ctrl && !shift && !alt && is_named(input, WinitNamedKey::Tab) {
            return Some(LookupResult::ComposeTab);
        }

        // 9-j: Ctrl+R → 无操作（D2 历史面板占位）
        if ctrl && !shift && is_char(input, 'r') {
            return Some(LookupResult::ComposeHistorySearch);
        }

        // 9-k: Esc → 关浮层 → 清选区 → 空操作
        if !ctrl && !shift && !alt && is_named(input, WinitNamedKey::Escape) {
            return Some(LookupResult::ComposeEsc);
        }

        // 9-l: Ctrl+L → 直通（清屏是 shell 行为）
        if ctrl && !shift && is_char(input, 'l') {
            return Some(LookupResult::PassThrough);
        }

        // 9-m: Ctrl+C（Compose 态三级逻辑）——设计稿 §4
        //   选区非空 → 复制选区（第一级，M2 现状保留）
        //   选中块 → 复制块输出（第二级，M2 现状保留）
        //   缓冲非空 → CancelLine（清空存放弃稿）
        //   缓冲为空 → Interrupt（下穿 ETX，逃生舱）
        if ctrl && !shift && is_char(input, 'c') {
            if guard.has_selection {
                return Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::CopySelection,
                )));
            }
            if guard.has_selected_block {
                return Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::CopyBlock,
                )));
            }
            if guard.compose_buf_empty {
                // 缓冲为空：下穿 ETX（逃生舱，模式误判时可中断）
                return Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::Interrupt,
                )));
            } else {
                // 缓冲非空：清空存放弃稿
                return Some(LookupResult::TerminalAction(Action::Composer(
                    ComposerAction::CancelLine,
                )));
            }
        }

        // 9-n: Ctrl+D
        //   缓冲空 → 直通 \x04（退 shell）
        //   缓冲非空 → DeleteForward
        if ctrl && !shift && is_char(input, 'd') {
            if guard.compose_buf_empty {
                return Some(LookupResult::PassThrough);
            } else {
                return Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::DeleteForward,
                )));
            }
        }

        // 9-o: Ctrl+V → 粘贴进编辑器（走 dispatch 按 bracketed_paste 包装）
        if ctrl && !shift && is_char(input, 'v') {
            return Some(LookupResult::TerminalAction(Action::Term(
                TermAction::PasteClipboard,
            )));
        }

        // 9-p: 普通字符键 / Space → InsertText（已过滤 ctrl/alt 修饰）
        if !ctrl && !alt {
            // 提取字符串（Character 变体包含字符，Named::Space 特殊处理）
            let text_opt: Option<String> = match &input.logical_key {
                Key::Character(s) => Some(s.to_string()),
                Key::Named(WinitNamedKey::Space) => Some(" ".to_string()),
                _ => None,
            };
            if let Some(text) = text_opt {
                if !text.is_empty() {
                    return Some(LookupResult::TerminalAction(Action::Edit(
                        EditAction::InsertText(text),
                    )));
                }
            }
        }

        // Compose 态未命中的其余键：消费但不写 PTY（不下穿直通）。
        // 例如：F1-F12、Ctrl+字母（未在上方处理的）等。
        return Some(LookupResult::Consumed);
    }

    // ── 层 10（非 Compose 态）：Ctrl+C 三级兜底 ─────────────────────────
    // Compose 态已在层 9 处理 Ctrl+C，此层仅对 Running/Fallback 以下（
    // AltScreen 的 Ctrl+C 已在层 5 安全规则处理）。
    // 此处逻辑保持批B 原状（Running/Fallback 无选区 = Interrupt）。
    if ctrl && is_char(input, 'c') {
        if guard.has_selection {
            return Some(LookupResult::TerminalAction(Action::Term(
                TermAction::CopySelection,
            )));
        }
        if guard.has_selected_block {
            return Some(LookupResult::TerminalAction(Action::Term(
                TermAction::CopyBlock,
            )));
        }
        if shift {
            // Ctrl+Shift+C 无选区时不下发（吞掉）
            return Some(LookupResult::Consumed);
        }
        // 纯 Ctrl+C 无选区 → ETX 中断
        return Some(LookupResult::TerminalAction(Action::Term(
            TermAction::Interrupt,
        )));
    }

    // ── 层 11：Ctrl+V 粘贴（非 Compose 态）────────────────────────────
    if ctrl && is_char(input, 'v') {
        return Some(LookupResult::TerminalAction(Action::Term(
            TermAction::PasteClipboard,
        )));
    }

    // ── 层 12：兜底直通编码 ────────────────────────────────────────────
    // 所有未匹配的按键通过 input::encode_key / encode_key_win32 编码后写 PTY。
    Some(LookupResult::PassThrough)
}

// ─────────────────────────────────────────────────────────────────
// 结果类型
// ─────────────────────────────────────────────────────────────────

/// keymap 查表结果。
#[derive(Debug)]
pub enum LookupResult {
    /// 产出一个终端 / 编辑器 / Composer Action，由 dispatch 执行。
    TerminalAction(Action),
    /// 外壳级动作（新建 tab、文件树开合等），由 main.rs 的外壳逻辑执行。
    ShellAction(ShellAction),
    /// win32-input-mode 抬起事件，由 main.rs 调 encode_key_win32 处理。
    Win32KeyUp,
    /// 按键已消费（不写 PTY、不做其他），如 Ctrl+Shift+C 无选区。
    Consumed,
    /// 兜底直通：调用方用 encode_key / encode_key_win32 编码后写 PTY。
    PassThrough,
    /// Compose 态 Tab 键（M3.4 补全占位）——main.rs 显示状态条提示。
    ComposeTab,
    /// Compose 态 Ctrl+R（D2 历史搜索占位）——main.rs 显示状态条提示。
    ComposeHistorySearch,
    /// Compose 态 Esc（关浮层/清选区/空操作）——main.rs 处理浮层清理。
    ComposeEsc,
}

/// 外壳级动作枚举（不走 dispatch，由 main.rs 外壳逻辑直接处理）。
///
/// 这些动作影响外壳结构（tab / 窗格 / 设置页 / 文件树），
/// 不属于终端输入路径，设计上不经过 dispatch。
#[derive(Debug, Clone)]
pub enum ShellAction {
    /// 新增窗格（Ctrl+Shift+D）。
    NewPane,
    /// 关闭当前窗格（Ctrl+Shift+W）。
    ClosePane,
    /// 最大化 / 还原焦点窗格（Ctrl+Shift+Enter）。
    ToggleMaximizePane,
    /// 切换设置页（Ctrl+,）。
    ToggleSettings,
    /// 新建 tab（Ctrl+T）。
    NewTab,
    /// 关闭当前 tab（Ctrl+W）。
    CloseTab,
    /// 切换文件树（Ctrl+B）。
    ToggleFiletree,
    /// 切换 tab（Ctrl+Tab / Ctrl+Shift+Tab），参数为方向（+1/-1）。
    CycleTab(isize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::{ModifiersState, NamedKey as WinitNamedKey};

    fn default_guard() -> GuardState {
        GuardState {
            terminal_focused: true,
            ..Default::default()
        }
    }

    /// 测试辅助：用 KeyInput 调用 lookup_input（避开 KeyEvent 的 pub(crate) 字段）。
    fn lookup_char(
        ch: &str,
        mods: ModifiersState,
        mode: InputMode,
        pressed: bool,
        guard: &GuardState,
    ) -> Option<LookupResult> {
        lookup_input(&KeyInput::char(ch), mods, mode, pressed, guard)
    }

    fn lookup_named(
        named: WinitNamedKey,
        mods: ModifiersState,
        mode: InputMode,
        pressed: bool,
        guard: &GuardState,
    ) -> Option<LookupResult> {
        lookup_input(&KeyInput::named(named), mods, mode, pressed, guard)
    }

    // ── 安全规则：Running/AltScreen 下 Ctrl+C 无条件 Interrupt ─────────

    #[test]
    fn safety_ctrl_c_running_no_selection_is_interrupt() {
        let result = lookup_char(
            "c",
            ModifiersState::CONTROL,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::Interrupt
                )))
            ),
            "Running 态 Ctrl+C 无选区应为 Interrupt"
        );
    }

    #[test]
    fn safety_ctrl_c_alt_screen_is_interrupt() {
        let guard = GuardState {
            terminal_focused: true,
            is_alt_screen: true,
            ..Default::default()
        };
        let result = lookup_char(
            "c",
            ModifiersState::CONTROL,
            InputMode::AltScreen,
            true,
            &guard,
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::Interrupt
                )))
            ),
            "AltScreen 态 Ctrl+C 应为 Interrupt（安全规则不得吞）"
        );
    }

    #[test]
    fn safety_ctrl_c_fallback_is_interrupt() {
        let result = lookup_char(
            "c",
            ModifiersState::CONTROL,
            InputMode::Fallback,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::Interrupt
                )))
            ),
            "Fallback 态 Ctrl+C 无选区应为 Interrupt"
        );
    }

    #[test]
    fn safety_ctrl_c_running_with_selection_is_interrupt() {
        // Running 态安全规则：无条件 Interrupt，不做复制（设计稿 §4）
        let guard = GuardState {
            terminal_focused: true,
            has_selection: true,
            ..Default::default()
        };
        let result = lookup_char(
            "c",
            ModifiersState::CONTROL,
            InputMode::Running,
            true,
            &guard,
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::Interrupt
                )))
            ),
            "Running 态 Ctrl+C 即使有选区也应为 Interrupt（安全规则）"
        );
    }

    // ── Compose 态 Ctrl+C 三级逻辑 ─────────────────────────────────────

    #[test]
    fn compose_ctrl_c_with_selection_is_copy_selection() {
        let guard = GuardState {
            terminal_focused: true,
            has_selection: true,
            ..Default::default()
        };
        let result = lookup_char(
            "c",
            ModifiersState::CONTROL,
            InputMode::Compose,
            true,
            &guard,
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::CopySelection
                )))
            ),
            "Compose 态 Ctrl+C 有选区 → 复制选区"
        );
    }

    #[test]
    fn compose_ctrl_c_with_block_is_copy_block() {
        let guard = GuardState {
            terminal_focused: true,
            has_selected_block: true,
            ..Default::default()
        };
        let result = lookup_char(
            "c",
            ModifiersState::CONTROL,
            InputMode::Compose,
            true,
            &guard,
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::CopyBlock
                )))
            ),
            "Compose 态 Ctrl+C 有选中块 → 复制块"
        );
    }

    #[test]
    fn compose_ctrl_shift_c_no_selection_consumed() {
        // Ctrl+Shift+C 在 Compose 态：层 9-m 要求 !shift，不命中；
        // 层 9-p 要求 !ctrl，不命中；Compose 兜底 → Consumed。
        let result = lookup_char(
            "c",
            ModifiersState::CONTROL | ModifiersState::SHIFT,
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(result, Some(LookupResult::Consumed)),
            "Compose 态 Ctrl+Shift+C 无选区应被消费（不下发）"
        );
    }

    #[test]
    fn compose_ctrl_c_buf_nonempty_is_cancel_line() {
        // compose_buf_empty = false（默认值） → 缓冲非空 → CancelLine（清空存放弃稿）
        let guard = GuardState {
            terminal_focused: true,
            compose_buf_empty: false,
            ..Default::default()
        };
        let result = lookup_char(
            "c",
            ModifiersState::CONTROL,
            InputMode::Compose,
            true,
            &guard,
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Composer(
                    ComposerAction::CancelLine
                )))
            ),
            "Compose 态 Ctrl+C 缓冲非空 → CancelLine（清空存放弃稿）"
        );
    }

    #[test]
    fn compose_ctrl_c_empty_buf_is_interrupt() {
        // compose_buf_empty = true → 缓冲为空 → Interrupt（ETX 逃生舱）
        let guard = GuardState {
            terminal_focused: true,
            compose_buf_empty: true,
            ..Default::default()
        };
        let result = lookup_char(
            "c",
            ModifiersState::CONTROL,
            InputMode::Compose,
            true,
            &guard,
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::Interrupt
                )))
            ),
            "Compose 态 Ctrl+C 缓冲为空 → Interrupt（ETX 逃生舱）"
        );
    }

    // ── 翻屏 ─────────────────────────────────────────────────────────────

    #[test]
    fn shift_pgup_scrolls_up() {
        let result = lookup_named(
            WinitNamedKey::PageUp,
            ModifiersState::SHIFT,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::Scroll(ScrollDir::Up)
                )))
            ),
            "Shift+PgUp → 向上翻屏"
        );
    }

    #[test]
    fn shift_pgdn_scrolls_down() {
        let result = lookup_named(
            WinitNamedKey::PageDown,
            ModifiersState::SHIFT,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::Scroll(ScrollDir::Down)
                )))
            ),
            "Shift+PgDn → 向下翻屏"
        );
    }

    #[test]
    fn shift_insert_pastes() {
        let result = lookup_named(
            WinitNamedKey::Insert,
            ModifiersState::SHIFT,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::PasteClipboard
                )))
            ),
            "Shift+Insert → 粘贴"
        );
    }

    // ── 块跳转 ───────────────────────────────────────────────────────────

    #[test]
    fn ctrl_arrow_up_jumps_block_backward() {
        let result = lookup_named(
            WinitNamedKey::ArrowUp,
            ModifiersState::CONTROL,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::JumpBlock(-1)
                )))
            ),
            "Ctrl+↑ → 跳到上一块"
        );
    }

    #[test]
    fn ctrl_arrow_down_jumps_block_forward() {
        let result = lookup_named(
            WinitNamedKey::ArrowDown,
            ModifiersState::CONTROL,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::JumpBlock(1)
                )))
            ),
            "Ctrl+↓ → 跳到下一块"
        );
    }

    #[test]
    fn ctrl_arrow_up_alt_screen_passthrough() {
        // AltScreen 下 Ctrl+↑/↓ 放行（vim 应用用这些键）
        let guard = GuardState {
            terminal_focused: true,
            is_alt_screen: true,
            ..Default::default()
        };
        let result = lookup_named(
            WinitNamedKey::ArrowUp,
            ModifiersState::CONTROL,
            InputMode::AltScreen,
            true,
            &guard,
        );
        assert!(
            matches!(result, Some(LookupResult::PassThrough)),
            "AltScreen 下 Ctrl+↑ 应直通（不做块跳转）"
        );
    }

    // ── 粘贴 ─────────────────────────────────────────────────────────────

    #[test]
    fn ctrl_v_pastes() {
        let result = lookup_char(
            "v",
            ModifiersState::CONTROL,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::PasteClipboard
                )))
            ),
            "Ctrl+V → 粘贴"
        );
    }

    // ── 外壳快捷键 ───────────────────────────────────────────────────────

    #[test]
    fn ctrl_shift_d_new_pane() {
        let result = lookup_char(
            "d",
            ModifiersState::CONTROL | ModifiersState::SHIFT,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::ShellAction(ShellAction::NewPane))
            ),
            "Ctrl+Shift+D → 新增窗格"
        );
    }

    #[test]
    fn ctrl_shift_w_close_pane() {
        let result = lookup_char(
            "w",
            ModifiersState::CONTROL | ModifiersState::SHIFT,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::ShellAction(ShellAction::ClosePane))
            ),
            "Ctrl+Shift+W → 关闭窗格"
        );
    }

    #[test]
    fn ctrl_shift_enter_maximize_pane() {
        let result = lookup_named(
            WinitNamedKey::Enter,
            ModifiersState::CONTROL | ModifiersState::SHIFT,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::ShellAction(ShellAction::ToggleMaximizePane))
            ),
            "Ctrl+Shift+Enter → 最大化窗格"
        );
    }

    #[test]
    fn ctrl_comma_toggle_settings() {
        let result = lookup_char(
            ",",
            ModifiersState::CONTROL,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::ShellAction(ShellAction::ToggleSettings))
            ),
            "Ctrl+, → 设置页开合"
        );
    }

    #[test]
    fn ctrl_t_new_tab() {
        let result = lookup_char(
            "t",
            ModifiersState::CONTROL,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(result, Some(LookupResult::ShellAction(ShellAction::NewTab))),
            "Ctrl+T → 新建 tab"
        );
    }

    #[test]
    fn ctrl_w_close_tab() {
        let result = lookup_char(
            "w",
            ModifiersState::CONTROL,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::ShellAction(ShellAction::CloseTab))
            ),
            "Ctrl+W → 关闭 tab"
        );
    }

    #[test]
    fn ctrl_b_toggle_filetree() {
        let result = lookup_char(
            "b",
            ModifiersState::CONTROL,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::ShellAction(ShellAction::ToggleFiletree))
            ),
            "Ctrl+B → 文件树开合"
        );
    }

    #[test]
    fn ctrl_tab_cycle_forward() {
        let result = lookup_named(
            WinitNamedKey::Tab,
            ModifiersState::CONTROL,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::ShellAction(ShellAction::CycleTab(1)))
            ),
            "Ctrl+Tab → 切换到下一 tab"
        );
    }

    #[test]
    fn ctrl_shift_tab_cycle_backward() {
        let result = lookup_named(
            WinitNamedKey::Tab,
            ModifiersState::CONTROL | ModifiersState::SHIFT,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::ShellAction(ShellAction::CycleTab(-1)))
            ),
            "Ctrl+Shift+Tab → 切换到上一 tab"
        );
    }

    // ── 终端聚焦闸 ───────────────────────────────────────────────────────

    #[test]
    fn not_focused_returns_none() {
        let guard = GuardState {
            terminal_focused: false,
            ..Default::default()
        };
        let result = lookup_char(
            "a",
            ModifiersState::empty(),
            InputMode::Running,
            true,
            &guard,
        );
        assert!(result.is_none(), "终端非聚焦时应返回 None（不写 PTY）");
    }

    // ── Ctrl+Shift+E 经典直通 ────────────────────────────────────────────

    #[test]
    fn ctrl_shift_e_toggles_fallback_running() {
        let result = lookup_char(
            "e",
            ModifiersState::CONTROL | ModifiersState::SHIFT,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::ToggleFallback
                )))
            ),
            "Ctrl+Shift+E → 切换经典直通模式（Running 态）"
        );
    }

    #[test]
    fn ctrl_shift_e_works_in_compose_mode() {
        let result = lookup_char(
            "e",
            ModifiersState::CONTROL | ModifiersState::SHIFT,
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::ToggleFallback
                )))
            ),
            "Ctrl+Shift+E 在 Compose 态也应生效（全模式可用）"
        );
    }

    #[test]
    fn ctrl_shift_e_works_in_fallback_mode() {
        let result = lookup_char(
            "e",
            ModifiersState::CONTROL | ModifiersState::SHIFT,
            InputMode::Fallback,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::ToggleFallback
                )))
            ),
            "Ctrl+Shift+E 在 Fallback 态也应生效"
        );
    }

    // ── 兜底直通 ─────────────────────────────────────────────────────────

    #[test]
    fn normal_key_passes_through() {
        let result = lookup_char(
            "a",
            ModifiersState::empty(),
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(result, Some(LookupResult::PassThrough)),
            "普通字符键应 PassThrough 直通编码"
        );
    }

    #[test]
    fn enter_passes_through_in_running() {
        let result = lookup_named(
            WinitNamedKey::Enter,
            ModifiersState::empty(),
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(result, Some(LookupResult::PassThrough)),
            "Running 态 Enter 应 PassThrough（直通 \\r）"
        );
    }

    // ── 抬起事件 ─────────────────────────────────────────────────────────

    #[test]
    fn key_up_without_win32_returns_none() {
        let guard = GuardState {
            terminal_focused: true,
            win32_input: false,
            ..Default::default()
        };
        let result = lookup_char(
            "a",
            ModifiersState::empty(),
            InputMode::Running,
            false,
            &guard,
        );
        assert!(result.is_none(), "非 win32-input 模式下抬起事件应返回 None");
    }

    #[test]
    fn key_up_with_win32_returns_win32_key_up() {
        let guard = GuardState {
            terminal_focused: true,
            win32_input: true,
            ..Default::default()
        };
        let result = lookup_char(
            "a",
            ModifiersState::empty(),
            InputMode::Running,
            false,
            &guard,
        );
        assert!(
            matches!(result, Some(LookupResult::Win32KeyUp)),
            "win32-input 模式下抬起事件应返回 Win32KeyUp"
        );
    }

    // ── 守卫条件：overlay 打开 ───────────────────────────────────────────

    #[test]
    fn overlay_open_blocks_ctrl_t() {
        // overlay 打开时 terminal_focused=false，外壳快捷键层 Ctrl+T 不产出 NewTab
        let guard = GuardState {
            terminal_focused: false,
            overlay_open: true,
            ..Default::default()
        };
        let result = lookup_char(
            "t",
            ModifiersState::CONTROL,
            InputMode::Running,
            true,
            &guard,
        );
        assert!(
            result.is_none(),
            "overlay 打开且非聚焦时 Ctrl+T 应返回 None"
        );
    }

    #[test]
    fn overlay_open_allows_ctrl_comma_close() {
        // overlay 打开时 Ctrl+, 仍可关闭设置页（外壳快捷键层在聚焦闸之前）
        let guard = GuardState {
            terminal_focused: false,
            overlay_open: true,
            is_alt_screen: false,
            ..Default::default()
        };
        let result = lookup_char(
            ",",
            ModifiersState::CONTROL,
            InputMode::Running,
            true,
            &guard,
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::ShellAction(ShellAction::ToggleSettings))
            ),
            "overlay 打开时 Ctrl+, 应仍可触发（关闭设置页）"
        );
    }

    // ────────────────────────────────────────────────────────────────
    // M4.1 批D1：Compose 态完整键位语义单测（设计稿 §4）
    // ────────────────────────────────────────────────────────────────

    // ── Enter → Submit ───────────────────────────────────────────────

    #[test]
    fn compose_enter_无修饰_是_submit() {
        let result = lookup_named(
            WinitNamedKey::Enter,
            ModifiersState::empty(),
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Composer(
                    ComposerAction::Submit
                )))
            ),
            "Compose 态 Enter → Submit"
        );
    }

    #[test]
    fn compose_shift_enter_是_insert_newline() {
        let result = lookup_named(
            WinitNamedKey::Enter,
            ModifiersState::SHIFT,
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::InsertNewline
                )))
            ),
            "Compose 态 Shift+Enter → InsertNewline"
        );
    }

    #[test]
    fn running_enter_是_passthrough_不是_submit() {
        // Running 态 Enter 不走 Compose 层，兜底直通
        let result = lookup_named(
            WinitNamedKey::Enter,
            ModifiersState::empty(),
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(result, Some(LookupResult::PassThrough)),
            "Running 态 Enter 应直通（不走 Compose 层）"
        );
    }

    // ── 删除键 ───────────────────────────────────────────────────────

    #[test]
    fn compose_backspace_是_delete_backward() {
        let result = lookup_named(
            WinitNamedKey::Backspace,
            ModifiersState::empty(),
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::DeleteBackward
                )))
            ),
            "Compose 态 Backspace → DeleteBackward"
        );
    }

    #[test]
    fn compose_delete_是_delete_forward() {
        let result = lookup_named(
            WinitNamedKey::Delete,
            ModifiersState::empty(),
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::DeleteForward
                )))
            ),
            "Compose 态 Delete → DeleteForward"
        );
    }

    #[test]
    fn compose_ctrl_backspace_是_delete_word_backward() {
        let result = lookup_named(
            WinitNamedKey::Backspace,
            ModifiersState::CONTROL,
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::DeleteWordBackward
                )))
            ),
            "Compose 态 Ctrl+Backspace → DeleteWordBackward"
        );
    }

    // ── 移动键 ───────────────────────────────────────────────────────

    #[test]
    fn compose_left_是_grapheme_left() {
        let result = lookup_named(
            WinitNamedKey::ArrowLeft,
            ModifiersState::empty(),
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::Move {
                        motion: Motion::GraphemeLeft,
                        extend: false,
                    }
                )))
            ),
            "Compose 态 ← → GraphemeLeft (no extend)"
        );
    }

    #[test]
    fn compose_shift_right_是_grapheme_right_extend() {
        let result = lookup_named(
            WinitNamedKey::ArrowRight,
            ModifiersState::SHIFT,
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::Move {
                        motion: Motion::GraphemeRight,
                        extend: true,
                    }
                )))
            ),
            "Compose 态 Shift+→ → GraphemeRight extend=true"
        );
    }

    #[test]
    fn compose_home_是_line_start() {
        let result = lookup_named(
            WinitNamedKey::Home,
            ModifiersState::empty(),
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::Move {
                        motion: Motion::LineStart,
                        extend: false,
                    }
                )))
            ),
            "Compose 态 Home → LineStart"
        );
    }

    #[test]
    fn compose_ctrl_a_是_select_all() {
        let result = lookup_char(
            "a",
            ModifiersState::CONTROL,
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::SelectAll
                )))
            ),
            "Compose 态 Ctrl+A → SelectAll"
        );
    }

    // ── Tab / Ctrl+R / Esc ───────────────────────────────────────────

    #[test]
    fn compose_tab_是_compose_tab_结果() {
        let result = lookup_named(
            WinitNamedKey::Tab,
            ModifiersState::empty(),
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(result, Some(LookupResult::ComposeTab)),
            "Compose 态 Tab → ComposeTab 占位"
        );
    }

    #[test]
    fn compose_ctrl_r_是_compose_history_search_结果() {
        let result = lookup_char(
            "r",
            ModifiersState::CONTROL,
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(result, Some(LookupResult::ComposeHistorySearch)),
            "Compose 态 Ctrl+R → ComposeHistorySearch 占位"
        );
    }

    #[test]
    fn compose_esc_是_compose_esc_结果() {
        let result = lookup_named(
            WinitNamedKey::Escape,
            ModifiersState::empty(),
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(result, Some(LookupResult::ComposeEsc)),
            "Compose 态 Esc → ComposeEsc"
        );
    }

    // ── Ctrl+L 直通（设计稿 §4）──────────────────────────────────────

    #[test]
    fn compose_ctrl_l_是_passthrough() {
        let result = lookup_char(
            "l",
            ModifiersState::CONTROL,
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(result, Some(LookupResult::PassThrough)),
            "Compose 态 Ctrl+L → PassThrough（清屏是 shell 行为）"
        );
    }

    // ── Ctrl+D 两态（设计稿 §4）──────────────────────────────────────

    #[test]
    fn compose_ctrl_d_empty_buf_是_passthrough() {
        // 缓冲为空 → 直通 \x04（退 shell）
        let guard = GuardState {
            terminal_focused: true,
            compose_buf_empty: true,
            ..Default::default()
        };
        let result = lookup_char(
            "d",
            ModifiersState::CONTROL,
            InputMode::Compose,
            true,
            &guard,
        );
        assert!(
            matches!(result, Some(LookupResult::PassThrough)),
            "Compose 态 Ctrl+D 缓冲空 → PassThrough（退 shell）"
        );
    }

    #[test]
    fn compose_ctrl_d_nonempty_buf_是_delete_forward() {
        // 缓冲非空 → DeleteForward
        let guard = GuardState {
            terminal_focused: true,
            compose_buf_empty: false,
            ..Default::default()
        };
        let result = lookup_char(
            "d",
            ModifiersState::CONTROL,
            InputMode::Compose,
            true,
            &guard,
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::DeleteForward
                )))
            ),
            "Compose 态 Ctrl+D 缓冲非空 → DeleteForward"
        );
    }

    // ── 普通字符 → InsertText ─────────────────────────────────────────

    #[test]
    fn compose_普通字符_是_insert_text() {
        let result = lookup_char(
            "h",
            ModifiersState::empty(),
            InputMode::Compose,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::InsertText(_)
                )))
            ),
            "Compose 态普通字符 → InsertText"
        );
    }

    // ── M4.1 批D2：历史导航 ↑/↓ ────────────────────────────────────

    #[test]
    fn compose_up_首行无选区_是_history_prev() {
        let guard = GuardState {
            terminal_focused: true,
            compose_cursor_at_first_line: true,
            ..Default::default()
        };
        let result = lookup_named(
            WinitNamedKey::ArrowUp,
            ModifiersState::empty(),
            InputMode::Compose,
            true,
            &guard,
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Composer(
                    ComposerAction::HistoryPrev
                )))
            ),
            "Compose 态首行 ↑ → HistoryPrev"
        );
    }

    #[test]
    fn compose_up_非首行_是_move_up() {
        let guard = GuardState {
            terminal_focused: true,
            compose_cursor_at_first_line: false,
            ..Default::default()
        };
        let result = lookup_named(
            WinitNamedKey::ArrowUp,
            ModifiersState::empty(),
            InputMode::Compose,
            true,
            &guard,
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::Move {
                        motion: Motion::Up,
                        extend: false,
                    }
                )))
            ),
            "Compose 态非首行 ↑ → Move Up"
        );
    }

    #[test]
    fn compose_down_末行无选区_是_history_next() {
        let guard = GuardState {
            terminal_focused: true,
            compose_cursor_at_last_line: true,
            ..Default::default()
        };
        let result = lookup_named(
            WinitNamedKey::ArrowDown,
            ModifiersState::empty(),
            InputMode::Compose,
            true,
            &guard,
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Composer(
                    ComposerAction::HistoryNext
                )))
            ),
            "Compose 态末行 ↓ → HistoryNext"
        );
    }

    #[test]
    fn compose_shift_up_首行_是_move_up_extend() {
        // Shift+↑ 即使在首行也是扩选移动，不触发历史导航。
        let guard = GuardState {
            terminal_focused: true,
            compose_cursor_at_first_line: true,
            ..Default::default()
        };
        let result = lookup_named(
            WinitNamedKey::ArrowUp,
            ModifiersState::SHIFT,
            InputMode::Compose,
            true,
            &guard,
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Edit(
                    EditAction::Move {
                        motion: Motion::Up,
                        extend: true,
                    }
                )))
            ),
            "Compose 态 Shift+↑ 首行 → Move Up extend=true（不触发历史导航）"
        );
    }

    // ── E9 铁律自查：Running/AltScreen Ctrl+C 路径无新增拦截 ─────────

    #[test]
    fn e9_running_ctrl_c_无选区_interrupt() {
        // E9 铁律：Running 态 Ctrl+C 无条件 Interrupt（层 5 安全规则）
        let result = lookup_char(
            "c",
            ModifiersState::CONTROL,
            InputMode::Running,
            true,
            &default_guard(),
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::Interrupt
                )))
            ),
            "E9 铁律：Running 态 Ctrl+C → Interrupt（不受 compose_buf_empty 影响）"
        );
    }

    #[test]
    fn e9_altscreen_ctrl_c_无选区_interrupt() {
        // E9 铁律：AltScreen 态 Ctrl+C 无条件 Interrupt（层 5 安全规则）
        let guard = GuardState {
            terminal_focused: true,
            is_alt_screen: true,
            ..Default::default()
        };
        let result = lookup_char(
            "c",
            ModifiersState::CONTROL,
            InputMode::AltScreen,
            true,
            &guard,
        );
        assert!(
            matches!(
                result,
                Some(LookupResult::TerminalAction(Action::Term(
                    TermAction::Interrupt
                )))
            ),
            "E9 铁律：AltScreen 态 Ctrl+C → Interrupt（不受 compose_buf_empty 影响）"
        );
    }
}
