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

use crate::i18n;
use crate::mode::InputMode;
use lumen_renderer::composer_view::ComposerView;

#[cfg(feature = "input-editor")]
use lumen_editor::EditorView;

/// 按当前有效输入模式组装 [`ComposerView`]。
///
/// - [`InputMode::Compose`] → 完整编辑卡片，内容来自 `editor_view`（真实 EditorView 内容）
/// - [`InputMode::Running`] → 等高状态条（文案 i18n）
/// - [`InputMode::AltScreen`] → 隐藏（grid 收回全高）
/// - [`InputMode::Fallback`] → 等高状态条（"shell 集成未生效"，批D1 真实化）
///
/// # 设计稿铁律
/// Compose ↔ Running 切换返回等高视图（均为 1 行），不改 footer 高度，
/// 不触发 `term.resize` / `pty.resize`（防 resize 风暴，设计稿 §7.1）。
/// Fallback 亦等高（隐藏改为状态条，确保与 Running 等高）。
///
/// # Arguments
/// * `mode` - 当前有效输入模式（由 [`crate::mode::effective_mode`] 推导）。
/// * `editor_view` - Compose 态时的编辑器只读视图（仅 Compose 态读取内容）。
#[cfg(feature = "input-editor")]
pub fn compose_view_for_mode(mode: InputMode, editor_view: EditorView<'_>) -> ComposerView {
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
            // Position { line, byte }——与 ComposerView.cursor (行, 字节偏移) 语义相同
            ComposerView {
                kind: lumen_renderer::composer_view::FooterKind::Composer,
                lines,
                cursor: (cur.line, cur.byte),
                label: s.footer_label_compose.to_owned(),
            }
        }
        InputMode::Running => ComposerView::running(s.footer_running_text, s.footer_label_running),
        // AltScreen：全屏 TUI 让位，footer 隐藏，grid 收回全高。
        InputMode::AltScreen => ComposerView::hidden(),
        // Fallback：shell integration 未生效，显示等高状态条说明（批D1 真实化）。
        InputMode::Fallback => {
            ComposerView::running(s.footer_fallback_text, s.footer_label_running)
        }
    }
}

/// 无 input-editor feature 时的退化版本（保持原批C 兼容行为）。
#[cfg(not(feature = "input-editor"))]
pub fn compose_view_for_mode(mode: InputMode) -> ComposerView {
    let s = i18n::strings();
    match mode {
        InputMode::Compose => ComposerView::compose_empty(s.footer_label_compose),
        InputMode::Running => ComposerView::running(s.footer_running_text, s.footer_label_running),
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
            let v = compose_view_for_mode(InputMode::Compose, editor.view());
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
            let v = compose_view_for_mode(InputMode::Compose, editor.view());
            assert_eq!(v.lines.len(), 1, "空编辑器应有 1 行");
            assert_eq!(v.cursor, (0, 0), "空编辑器光标在原点");
        }

        #[test]
        fn running_模式_产出_statusbar_形态() {
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::Running, editor.view());
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
            let v = compose_view_for_mode(InputMode::AltScreen, editor.view());
            assert_eq!(
                v.kind,
                FooterKind::Hidden,
                "AltScreen 模式应产出 Hidden 形态"
            );
            assert!(!v.is_visible(), "AltScreen 形态应隐藏");
        }

        #[test]
        fn fallback_模式_产出_statusbar_形态() {
            // 批D1：Fallback 不再隐藏，改为等高状态条显示"shell 集成未生效"
            let editor = empty_view();
            let v = compose_view_for_mode(InputMode::Fallback, editor.view());
            assert_eq!(
                v.kind,
                FooterKind::StatusBar,
                "Fallback 模式应产出 StatusBar 形态（显示说明文案）"
            );
            assert!(v.is_visible(), "Fallback 形态应可见（等高状态条）");
        }

        /// Compose ↔ Running 等高铁律：两者均为 1 行，footer_height_px 返回相同值。
        #[test]
        fn compose_与_running_等高() {
            use lumen_renderer::composer_view::footer_height_px;
            let editor = empty_view();
            let v_compose = compose_view_for_mode(InputMode::Compose, editor.view());
            let v_running = compose_view_for_mode(InputMode::Running, editor.view());
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
            let v_alt = compose_view_for_mode(InputMode::AltScreen, editor.view());
            assert_eq!(
                footer_height_px(Some(&v_alt), 20.0, 6.0, 1000.0),
                0.0,
                "AltScreen 高度应为 0"
            );
        }

        /// Fallback 等高状态条高度与 Running 一致（批D1 变更）。
        #[test]
        fn fallback_与_running_等高() {
            use lumen_renderer::composer_view::footer_height_px;
            let editor = empty_view();
            let v_fb = compose_view_for_mode(InputMode::Fallback, editor.view());
            let v_run = compose_view_for_mode(InputMode::Running, editor.view());
            let h_fb = footer_height_px(Some(&v_fb), 20.0, 6.0, 1000.0);
            let h_run = footer_height_px(Some(&v_run), 20.0, 6.0, 1000.0);
            assert_eq!(h_fb, h_run, "Fallback({h_fb}) 与 Running({h_run}) 应等高");
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
