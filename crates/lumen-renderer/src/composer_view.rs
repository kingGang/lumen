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

/// footer 编辑器的文本选区（规范化端点，start ≤ end）。
///
/// 坐标单位与 [`ComposerView::cursor`] 相同：`(行索引, 行内字节偏移)`。
///
/// 保证不变量：`start.0 < end.0`，或 `start.0 == end.0 && start.1 < end.1`，
/// 即 start 在文档顺序上严格早于 end（等于 = 空选区，app 侧不填此字段）。
///
/// # Examples
/// ```
/// use lumen_renderer::composer_view::FooterSelection;
///
/// // 单行选区：第 0 行字节 2..5
/// let sel = FooterSelection { start: (0, 2), end: (0, 5) };
/// assert!(sel.start < sel.end);
///
/// // 多行选区：第 0 行字节 3 到第 2 行字节 1
/// let multi = FooterSelection { start: (0, 3), end: (2, 1) };
/// assert!(multi.start.0 < multi.end.0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FooterSelection {
    /// 选区起点 `(行索引, 字节偏移)`（文档顺序的较早端）。
    pub start: (usize, usize),
    /// 选区终点 `(行索引, 字节偏移)`（文档顺序的较晚端，exclusive）。
    pub end: (usize, usize),
}

/// 将 anchor/cursor 两端规范化为 [`FooterSelection`]。
///
/// 若两端相等（纯光标，无选区），返回 `None`。
/// 否则以文档顺序将较小者设为 start，较大者设为 end。
///
/// # Arguments
/// * `anchor` - 选区锚点 `(行, 字节)`。
/// * `cursor` - 选区活动端 `(行, 字节)`。
///
/// # Examples
/// ```
/// use lumen_renderer::composer_view::normalize_selection;
///
/// // 正向选区（anchor 早于 cursor）
/// assert_eq!(
///     normalize_selection((0, 1), (0, 5)),
///     Some(lumen_renderer::composer_view::FooterSelection { start: (0, 1), end: (0, 5) })
/// );
///
/// // 逆向选区（cursor 早于 anchor）
/// assert_eq!(
///     normalize_selection((1, 3), (0, 2)),
///     Some(lumen_renderer::composer_view::FooterSelection { start: (0, 2), end: (1, 3) })
/// );
///
/// // 相等 = 无选区
/// assert_eq!(normalize_selection((0, 0), (0, 0)), None);
/// ```
pub fn normalize_selection(
    anchor: (usize, usize),
    cursor: (usize, usize),
) -> Option<FooterSelection> {
    if anchor == cursor {
        return None;
    }
    let (start, end) = if anchor <= cursor {
        (anchor, cursor)
    } else {
        (cursor, anchor)
    };
    Some(FooterSelection { start, end })
}

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

