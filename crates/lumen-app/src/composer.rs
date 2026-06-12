//! footer 输入区视图组装（M4.1 批D1，feature = "input-editor"）——设计稿 §7.1。
//!
//! app 层按 [`effective_mode`] 的结果组装 [`ComposerView`] 传给 renderer；
//! renderer 不依赖 lumen-editor，数据流：
//! `mode → compose_view_for_mode → Option<&ComposerView> → renderer.render(...)`。
//!
//! # 节拍纪律（设计稿 §7.4）
//! 编辑器变更直接 `request_redraw()`，不触碰 PTY debounce 字段。
//! 本模块函数是纯函数，不持有状态，每帧按需调用。
//!
//! # 设计稿对应章节
//! 设计稿 §2「输入区形态」列、§7.1「footer 输入区」、§7.4「节拍纪律」。
//!
//! # Ghost text（M4.1 批3）
//! PSReadLine 内建预测已在 shell 侧禁用（Set-PSReadLineOption -PredictionSource None），
//! 本模块实现历史前缀匹配 ghost text 作为替代，保留补全体验。
//! ghost = history.find_ghost_prefix(current_text)，由 main.rs 计算后作为参数传入。

use crate::i18n;
use crate::mode::InputMode;
use lumen_renderer::composer_view::ComposerView;

#[cfg(feature = "input-editor")]
use lumen_editor::EditorView;
#[cfg(feature = "input-editor")]
use lumen_renderer::composer_view::{normalize_selection, ExitBadge, PreeditState};

/// 按当前有效输入模式组装 [`ComposerView`]。
///
/// - [`InputMode::Compose`] → 完整编辑卡片，内容来自 `editor_view`（真实 EditorView 内容）
/// - [`InputMode::Running`] → 等高状态条（文案 i18n）
/// - [`InputMode::AltScreen`] → 隐藏（grid 收回全高）
/// - [`InputMode::Fallback`] → 隐藏（与 AltScreen 同路径；底部状态栏已有"经典直通"模式指示）
///
/// # 设计稿铁律
/// Compose ↔ Running 切换返回等高视图（均为 1 行），不改 footer 高度，
/// 不触发 `term.resize` / `pty.resize`（防 resize 风暴，设计稿 §7.1）。
/// Fallback 隐藏（footer_px=0），grid 收回全高，与 AltScreen 行为一致。
///
/// # Arguments
/// * `mode` - 当前有效输入模式（由 [`crate::mode::effective_mode`] 推导）。
/// * `editor_view` - Compose 态时的编辑器只读视图（仅 Compose 态读取内容）。
/// * `preedit` - IME 预编辑状态（M4.1 批D2，仅 Compose 态有效）。
/// * `exit_badge` - 退出码角标（M4.1 批D2，仅 Compose 态显示）。
/// * `ghost` - 历史联想后缀（M4.1 批3）：Compose 态 + 光标在文末时追加渲染；
///   None 表示无联想（缓冲为空、多行或无前缀匹配）。
#[cfg(feature = "input-editor")]
pub fn compose_view_for_mode(
    mode: InputMode,
    editor_view: EditorView<'_>,
    preedit: Option<PreeditState>,
    exit_badge: Option<ExitBadge>,
    ghost: Option<String>,
) -> ComposerView {
    let s = i18n::strings();
    match mode {
        InputMode::Compose => {
            // 批D1：从真实 EditorView 读取内容，填充 lines 和 cursor。
            let lines: Vec<String> = editor_view
                .lines()
                .map(|l| l.to_owned())
                .collect::<Vec<_>>()
                .into_iter()
                .collect();
            let lines = if lines.is_empty() {
                vec![String::new()]
            } else {
                lines
            };
            let cur = editor_view.cursor();
            // 占位提示（M4.1 批E）：lines 全空时显示 composer_placeholder。
            let all_empty = lines.iter().all(|l| l.is_empty());
            let placeholder = all_empty.then(|| s.composer_placeholder.to_owned());
            // M4.1 批F：从 EditorView 取选区，规范化后填入 ComposerView。
            // anchor ≠ cursor 时为非空选区；normalize_selection 返回 Some(FooterSelection)。
            // 有选区时 ghost text 不显示（视觉冲突，系统惯例选区时无 inline 补全）。
            let editor_sel = editor_view.selection();
            let selection = normalize_selection(
                (editor_sel.anchor.line, editor_sel.anchor.byte),
                (editor_sel.cursor.line, editor_sel.cursor.byte),
            );
            let ghost = if selection.is_some() { None } else { ghost };
            // Position { line, byte }——与 ComposerView.cursor (行, 字节偏移) 语义相同
            ComposerView {
                kind: lumen_renderer::composer_view::FooterKind::Composer,
                lines,
                cursor: (cur.line, cur.byte),
                selection,   // M4.1 批F：文本选区（规范化后，None=纯光标）
                preedit,     // M4.1 批D2：IME 预编辑
                exit_badge,  // M4.1 批D2：退出码角标
                placeholder, // M4.1 批E：占位提示（仅空编辑器显示）
                ghost,       // M4.1 批3：历史联想后缀（有选区时为 None）
            }
        }
        InputMode::Running => ComposerView::running(s.footer_running_text),
        // AltScreen：全屏 TUI 让位，footer 隐藏，grid 收回全高。
        InputMode::AltScreen => ComposerView::hidden(),
        // Fallback：shell integration 未生效，footer 隐藏（与 AltScreen 同路径，
        // footer_px=0，grid 收回全高；底部状态栏已有"经典直通"模式指示）。
        InputMode::Fallback => ComposerView::hidden(),
    }
}

