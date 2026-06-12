//! footer 输入区视图数据结构（M4.1 批C）——设计稿 §7.1。
//!
//! [`ComposerView`] 是 app 层组装、renderer 只读消费的纯数据结构；
//! renderer 不依赖 lumen-editor，数据流方向为：
//! `lumen-app（组装）→ render(Option<&ComposerView>)→ lumen-renderer（绘制）`。
//!
//! # feature 门控
//! 本模块整体受 `input-editor` feature 门控；flag 剔除时编译单元不包含
//! 此模块，renderer 接口退化到无 footer 行为，与现状逐字节一致。
//!
//! # 设计稿对应章节
//! 设计稿 §2「输入模式机（四态）」、§7.1「footer 输入区（零新 GPU 管线）」。

/// footer 的显示形态，由上层 app 按当前 [`InputMode`] 组装。
///
/// [`InputMode`]: crate::InputMode（app 侧 mode.rs）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FooterKind {
    /// Compose 态：完整编辑卡片（文本 + 光标 + 选区）。
    Composer,
    /// Running 态：坍缩为等高状态条（文案由 app 传入，三语 i18n）。
    StatusBar,
    /// AltScreen / Fallback：整体隐藏，grid 收回全高。
    Hidden,
}

/// app 组装、renderer 消费的 footer 只读视图（M4.1 批C/D2）。
///
/// renderer 仅读本结构，不感知 lumen-editor 内部状态。
///
/// # 字段
/// - `kind`：形态选择（Composer / StatusBar / Hidden）。
/// - `lines`：文本行内容（Compose 态为编辑内容，Running 态为 `["状态文案"]`）。
/// - `cursor`：光标位置 `(行索引, 字节偏移)`，仅 Compose 态有意义。
/// - `label`：模式提示标签（如 "Compose" / "直通中" / 空串）。
/// - `preedit`：IME 预编辑文本（Compose 态；None 表示无预编辑）。M4.1 批D2。
/// - `exit_badge`：退出码角标（None 表示无需显示）。M4.1 批D2。
#[derive(Debug, Clone)]
pub struct ComposerView {
    /// footer 形态。
    pub kind: FooterKind,
    /// 文本行（Compose 态=编辑内容行；Running 态=单行状态文案；Hidden 态=空）。
    pub lines: Vec<String>,
    /// 光标 `(行索引, 字节偏移)`，仅 Compose 态绘制竖条光标时使用。
    pub cursor: (usize, usize),
    /// 模式提示标签（状态条右侧角落，可为空串）。
    pub label: String,
    /// IME 预编辑文本（M4.1 批D2）：Some 时在光标处内嵌绘制（下划线样式）。
    /// 不进文档模型、不参与 undo；Ime::Commit 时清空。
    pub preedit: Option<PreeditState>,
    /// 退出码角标（M4.1 批D2）：None 表示不显示；任意按键后由 app 层清空。
    pub exit_badge: Option<ExitBadge>,
}

/// IME 预编辑状态（M4.1 批D2，设计稿 §7.3）。
#[derive(Debug, Clone, Default)]
pub struct PreeditState {
    /// 预编辑文本内容。
    pub text: String,
    /// 预编辑活动区间（字节偏移，[start, end)），None 表示整段均激活。
    pub cursor_range: Option<(usize, usize)>,
}

/// 退出码角标（M4.1 批D2，设计稿 §3.2 第⑥步）。
///
/// 任意按键后由 app 层清空（dispatcher 收到任意键盘事件时 clear）。
#[derive(Debug, Clone)]
pub struct ExitBadge {
    /// 退出码（0 = 成功绿色 ✓，非零 = 失败红色 ✗）。
    pub exit_code: i32,
    /// 命令耗时（毫秒），用于显示耗时文本。
    pub duration_ms: u64,
}

impl ComposerView {
    /// 构造 Compose 态视图（空编辑器）。
    ///
    /// 本批（批C）内容在 app 侧尚未接管输入，传入空行+光标在原点。
    ///
    /// # Arguments
    /// * `label` - 模式提示标签（通常为 "Compose"）。
    pub fn compose_empty(label: impl Into<String>) -> Self {
        Self {
            kind: FooterKind::Composer,
            lines: vec![String::new()],
            cursor: (0, 0),
            label: label.into(),
            preedit: None,
            exit_badge: None,
        }
    }

    /// 构造 Running 态视图（等高状态条）。
    ///
    /// # Arguments
    /// * `status_text` - 状态条文案（三语 i18n 由 app 侧传入）。
    /// * `label` - 模式提示标签（通常为 "Running"）。
    pub fn running(status_text: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            kind: FooterKind::StatusBar,
            lines: vec![status_text.into()],
            cursor: (0, 0),
            label: label.into(),
            preedit: None,
            exit_badge: None,
        }
    }

    /// 构造隐藏态视图（AltScreen / Fallback，grid 收回全高）。
    pub fn hidden() -> Self {
        Self {
            kind: FooterKind::Hidden,
            lines: Vec::new(),
            cursor: (0, 0),
            label: String::new(),
            preedit: None,
            exit_badge: None,
        }
    }

    /// footer 文本行数（用于高度计算，隐藏态返回 0）。
    pub fn line_count(&self) -> usize {
        match self.kind {
            FooterKind::Hidden => 0,
            _ => self.lines.len().max(1),
        }
    }

    /// 是否需要绘制 footer（Hidden 态不绘制）。
    pub fn is_visible(&self) -> bool {
        self.kind != FooterKind::Hidden
    }
}