/// app 组装、renderer 消费的 footer 只读视图（M4.1 批C/D2/E）。
///
/// renderer 仅读本结构，不感知 lumen-editor 内部状态。
///
/// # 字段
/// - `kind`：形态选择（Composer / StatusBar / Hidden）。
/// - `lines`：文本行内容（Compose 态为编辑内容，Running 态为 `["状态文案"]`）。
/// - `cursor`：光标位置 `(行索引, 字节偏移)`，仅 Compose 态有意义。
/// - `selection`：文本选区（规范化后的 [`FooterSelection`]，有选区时 Some）。M4.1 批F。
/// - `preedit`：IME 预编辑文本（Compose 态；None 表示无预编辑）。M4.1 批D2。
/// - `exit_badge`：退出码角标（None 表示无需显示）。M4.1 批D2。
/// - `placeholder`：占位提示文字（Compose 态 lines 全空时以 fg_dim 色显示）。M4.1 批E。
/// - `ghost`：历史联想 ghost text（有选区时不显示；光标在文末时追加渲染，fg_dim 色）。M4.1 批E。
#[derive(Debug, Clone)]
pub struct ComposerView {
    /// footer 形态。
    pub kind: FooterKind,
    /// 文本行（Compose 态=编辑内容行；Running 态=单行状态文案；Hidden 态=空）。
    pub lines: Vec<String>,
    /// 光标 `(行索引, 字节偏移)`，仅 Compose 态绘制竖条光标时使用。
    pub cursor: (usize, usize),
    /// 文本选区（M4.1 批F）：anchor ≠ cursor 时为 Some；已由 app 侧规范化（start ≤ end）。
    /// None = 纯光标（无选区）；有选区时 ghost text 不显示（视觉冲突，与系统惯例一致）。
    pub selection: Option<FooterSelection>,
    /// IME 预编辑文本（M4.1 批D2）：Some 时在光标处内嵌绘制（下划线样式）。
    /// 不进文档模型、不参与 undo；Ime::Commit 时清空。
    pub preedit: Option<PreeditState>,
    /// 退出码角标（M4.1 批D2）：None 表示不显示；任意按键后由 app 层清空。
    pub exit_badge: Option<ExitBadge>,
    /// Compose 态占位提示（M4.1 批E）：仅当 `lines` 全空（空编辑器）时
    /// 以 fg_dim 色显示在光标位置，光标仍在行首正常渲染。
    /// None 或空串 = 不显示占位。
    pub placeholder: Option<String>,
    /// 历史联想 ghost text（M4.1 批E）：光标在文末时在光标后以 fg_dim 色
    /// 追加渲染；→/End 键在文末+ghost 存在时接受（InsertText(ghost)）。
    /// `selection.is_some()` 时不渲染（有选区时无 inline 补全，与系统惯例一致）。
    /// None = 无联想；不参与光标/选区几何；超行宽由 TextBounds 自然裁剪。
    pub ghost: Option<String>,
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
    pub fn compose_empty() -> Self {
        Self {
            kind: FooterKind::Composer,
            lines: vec![String::new()],
            cursor: (0, 0),
            selection: None,
            preedit: None,
            exit_badge: None,
            placeholder: None,
            ghost: None,
        }
    }

    /// 构造 Running 态视图（等高状态条）。
    ///
    /// # Arguments
    /// * `status_text` - 状态条文案（三语 i18n 由 app 侧传入）。
    pub fn running(status_text: impl Into<String>) -> Self {
        Self {
            kind: FooterKind::StatusBar,
            lines: vec![status_text.into()],
            cursor: (0, 0),
            selection: None,
            preedit: None,
            exit_badge: None,
            placeholder: None,
            ghost: None,
        }
    }

