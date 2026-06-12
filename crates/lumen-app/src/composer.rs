//! footer 输入区视图组装（M4.1 批C，feature = "input-editor"）——设计稿 §7.1。
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

/// 按当前有效输入模式组装 [`ComposerView`]。
///
/// - [`InputMode::Compose`] → 完整编辑卡片（本批 editor 尚未接管，传空行+原点光标）
/// - [`InputMode::Running`] → 等高状态条（文案 i18n）
/// - [`InputMode::AltScreen`] | [`InputMode::Fallback`] → 隐藏（grid 收回全高）
///
/// # 设计稿铁律
/// Compose ↔ Running 切换返回等高视图（均为 1 行），不改 footer 高度，
/// 不触发 `term.resize` / `pty.resize`（防 resize 风暴，设计稿 §7.1）。
///
/// # Arguments
/// * `mode` - 当前有效输入模式（由 [`crate::mode::effective_mode`] 推导）。
pub fn compose_view_for_mode(mode: InputMode) -> ComposerView {
    let s = i18n::strings();
    match mode {
        InputMode::Compose => {
            // 批C：editor 尚未接管输入，显示空卡片 + 模式提示。
            // 批D 接通 lumen-editor 后此处改为真实 EditorView 内容。
            ComposerView::compose_empty(s.footer_label_compose)
        }
        InputMode::Running => ComposerView::running(s.footer_running_text, s.footer_label_running),
        // AltScreen：全屏 TUI 让位，footer 隐藏，grid 收回全高。
        // Fallback：shell integration 未生效，footer 隐藏。
        InputMode::AltScreen | InputMode::Fallback => ComposerView::hidden(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_renderer::composer_view::FooterKind;

    // ── 四模式 → 形态映射 ────────────────────────────────────────────

    #[test]
    fn compose_模式_产出_composer_形态() {
        let v = compose_view_for_mode(InputMode::Compose);
        assert_eq!(
            v.kind,
            FooterKind::Composer,
            "Compose 模式应产出 Composer 形态"
        );
        assert!(v.is_visible(), "Compose 形态应可见");
    }

    #[test]
    fn running_模式_产出_statusbar_形态() {
        let v = compose_view_for_mode(InputMode::Running);
        assert_eq!(
            v.kind,
            FooterKind::StatusBar,
            "Running 模式应产出 StatusBar 形态"
        );
        assert!(v.is_visible(), "Running 形态应可见");
    }

    #[test]
    fn altscreen_模式_产出_hidden_形态() {
        let v = compose_view_for_mode(InputMode::AltScreen);
        assert_eq!(
            v.kind,
            FooterKind::Hidden,
            "AltScreen 模式应产出 Hidden 形态"
        );
        assert!(!v.is_visible(), "AltScreen 形态应隐藏");
    }

    #[test]
    fn fallback_模式_产出_hidden_形态() {
        let v = compose_view_for_mode(InputMode::Fallback);
        assert_eq!(
            v.kind,
            FooterKind::Hidden,
            "Fallback 模式应产出 Hidden 形态"
        );
        assert!(!v.is_visible(), "Fallback 形态应隐藏");
    }

    /// Compose ↔ Running 等高铁律：两者均为 1 行，footer_height_px 返回相同值。
    #[test]
    fn compose_与_running_等高() {
        use lumen_renderer::composer_view::footer_height_px;
        let v_compose = compose_view_for_mode(InputMode::Compose);
        let v_running = compose_view_for_mode(InputMode::Running);
        let h_c = footer_height_px(Some(&v_compose), 20.0, 6.0, 1000.0);
        let h_r = footer_height_px(Some(&v_running), 20.0, 6.0, 1000.0);
        assert_eq!(
            h_c, h_r,
            "Compose({h_c}) 与 Running({h_r}) 应等高，否则切换触发 resize"
        );
    }

    /// AltScreen / Fallback 隐藏态高度为 0（grid 收回全高）。
    #[test]
    fn altscreen_fallback_高度为零() {
        use lumen_renderer::composer_view::footer_height_px;
        let v_alt = compose_view_for_mode(InputMode::AltScreen);
        let v_fb = compose_view_for_mode(InputMode::Fallback);
        assert_eq!(
            footer_height_px(Some(&v_alt), 20.0, 6.0, 1000.0),
            0.0,
            "AltScreen 高度应为 0"
        );
        assert_eq!(
            footer_height_px(Some(&v_fb), 20.0, 6.0, 1000.0),
            0.0,
            "Fallback 高度应为 0"
        );
    }
}