/// 无 input-editor feature 时的退化版本（保持原批C 兼容行为）。
#[cfg(not(feature = "input-editor"))]
pub fn compose_view_for_mode(mode: InputMode) -> ComposerView {
    let s = i18n::strings();
    match mode {
        InputMode::Compose => ComposerView::compose_empty(),
        InputMode::Running => ComposerView::running(s.footer_running_text),
        InputMode::AltScreen | InputMode::Fallback => ComposerView::hidden(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_renderer::composer_view::FooterKind;

    // ── 四模式 → 形态映射（input-editor feature）────────────────────────

    #[cfg(feature = "input-editor")]
    mod with_editor {
        use super::*;

        /// 构造一个空 Editor，拿 EditorView 供测试。
        fn empty_view() -> lumen_editor::Editor {
            lumen_editor::Editor::default()
        }

        #[test]
        fn compose_模式_产出_composer_形态() {
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, None);
            assert_eq!(
                v.kind,
                FooterKind::Composer,
                "Compose 模式应产出 Composer 形态"
            );
            assert!(v.is_visible(), "Compose 形态应可见");
        }

        #[test]
        fn compose_空编辑器_一行() {
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, None);
            assert_eq!(v.lines.len(), 1, "空编辑器应有 1 行");
            assert_eq!(v.cursor, (0, 0), "空编辑器光标在原点");
        }

        #[test]
        fn running_模式_产出_statusbar_形态() {
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::Running, editor.view(), None, None, None);
            assert_eq!(
                v.kind,
                FooterKind::StatusBar,
                "Running 模式应产出 StatusBar 形态"
            );
            assert!(v.is_visible(), "Running 形态应可见");
        }

        #[test]
        fn altscreen_模式_产出_hidden_形态() {
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::AltScreen, editor.view(), None, None, None);
            assert_eq!(
                v.kind,
                FooterKind::Hidden,
                "AltScreen 模式应产出 Hidden 形态"
            );
            assert!(!v.is_visible(), "AltScreen 形态应隐藏");
        }

        #[test]
        fn fallback_模式_产出_hidden_形态() {
            // 第十四轮：Fallback 改为隐藏（与 AltScreen 同路径），
            // footer_px=0，grid 收回全高；底部状态栏已有"经典直通"模式指示，无需重复。
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::Fallback, editor.view(), None, None, None);
            assert_eq!(
                v.kind,
                FooterKind::Hidden,
                "Fallback 模式应产出 Hidden 形态（底部状态栏已有模式指示）"
            );
            assert!(!v.is_visible(), "Fallback 形态应隐藏");
        }

        /// Compose ↔ Running 等高铁律：两者均为 1 行，footer_height_px 返回相同值。
        #[test]
        fn compose_与_running_等高() {
            use lumen_renderer::composer_view::footer_height_px;
            let editor = empty_view();
            let v_compose =
                compose_view_for_mode(InputMode::Compose, editor.view(), None, None, None);
            let v_running =
                compose_view_for_mode(InputMode::Running, editor.view(), None, None, None);
            let h_c = footer_height_px(Some(&v_compose), 20.0, 6.0, 1000.0);
            let h_r = footer_height_px(Some(&v_running), 20.0, 6.0, 1000.0);
            assert_eq!(
                h_c, h_r,
                "Compose({h_c}) 与 Running({h_r}) 应等高，否则切换触发 resize"
            );
        }

        /// AltScreen 隐藏态高度为 0（grid 收回全高）。
        #[test]
        fn altscreen_高度为零() {
            use lumen_renderer::composer_view::footer_height_px;
            let editor = empty_view();
            let v_alt =
                compose_view_for_mode(InputMode::AltScreen, editor.view(), None, None, None);
            assert_eq!(
                footer_height_px(Some(&v_alt), 20.0, 6.0, 1000.0),
                0.0,
                "AltScreen 高度应为 0"
            );
        }

        /// Fallback 隐藏态高度为 0（第十四轮变更：与 AltScreen 同路径）。
        #[test]
        fn fallback_高度为零() {
            use lumen_renderer::composer_view::footer_height_px;
            let editor = empty_view();
            let v_fb = compose_view_for_mode(InputMode::Fallback, editor.view(), None, None, None);
            let h_fb = footer_height_px(Some(&v_fb), 20.0, 6.0, 1000.0);
            assert_eq!(h_fb, 0.0, "Fallback 隐藏态高度应为 0");
        }

        /// ghost text 传入 Compose 态时应被带入 ComposerView（M4.1 批3）。
        #[test]
        fn ghost_传入_compose_视图() {
            let editor = empty_view();
            let ghost = Some("tatus --short".to_owned());
            let v =
                compose_view_for_mode(InputMode::Compose, editor.view(), None, None, ghost.clone());
            assert_eq!(v.ghost, ghost, "ghost 应被带入 ComposerView");
        }

        /// ghost text 在非 Compose 态（Running）时应不影响形态（Running 忽略 ghost）。
        #[test]
        fn ghost_非compose_态_不影响形态() {
            let editor = empty_view();
            let v = compose_view_for_mode(
                InputMode::Running,
                editor.view(),
                None,
                None,
                Some("suffix".to_owned()),
            );
            // Running 态不消费 ghost，ComposerView::running 不含 ghost
            assert_eq!(
                v.kind,
                lumen_renderer::composer_view::FooterKind::StatusBar,
                "Running 态形态不受 ghost 影响"
            );
        }

        // ── M4.1 批F：选区传递 ──────────────────────────────────────────

        /// 空编辑器（纯光标）：selection 应为 None。
        #[test]
        fn 空编辑器_无选区() {
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, None);
            assert_eq!(v.selection, None, "空编辑器应无选区");
        }

        /// 编辑器有选区时：selection 应被填入 Some(FooterSelection)，且已规范化（start ≤ end）。
        #[test]
        fn 有选区_正向_传入_compose_视图() {
            use lumen_editor::{EditAction, Editor, Motion};
            use lumen_renderer::composer_view::FooterSelection;

            let mut editor = Editor::default();
            editor.apply(&EditAction::InsertText("hello".to_string()));
            editor.apply(&EditAction::Move {
                motion: Motion::LineStart,
                extend: false,
            });
            // Shift+Right × 3 → 正向选区 anchor=(0,0) cursor=(0,3)
            for _ in 0..3 {
                editor.apply(&EditAction::Move {
                    motion: Motion::GraphemeRight,
                    extend: true,
                });
            }
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, None);
            assert_eq!(
                v.selection,
                Some(FooterSelection {
                    start: (0, 0),
                    end: (0, 3)
                }),
                "正向选区应被正确填入"
            );
        }

        /// 逆向选区（cursor < anchor）：规范化后 start ≤ end。
        #[test]
        fn 有选区_逆向_规范化() {
            use lumen_editor::{EditAction, Editor, Motion};
            use lumen_renderer::composer_view::FooterSelection;

            let mut editor = Editor::default();
            editor.apply(&EditAction::InsertText("hello".to_string()));
            // 光标在行尾，Shift+Left × 2 → 逆向选区 anchor=(0,5) cursor=(0,3)
            for _ in 0..2 {
                editor.apply(&EditAction::Move {
                    motion: Motion::GraphemeLeft,
                    extend: true,
                });
            }
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, None);
            // 规范化后：start=(0,3) end=(0,5)
            assert_eq!(
                v.selection,
                Some(FooterSelection {
                    start: (0, 3),
                    end: (0, 5)
                }),
                "逆向选区应被规范化为 start ≤ end"
            );
        }

        /// 有选区时：ghost text 应被清空（视觉冲突，选区时无 inline 补全）。
        #[test]
        fn 有选区_ghost_被清空() {
            use lumen_editor::{EditAction, Editor, Motion};

            let mut editor = Editor::default();
            editor.apply(&EditAction::InsertText("hello".to_string()));
            editor.apply(&EditAction::Move {
                motion: Motion::LineStart,
                extend: false,
            });
            editor.apply(&EditAction::Move {
                motion: Motion::GraphemeRight,
                extend: true,
            });
            // 有选区，传入 ghost
            let ghost = Some("suffix".to_owned());
            let v = compose_view_for_mode(InputMode::Compose, editor.view(), None, None, ghost);
            assert!(v.selection.is_some(), "应有选区");
            assert_eq!(v.ghost, None, "有选区时 ghost 应被清空");
        }
    }

    // ── 无 input-editor feature 时的退化版（保持批C 兼容）─────────────

    #[cfg(not(feature = "input-editor"))]
    mod without_editor {
        use super::*;

        #[test]
        fn compose_模式_产出_composer_形态() {
            let v = compose_view_for_mode(InputMode::Compose);
            assert_eq!(v.kind, FooterKind::Composer);
            assert!(v.is_visible());
        }

        #[test]
        fn running_模式_产出_statusbar_形态() {
            let v = compose_view_for_mode(InputMode::Running);
            assert_eq!(v.kind, FooterKind::StatusBar);
            assert!(v.is_visible());
        }

        #[test]
        fn altscreen_fallback_产出_hidden_形态() {
            let v_alt = compose_view_for_mode(InputMode::AltScreen);
            let v_fb = compose_view_for_mode(InputMode::Fallback);
            assert_eq!(v_alt.kind, FooterKind::Hidden);
            assert_eq!(v_fb.kind, FooterKind::Hidden);
        }

        #[test]
        fn compose_与_running_等高() {
            use lumen_renderer::composer_view::footer_height_px;
            let v_compose = compose_view_for_mode(InputMode::Compose);
            let v_running = compose_view_for_mode(InputMode::Running);
            let h_c = footer_height_px(Some(&v_compose), 20.0, 6.0, 1000.0);
            let h_r = footer_height_px(Some(&v_running), 20.0, 6.0, 1000.0);
            assert_eq!(h_c, h_r);
        }
    }
}