/// 计算 footer 占用的物理像素高度。
///
/// 规则（设计稿 §7.1 防 resize 风暴）：
/// - 隐藏态（AltScreen / Fallback）返回 `0`，grid 收回全高。
/// - 可见态：`line_count × cell_h + 2 × footer_padding`（至少 1 行常驻等高）。
/// - 多行增高上限：不超过 `max_height_px`（1/3 窗格高，超出内部滚动）。
///
/// # Arguments
/// * `view` - footer 视图（None 等价于 Hidden，返回 0）。
/// * `cell_h` - 单元格物理像素高度。
/// * `footer_padding` - footer 上下各加的内边距（物理像素）。
/// * `max_height_px` - 高度上限（1/3 窗格高，物理像素）。
///
/// # Examples
/// ```
/// use lumen_renderer::composer_view::{ComposerView, footer_height_px};
///
/// let view = ComposerView::compose_empty("Compose");
/// let h = footer_height_px(Some(&view), 20.0, 6.0, 200.0);
/// assert_eq!(h, 20.0 + 6.0 * 2.0); // 1 行 + 内边距
/// ```
pub fn footer_height_px(
    view: Option<&ComposerView>,
    cell_h: f32,
    footer_padding: f32,
    max_height_px: f32,
) -> f32 {
    let Some(v) = view else { return 0.0 };
    if !v.is_visible() {
        return 0.0;
    }
    let line_count = v.line_count().max(1) as f32;
    let raw = line_count * cell_h + footer_padding * 2.0;
    raw.min(max_height_px)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── FooterKind / ComposerView 组装 ───────────────────────────────

    #[test]
    fn compose_empty_形态正确() {
        let v = ComposerView::compose_empty("Compose");
        assert_eq!(v.kind, FooterKind::Composer);
        assert_eq!(v.line_count(), 1, "Compose 空编辑器应有 1 行");
        assert!(v.is_visible());
    }

    #[test]
    fn running_形态正确() {
        let v = ComposerView::running("运行中", "Running");
        assert_eq!(v.kind, FooterKind::StatusBar);
        assert_eq!(v.line_count(), 1);
        assert!(v.is_visible());
    }

    #[test]
    fn hidden_形态正确() {
        let v = ComposerView::hidden();
        assert_eq!(v.kind, FooterKind::Hidden);
        assert_eq!(v.line_count(), 0, "Hidden 态 line_count 应为 0");
        assert!(!v.is_visible());
    }

    // ── footer_height_px 纯函数 ──────────────────────────────────────

    /// 1 行常驻：高度 = cell_h + padding*2。
    #[test]
    fn 单行等高_无上限() {
        let v = ComposerView::compose_empty("Compose");
        let h = footer_height_px(Some(&v), 20.0, 6.0, 1000.0);
        assert_eq!(h, 20.0 + 12.0, "1 行：cell_h + padding*2");
    }

    /// 多行增高：3 行 × cell_h + padding*2。
    #[test]
    fn 三行增高() {
        let mut v = ComposerView::compose_empty("Compose");
        v.lines = vec!["line1".to_owned(), "line2".to_owned(), "line3".to_owned()];
        let h = footer_height_px(Some(&v), 20.0, 6.0, 1000.0);
        assert_eq!(h, 3.0 * 20.0 + 12.0, "3 行：3*cell_h + padding*2");
    }

    /// 1/3 窗高上限钳制。
    #[test]
    fn 超高被钳制() {
        let mut v = ComposerView::compose_empty("Compose");
        // 20 行高度 = 20*20+12 = 412，但上限为 100。
        v.lines = (0..20).map(|i| format!("line{i}")).collect();
        let h = footer_height_px(Some(&v), 20.0, 6.0, 100.0);
        assert_eq!(h, 100.0, "超出上限应被钳制");
    }

    /// Hidden 态高度为 0。
    #[test]
    fn 隐藏态高度为零() {
        let v = ComposerView::hidden();
        let h = footer_height_px(Some(&v), 20.0, 6.0, 1000.0);
        assert_eq!(h, 0.0, "Hidden 态 footer 高度应为 0");
    }

    /// None 等价于 Hidden，高度为 0。
    #[test]
    fn none_等价隐藏() {
        let h = footer_height_px(None, 20.0, 6.0, 1000.0);
        assert_eq!(h, 0.0, "None 应返回 0");
    }

    /// Running 态与 Compose 等高（同为 1 行，防 Compose↔Running 切换引发 resize）。
    #[test]
    fn running_与_compose_等高() {
        let compose = ComposerView::compose_empty("Compose");
        let running = ComposerView::running("运行中", "Running");
        let h_compose = footer_height_px(Some(&compose), 20.0, 6.0, 1000.0);
        let h_running = footer_height_px(Some(&running), 20.0, 6.0, 1000.0);
        assert_eq!(
            h_compose, h_running,
            "Compose↔Running 等高，切换不触发 resize"
        );
    }
}
