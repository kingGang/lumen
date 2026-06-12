//! 文件路径补全弹窗 UI（M4.4 批1）。
//!
//! 以 [`egui::Area`] 实现锚定小浮层（非全屏 Modal），
//! 锚点由 main.rs 传入（footer 上方合理位置）。
//! 键盘导航：↑↓/Tab/Shift+Tab 移动；Enter/Tab 接受；Esc 关闭。
//! 候选数据每帧由 main.rs 计算后通过 [`CompletionView`] 传入，本模块不缓存。

use super::theme::Palette;

/// 弹窗列表中一行的展示数据（由 main.rs 从 `Completion` 构造）。
pub struct CandidateRow {
    /// 列表显示名（目录末尾含 `/`）。
    pub display: String,
    /// 是否为目录（用于区分颜色）。
    pub is_dir: bool,
}

/// 一帧补全弹窗的输入视图（main.rs 每帧构造）。
pub struct CompletionView<'a> {
    /// 本帧的候选列表。
    pub candidates: &'a [CandidateRow],
    /// 弹窗左下锚点（egui 逻辑坐标；弹窗向上展开）。
    pub anchor: egui::Pos2,
}

/// 补全弹窗的跨帧 UI 状态。
#[derive(Default)]
pub struct CompletionUiState {
    /// 弹窗是否打开。
    pub open: bool,
    /// 当前高亮行下标（0 = 第一行）。
    pub selected: usize,
}

/// 一帧补全弹窗 UI 的产出。
#[derive(Default)]
pub struct CompletionOutput {
    /// 用户接受了某个候选（值为候选下标）。
    pub accept: Option<usize>,
    /// 弹窗应关闭（Esc 或外部触发）。
    pub closed: bool,
}

/// 弹窗最大显示行数（超出后列表滚动）。
const MAX_VISIBLE_ROWS: usize = 10;
/// 每行高度（逻辑像素）。
const ROW_HEIGHT: f32 = 26.0;
/// 弹窗水平内边距。
const HPAD: f32 = 10.0;
/// 弹窗垂直内边距。
const VPAD: f32 = 4.0;
/// 弹窗圆角半径。
const RADIUS: f32 = 6.0;
/// 弹窗固定宽度（逻辑像素）。
const POPUP_WIDTH: f32 = 320.0;

/// 绘制文件路径补全弹窗。调用方保证 `state.open == true` 时才调用。
///
/// # Arguments
/// * `ctx`   - egui 上下文。
/// * `state` - 跨帧 UI 状态（open / selected）。
/// * `view`  - 本帧的候选列表与锚点位置。
/// * `pal`   - 当前外壳色板。
///
/// # Returns
/// 本帧的 UI 产出（接受候选 / 关闭信号）。
pub fn show(
    ctx: &egui::Context,
    state: &mut CompletionUiState,
    view: &CompletionView<'_>,
    pal: &Palette,
) -> CompletionOutput {
    let mut out = CompletionOutput::default();
    let candidates = view.candidates;

    // selected 按 candidates.len() 钳制（候选可能减少）。
    if candidates.is_empty() {
        state.selected = 0;
    } else {
        state.selected = state.selected.min(candidates.len() - 1);
    }

    // ── 键盘事件（在 Area 之前读取，避免 Area 内部控件先消化）──────
    ctx.input(|i| {
        let n = candidates.len();
        if n == 0 {
            return;
        }

        // ↑ 或 Shift+Tab：向上移动（循环）。
        let up = i.key_pressed(egui::Key::ArrowUp)
            || (i.key_pressed(egui::Key::Tab) && i.modifiers.shift);
        // ↓ 或 Tab（无 Shift）：向下移动（循环）。
        let down = i.key_pressed(egui::Key::ArrowDown)
            || (i.key_pressed(egui::Key::Tab) && !i.modifiers.shift);

        if up {
            if state.selected == 0 {
                state.selected = n - 1;
            } else {
                state.selected -= 1;
            }
        }
        if down {
            state.selected = (state.selected + 1) % n;
        }

        // Enter：接受当前候选。
        if i.key_pressed(egui::Key::Enter) {
            out.accept = Some(state.selected);
        }

        // Esc：关闭弹窗。
        if i.key_pressed(egui::Key::Escape) {
            out.closed = true;
        }
    });

    // 已决定关闭则提前返回（不画弹窗）。
    if out.closed {
        return out;
    }

    // ── 计算弹窗尺寸 ──────────────────────────────────────────────────
    let visible_rows = candidates.len().min(MAX_VISIBLE_ROWS);
    let list_h = visible_rows as f32 * ROW_HEIGHT;
    let popup_h = list_h + VPAD * 2.0;

    // 弹窗**向上**展开：anchor 是 footer 上方位置，弹窗顶部 = anchor.y - popup_h。
    let popup_min = egui::pos2(view.anchor.x, view.anchor.y - popup_h);
    let popup_rect = egui::Rect::from_min_size(popup_min, egui::vec2(POPUP_WIDTH, popup_h));

    // ── Area 浮层（Foreground 层，盖在终端纹理之上）──────────────────
    egui::Area::new(egui::Id::new("lumen_completion_popup"))
        .fixed_pos(popup_rect.min)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            // 背景底色 + 描边 + 圆角。
            let painter = ui.painter();
            painter.rect_filled(popup_rect, RADIUS, pal.bg_dark);
            painter.rect_stroke(
                popup_rect,
                RADIUS,
                egui::Stroke::new(1.0, pal.panel_outline),
                egui::StrokeKind::Inside,
            );

            // 内容区。
            let inner = popup_rect.shrink2(egui::vec2(HPAD, VPAD));
            let list_rect = egui::Rect::from_min_size(inner.min, egui::vec2(inner.width(), list_h));

            if candidates.is_empty() {
                return;
            }

            let mut scroll_ui = ui.new_child(egui::UiBuilder::new().max_rect(list_rect));
            egui::ScrollArea::vertical()
                .id_salt("completion_list")
                .max_height(list_h)
                .auto_shrink([false, true])
                .show(&mut scroll_ui, |ui| {
                    for (idx, row) in candidates.iter().enumerate() {
                        let row_rect = egui::Rect::from_min_size(
                            ui.cursor().min,
                            egui::vec2(ui.available_width(), ROW_HEIGHT),
                        );
                        let resp = ui.interact(
                            row_rect,
                            ui.id().with(("completion_row", idx)),
                            egui::Sense::click(),
                        );

                        let is_selected = idx == state.selected;
                        let is_hovered = resp.hovered();

                        // 高亮背景（selected 或 hover）。
                        if is_selected || is_hovered {
                            ui.painter().rect_filled(row_rect, 3.0, pal.bg_highlight);
                        }

                        // 点击接受。
                        if resp.clicked() {
                            state.selected = idx;
                            out.accept = Some(idx);
                        }
                        // hover 时即时更新 selected。
                        if is_hovered {
                            state.selected = idx;
                        }

                        // 文本绘制：目录用 accent 色、文件用 fg。
                        let text_color = if row.is_dir { pal.accent } else { pal.fg };
                        let p = ui.painter();
                        let galley = p.layout_no_wrap(
                            row.display.clone(),
                            egui::FontId::monospace(13.0),
                            text_color,
                        );
                        let text_pos = egui::pos2(
                            row_rect.min.x + 2.0,
                            row_rect.center().y - galley.size().y / 2.0,
                        );
                        p.galley(text_pos, galley, text_color);

                        ui.advance_cursor_after_rect(row_rect);
                    }
                });
        });

    out
}