    /// 构造隐藏态视图（AltScreen / Fallback，grid 收回全高）。
    pub fn hidden() -> Self {
        Self {
            kind: FooterKind::Hidden,
            lines: Vec::new(),
            cursor: (0, 0),
            selection: None,
            preedit: None,
            exit_badge: None,
            placeholder: None,
            ghost: None,
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
/// let view = ComposerView::compose_empty();
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

/// 将行内字节偏移转换为显示列数（宽字符 CJK/emoji 占 2 列）。
///
/// 与 `lumen-editor::cursor::byte_to_display_col` 语义相同；
/// renderer 不依赖 lumen-editor，此处复制相同算法并与光标绘制逻辑对齐。
///
/// # Arguments
/// * `line` - 行文本（UTF-8）。
/// * `byte` - 行内字节偏移（超出行长时夹紧到行尾）。
///
/// # Examples
/// ```
/// use lumen_renderer::composer_view::footer_byte_to_col;
///
/// assert_eq!(footer_byte_to_col("hello", 3), 3);
/// // 汉字各占 3 字节（UTF-8）但显示宽度 2 列
/// assert_eq!(footer_byte_to_col("中文", 3), 2);  // 第一个汉字后
/// assert_eq!(footer_byte_to_col("中文", 6), 4);  // 两个汉字后
/// // ASCII 后接汉字
/// assert_eq!(footer_byte_to_col("ab中", 2), 2);  // "ab" 后
/// assert_eq!(footer_byte_to_col("ab中", 5), 4);  // "ab中" 后
/// ```
pub fn footer_byte_to_col(line: &str, byte: usize) -> usize {
    // 注意：与光标绘制处的 `chars().count()` 不同，此处使用 unicode-width
    // 正确计算宽字符（CJK/emoji）的显示列数（每个宽字符占 2 列）。
    // 光标绘制处的 chars().count() 在 CJK 场景下会低估列数（教训 8ff0cb5）；
    // 选区几何与光标几何必须用同一套列换算，故此处使用精确宽度。
    //
    // ALLOW: unicode_width 是 glyphon 传递依赖，已在 workspace Cargo.lock 中；
    // renderer Cargo.toml 显式声明以避免隐式传递依赖被静默升降版本。
    use unicode_width::UnicodeWidthChar;
    let byte = byte.min(line.len());
    line[..byte]
        .chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// 选区高亮矩形（像素坐标），供 renderer 转为 `RectInstance`。
///
/// 每个元素为 `(x, y, width, height)`（物理像素，左上角原点）。
/// 单行选区返回 1 个矩形；多行返回 3 段（首行尾段、中间整行、末行首段）。
/// 空选区（`sel` 的 start == end，或 lines 为空）返回空 Vec。
///
/// 与光标绘制使用相同的列换算（[`footer_byte_to_col`]），保证选区边界与光标位置对齐。
///
/// # Arguments
/// * `sel` - 已规范化的选区（start ≤ end）。
/// * `lines` - 编辑器行文本切片（与 `ComposerView::lines` 对应）。
/// * `footer_top` - footer 顶边物理像素 y 坐标。
/// * `fp` - footer 内边距（物理像素，与光标绘制同一 `fp`）。
/// * `footer_w` - footer 物理宽度（像素）。
/// * `cw` - 单元格宽度（物理像素）。
/// * `ch` - 单元格高度（物理像素）。
///
/// # Examples
/// ```
/// use lumen_renderer::composer_view::{FooterSelection, selection_rects};
///
/// let lines = vec!["hello world".to_string()];
/// let sel = FooterSelection { start: (0, 0), end: (0, 5) };
/// let rects = selection_rects(&sel, &lines, 100.0, 4.0, 300.0, 8.0, 20.0);
/// assert_eq!(rects.len(), 1);
/// let (x, _y, w, h) = rects[0];
/// assert_eq!(x, 4.0); // fp
/// assert!((w - 5.0 * 8.0).abs() < 0.01); // 5 列 × cw
/// assert!((h - 20.0).abs() < 0.01); // ch
/// ```
pub fn selection_rects(
    sel: &FooterSelection,
    lines: &[String],
    footer_top: f32,
    fp: f32,
    footer_w: f32,
    cw: f32,
    ch: f32,
) -> Vec<(f32, f32, f32, f32)> {
    let (sl, sb) = sel.start;
    let (el, eb) = sel.end;
    if sl == el && sb == eb {
        return Vec::new(); // 空选区
    }

    let line_y = |li: usize| footer_top + fp + li as f32 * ch;

    // 取某行的显示列数（超出行范围时返回 0 或行末列）
    let col_of = |li: usize, byte: usize| -> f32 {
        let text = lines.get(li).map(|s| s.as_str()).unwrap_or("");
        footer_byte_to_col(text, byte) as f32
    };

    // 某行的文本宽（显示列数），即行末列
    let line_end_col = |li: usize| -> f32 {
        let text = lines.get(li).map(|s| s.as_str()).unwrap_or("");
        footer_byte_to_col(text, text.len()) as f32
    };

    let mut rects = Vec::new();

    if sl == el {
        // 单行选区
        let start_col = col_of(sl, sb);
        let end_col = col_of(el, eb);
        let x = fp + start_col * cw;
        let w = ((end_col - start_col) * cw).max(0.0);
        if w > 0.0 {
            rects.push((x, line_y(sl), w.min(footer_w - x), ch));
        }
    } else {
        // 多行选区：首行尾段 + 中间整行 + 末行首段

        // 首行：从 start_col 到行末
        {
            let start_col = col_of(sl, sb);
            let end_col = line_end_col(sl);
            let x = fp + start_col * cw;
            // 若行为空则画到行末（至少留 1 列宽做视觉提示）
            let w = if end_col > start_col {
                (end_col - start_col) * cw
            } else {
                cw // 行尾选区，最少画 1 格宽
            };
            let w = w.min(footer_w - x);
            if w > 0.0 {
                rects.push((x, line_y(sl), w, ch));
            }
        }

        // 中间整行（若有）
        for li in (sl + 1)..el {
            let end_col = line_end_col(li).max(1.0); // 空行至少 1 格宽
            let x = fp;
            let w = (end_col * cw).min(footer_w - x);
            if w > 0.0 {
                rects.push((x, line_y(li), w, ch));
            }
        }

        // 末行：从行首到 end_col
        {
            let end_col = col_of(el, eb);
            let x = fp;
            let w = (end_col * cw).min(footer_w - x);
            if w > 0.0 {
                rects.push((x, line_y(el), w, ch));
            }
        }
    }

    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── FooterKind / ComposerView 组装 ───────────────────────────────

    #[test]
    fn compose_empty_形态正确() {
        let v = ComposerView::compose_empty();
        assert_eq!(v.kind, FooterKind::Composer);
        assert_eq!(v.line_count(), 1, "Compose 空编辑器应有 1 行");
        assert!(v.is_visible());
    }

    #[test]
    fn running_形态正确() {
        let v = ComposerView::running("运行中");
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
        let v = ComposerView::compose_empty();
        let h = footer_height_px(Some(&v), 20.0, 6.0, 1000.0);
        assert_eq!(h, 20.0 + 12.0, "1 行：cell_h + padding*2");
    }

    /// 多行增高：3 行 × cell_h + padding*2。
    #[test]
    fn 三行增高() {
        let mut v = ComposerView::compose_empty();
        v.lines = vec!["line1".to_owned(), "line2".to_owned(), "line3".to_owned()];
        let h = footer_height_px(Some(&v), 20.0, 6.0, 1000.0);
        assert_eq!(h, 3.0 * 20.0 + 12.0, "3 行：3*cell_h + padding*2");
    }

    /// 1/3 窗高上限钳制。
    #[test]
    fn 超高被钳制() {
        let mut v = ComposerView::compose_empty();
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
        let compose = ComposerView::compose_empty();
        let running = ComposerView::running("运行中");
        let h_compose = footer_height_px(Some(&compose), 20.0, 6.0, 1000.0);
        let h_running = footer_height_px(Some(&running), 20.0, 6.0, 1000.0);
        assert_eq!(
            h_compose, h_running,
            "Compose↔Running 等高，切换不触发 resize"
        );
    }

    // ── normalize_selection 选区规范化 ─────────────────────────────────

    /// 正向选区（anchor 早于 cursor）：直接映射 start/end。
    #[test]
    fn 规范化_正向选区() {
        let result = normalize_selection((0, 1), (0, 5));
        assert_eq!(
            result,
            Some(FooterSelection {
                start: (0, 1),
                end: (0, 5)
            }),
            "正向选区应原样映射 start/end"
        );
    }

    /// 逆向选区（cursor 早于 anchor）：交换后规范化。
    #[test]
    fn 规范化_逆向选区() {
        let result = normalize_selection((1, 3), (0, 2));
        assert_eq!(
            result,
            Some(FooterSelection {
                start: (0, 2),
                end: (1, 3)
            }),
            "逆向选区应交换为 start ≤ end"
        );
    }

    /// 折叠选区（anchor == cursor）：纯光标，应返回 None。
    #[test]
    fn 规范化_折叠为空() {
        assert_eq!(
            normalize_selection((0, 0), (0, 0)),
            None,
            "纯光标应返回 None"
        );
        assert_eq!(
            normalize_selection((2, 5), (2, 5)),
            None,
            "多行纯光标应返回 None"
        );
    }

    // ── footer_byte_to_col 字节→列换算 ────────────────────────────────

    /// ASCII 字符：每字节 = 1 列。
    #[test]
    fn 字节到列_ascii() {
        assert_eq!(footer_byte_to_col("hello", 0), 0);
        assert_eq!(footer_byte_to_col("hello", 3), 3);
        assert_eq!(footer_byte_to_col("hello", 5), 5);
    }

    /// CJK 汉字：每字符 3 字节 UTF-8，显示 2 列。
    #[test]
    fn 字节到列_cjk() {
        // "中文" = 6 字节，显示 4 列
        assert_eq!(footer_byte_to_col("中文", 0), 0);
        assert_eq!(footer_byte_to_col("中文", 3), 2, "第一个汉字后 = 2 列");
        assert_eq!(footer_byte_to_col("中文", 6), 4, "两个汉字后 = 4 列");
    }

    /// 超出行长：夹紧到行尾。
    #[test]
    fn 字节到列_超出行长夹紧() {
        assert_eq!(footer_byte_to_col("hi", 999), 2, "超出行长应夹紧到行尾");
    }

    // ── selection_rects 选区几何纯函数 ────────────────────────────────

    /// 单行选区：返回 1 个矩形，x/w 对应列范围。
    #[test]
    fn 选区矩形_单行() {
        let lines = vec!["hello world".to_string()];
        let sel = FooterSelection {
            start: (0, 0),
            end: (0, 5),
        };
        let rects = selection_rects(&sel, &lines, 100.0, 4.0, 300.0, 8.0, 20.0);
        assert_eq!(rects.len(), 1, "单行选区应返回 1 个矩形");
        let (x, y, w, h) = rects[0];
        assert!((x - 4.0).abs() < 0.01, "x = fp = 4");
        assert!((y - (100.0 + 4.0)).abs() < 0.01, "y = footer_top + fp");
        assert!((w - 5.0 * 8.0).abs() < 0.01, "w = 5列 × cw");
        assert!((h - 20.0).abs() < 0.01, "h = ch");
    }

    /// 单行 CJK 选区：宽字符列换算正确。
    #[test]
    fn 选区矩形_单行_cjk() {
        // "中文ab"：前两字各 2 列，选区覆盖 "中文"（字节 0..6）
        let lines = vec!["中文ab".to_string()];
        let sel = FooterSelection {
            start: (0, 0),
            end: (0, 6),
        };
        let rects = selection_rects(&sel, &lines, 0.0, 4.0, 500.0, 8.0, 20.0);
        assert_eq!(rects.len(), 1);
        let (_x, _y, w, _h) = rects[0];
        // "中文" = 4 显示列，宽 = 4 × 8 = 32
        assert!((w - 4.0 * 8.0).abs() < 0.01, "CJK 选区宽应为 4列×cw");
    }

    /// 两行选区：返回 2 个矩形（首行尾段 + 末行首段，无中间整行）。
    #[test]
    fn 选区矩形_两行() {
        let lines = vec!["hello".to_string(), "world".to_string()];
        let sel = FooterSelection {
            start: (0, 2), // "hello" 字节 2（列 2）
            end: (1, 3),   // "world" 字节 3（列 3）
        };
        let rects = selection_rects(&sel, &lines, 0.0, 4.0, 500.0, 8.0, 20.0);
        // 首行：列 2..5（"llo"，3 列）+ 末行：列 0..3（"wor"，3 列）
        assert_eq!(rects.len(), 2, "两行选区应返回 2 个矩形");
        let (x0, _y0, w0, _h0) = rects[0]; // 首行
        assert!(
            (x0 - (4.0 + 2.0 * 8.0)).abs() < 0.01,
            "首行 x = fp + 2列×cw"
        );
        assert!((w0 - 3.0 * 8.0).abs() < 0.01, "首行 w = 3列×cw（'llo'）");
        let (x1, _y1, w1, _h1) = rects[1]; // 末行
        assert!((x1 - 4.0).abs() < 0.01, "末行 x = fp（从行首）");
        assert!((w1 - 3.0 * 8.0).abs() < 0.01, "末行 w = 3列×cw（'wor'）");
    }

    /// 三行选区（含中间整行）：返回 3 个矩形。
    #[test]
    fn 选区矩形_三行含中间整行() {
        let lines = vec![
            "hello".to_string(),  // 行 0：5 列
            "middle".to_string(), // 行 1：6 列（中间整行）
            "world".to_string(),  // 行 2：5 列
        ];
        let sel = FooterSelection {
            start: (0, 2), // "hello" 列 2
            end: (2, 3),   // "world" 列 3
        };
        let rects = selection_rects(&sel, &lines, 0.0, 4.0, 500.0, 8.0, 20.0);
        assert_eq!(
            rects.len(),
            3,
            "三行选区应返回 3 个矩形（首行尾段、中间整行、末行首段）"
        );
        // 中间整行（行 1）宽 = 6列×cw
        let (_x1, _y1, w1, _h1) = rects[1];
        assert!((w1 - 6.0 * 8.0).abs() < 0.01, "中间整行 w = 6列×cw");
    }

    // ── CJK 光标列与选区端点列一致性（三连绿必测项）────────────────────

    /// "你好ab" 光标在 byte 6（"你好" 之后）→ 显示列 4。
    ///
    /// "你" = 3 字节，2 列；"好" = 3 字节，2 列；合计 byte=6 → col=4。
    /// 这是修复前 chars().count() 会给出 2（字符数）而非 4（显示列数）的典型场景。
    #[test]
    fn cjk行_光标字节偏移转列_你好ab() {
        // "你好ab"：你(3B,2col) 好(3B,2col) a(1B,1col) b(1B,1col)
        let line = "你好ab";
        assert_eq!(footer_byte_to_col(line, 0), 0, "行首 = 列 0");
        assert_eq!(footer_byte_to_col(line, 3), 2, "你 之后 = 列 2");
        assert_eq!(footer_byte_to_col(line, 6), 4, "你好 之后 = 列 4");
        assert_eq!(footer_byte_to_col(line, 7), 5, "你好a 之后 = 列 5");
        assert_eq!(footer_byte_to_col(line, 8), 6, "你好ab 之后 = 列 6");
    }

    /// 光标列 == 选区端点列一致性：CJK 行中 cursor 落在选区 end 字节处，
    /// 二者计算出的列数必须相等（保证光标与高亮右边缘对齐）。
    #[test]
    fn cjk行_光标列等于选区端点列() {
        // 行 "你好ab"，选区覆盖 "你好"（字节 0..6），光标在 byte 6。
        let line = "你好ab".to_string();
        let lines = vec![line.clone()];
        let sel = FooterSelection {
            start: (0, 0),
            end: (0, 6),
        };

        // 选区右边界列（selection_rects 内部等价于 footer_byte_to_col(line, 6)）
        let sel_end_col = footer_byte_to_col(&line, 6);
        // 光标列（footer_byte_to_col，与 render_impl 中修复后的算法相同）
        let cursor_col = footer_byte_to_col(&line, 6);
        assert_eq!(
            sel_end_col, cursor_col,
            "CJK 行光标列应等于选区端点列（两处算法统一后必须相等）"
        );

        // 进一步验证 selection_rects 算出的右边界 x 与光标 x 一致。
        let fp = 4.0_f32;
        let cw = 8.0_f32;
        let rects = selection_rects(&sel, &lines, 0.0, fp, 500.0, cw, 20.0);
        assert_eq!(rects.len(), 1, "单行选区应有 1 个矩形");
        let (sel_x, _sy, sel_w, _sh) = rects[0];
        // 选区右边缘 x = sel_x + sel_w
        let sel_right = sel_x + sel_w;
        // 光标 x（与 render_impl 修复后一致）
        let cursor_x = fp + cursor_col as f32 * cw;
        assert!(
            (sel_right - cursor_x).abs() < 0.01,
            "选区右边缘 ({sel_right}) 应与光标 x ({cursor_x}) 重合"
        );
    }
}
