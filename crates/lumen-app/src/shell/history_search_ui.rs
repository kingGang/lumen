//! 历史搜索面板 UI（M4.3 Ctrl+R）。
//!
//! 以 egui Modal 实现全屏遮罩 + 顶部居中浮层，与设置页/登录页同款层级。
//! 引擎（[`crate::history::HistoryStore::fuzzy_search`]）由 main.rs 调用，
//! 结果以 [`HistoryRow`] 切片传入本模块；本模块只负责 UI 渲染与键盘导航。

use super::theme::Palette;
use crate::i18n;

/// 面板固定逻辑宽度（像素）；实际宽取此值与可用宽 * 0.7 的较小值。
const PANEL_WIDTH: f32 = 680.0;
/// 搜索输入框高度。
const SEARCH_HEIGHT: f32 = 36.0;
/// 列表区最大高度（超出后可滚动）。
const LIST_MAX_HEIGHT: f32 = 360.0;
/// 每行高度。
const ROW_HEIGHT: f32 = 32.0;
/// 退出码徽标列宽。
const BADGE_COL: f32 = 24.0;
/// cwd 尾目录名列最大宽度（超出截断）。
const CWD_MAX_WIDTH: f32 = 140.0;
/// 面板圆角半径。
const PANEL_RADIUS: f32 = 10.0;
/// 面板内边距（水平/垂直）。
const PANEL_HPAD: f32 = 16.0;
const PANEL_VPAD: f32 = 12.0;

/// 历史搜索面板的跨帧 UI 状态。
#[derive(Default)]
pub struct HistorySearchUiState {
    /// 面板是否打开。
    pub open: bool,
    /// 搜索输入框当前文本。
    pub query: String,
    /// 当前高亮行下标（0 = 第一行）。
    pub selected: usize,
    /// 为 true 时下一帧对搜索框 request_focus 并清零此标志。
    pub focus_query: bool,
}

/// 历史搜索面板中一行的展示数据（由 main.rs 从 fuzzy_search + entries 构造）。
pub struct HistoryRow {
    /// 命令文本（用于展示与填入）。
    pub text: String,
    /// 工作目录（可选；展示尾目录名）。
    pub cwd: Option<String>,
    /// 退出码（None 不展示徽标）。
    pub exit_code: Option<i32>,
    /// 命中字符的字节区间列表 `[start, end)`，已合并连续段，用于高亮。
    pub match_spans: Vec<(usize, usize)>,
}

/// 一帧历史搜索面板 UI 的产出。
#[derive(Default)]
pub struct HistorySearchOutput {
    /// 用户选定了某条命令，值为命令文本（应填入输入框）。
    pub accept: Option<String>,
    /// 面板应关闭（Esc / backdrop 点击）。
    pub closed: bool,
    /// query 本帧发生变化（main 应 request_redraw 在下帧重新计算搜索结果）。
    pub query_changed: bool,
}

/// 绘制历史搜索面板。调用方保证 `state.open == true` 时才调用。
///
/// # Arguments
/// * `ctx`   - egui 上下文。
/// * `state` - 跨帧 UI 状态（query / selected / focus_query）。
/// * `rows`  - 本帧的搜索结果行（由 main 在调用前计算好）。
/// * `pal`   - 当前外壳色板。
///
/// # Returns
/// 本帧的 UI 产出（接受选定/关闭/query 变化信号）。
pub fn show(
    ctx: &egui::Context,
    state: &mut HistorySearchUiState,
    rows: &[HistoryRow],
    pal: &Palette,
) -> HistorySearchOutput {
    let mut out = HistorySearchOutput::default();
    let screen = ctx.content_rect();

    // ── 键盘事件（在 Modal 之前读取，避免 Modal 消化部分按键）──────────
    // 注意：egui 的 Modal 会在 should_close 中处理 Esc，但我们先在此处
    // 捕获方向键与 Enter，避免它们被其他控件（TextEdit）消化。
    ctx.input(|i| {
        // ↑：向上移动选中行（钳制到 0）。
        if i.key_pressed(egui::Key::ArrowUp) && state.selected > 0 {
            state.selected -= 1;
        }
        // ↓：向下移动选中行（钳制到 rows.len().saturating_sub(1)）。
        if i.key_pressed(egui::Key::ArrowDown) {
            let max = rows.len().saturating_sub(1);
            if state.selected < max {
                state.selected += 1;
            }
        }
        // Enter：接受当前选中行。
        if i.key_pressed(egui::Key::Enter) {
            if let Some(row) = rows.get(state.selected) {
                out.accept = Some(row.text.clone());
            }
        }
        // Esc：关闭面板。
        if i.key_pressed(egui::Key::Escape) {
            out.closed = true;
        }
    });

    // selected 每帧按 rows.len() 钳制（rows 可能减少）。
    if !rows.is_empty() {
        state.selected = state.selected.min(rows.len() - 1);
    } else {
        state.selected = 0;
    }

    // 已决定关闭则提前返回，不必再画 Modal。
    if out.closed {
        return out;
    }

    // ── Modal 遮罩 + 面板 ────────────────────────────────────────────────
    let modal = egui::Modal::new(egui::Id::new("lumen_history_search_modal"))
        // 半透明 backdrop；点击 backdrop 关闭。
        .backdrop_color(egui::Color32::from_black_alpha(100))
        .frame(egui::Frame::NONE)
        .show(ctx, |ui| {
            // 面板宽度：取 PANEL_WIDTH 与可用宽 70% 的较小值。
            let avail_w = screen.width();
            let panel_w = PANEL_WIDTH.min(avail_w * 0.7);

            // 面板顶部偏上居中（距屏幕顶部约 15%）。
            let panel_top = screen.min.y + screen.height() * 0.15;
            let panel_left = screen.center().x - panel_w / 2.0;

            // 搜索框高 + 列表高（最多 LIST_MAX_HEIGHT）+ 底部提示行高 +
            // 内边距。
            let list_h = (rows.len() as f32 * ROW_HEIGHT).min(LIST_MAX_HEIGHT);
            let hint_h = 20.0;
            let inner_h = SEARCH_HEIGHT + 8.0 + list_h + 8.0 + hint_h;
            let panel_h = inner_h + PANEL_VPAD * 2.0;

            let panel_rect = egui::Rect::from_min_size(
                egui::pos2(panel_left, panel_top),
                egui::vec2(panel_w, panel_h),
            );

            // 面板底色 + 描边 + 圆角（Foreground 层，盖在终端纹理之上）。
            let painter = ui.painter();
            painter.rect_filled(panel_rect, PANEL_RADIUS, pal.bg_dark);
            painter.rect_stroke(
                panel_rect,
                PANEL_RADIUS,
                egui::Stroke::new(1.0, pal.panel_outline),
                egui::StrokeKind::Inside,
            );

            // 面板内容区（内边距）。
            let inner = panel_rect.shrink2(egui::vec2(PANEL_HPAD, PANEL_VPAD));

            // —— 搜索输入框 ——
            let search_rect =
                egui::Rect::from_min_size(inner.min, egui::vec2(inner.width(), SEARCH_HEIGHT));
            let mut search_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(search_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            let s = i18n::strings();
            let old_query = state.query.clone();
            let edit_resp = search_ui.add(
                egui::TextEdit::singleline(&mut state.query)
                    .hint_text(s.history_search_placeholder)
                    .desired_width(f32::INFINITY)
                    .font(egui::FontId::monospace(13.0)),
            );
            // focus_query 标志：request_focus 并清零。
            if state.focus_query {
                edit_resp.request_focus();
                state.focus_query = false;
            }
            // 检测 query 变化。
            if state.query != old_query {
                out.query_changed = true;
                state.selected = 0;
            }

            // —— 列表区 ——
            let list_top = search_rect.max.y + 8.0;
            let list_rect = egui::Rect::from_min_size(
                egui::pos2(inner.min.x, list_top),
                egui::vec2(inner.width(), list_h),
            );

            if rows.is_empty() {
                // 空态文案。
                let mut empty_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(list_rect)
                        .layout(egui::Layout::top_down(egui::Align::Center)),
                );
                empty_ui.add_space(8.0);
                empty_ui.label(
                    egui::RichText::new(s.history_search_empty)
                        .size(13.0)
                        .color(pal.fg_dim),
                );
            } else {
                // 滚动列表。
                let mut scroll_ui = ui.new_child(egui::UiBuilder::new().max_rect(list_rect));
                egui::ScrollArea::vertical()
                    .id_salt("history_search_list")
                    .max_height(LIST_MAX_HEIGHT)
                    .auto_shrink([false, true])
                    .show(&mut scroll_ui, |ui| {
                        for (idx, row) in rows.iter().enumerate() {
                            let row_rect = egui::Rect::from_min_size(
                                ui.cursor().min,
                                egui::vec2(ui.available_width(), ROW_HEIGHT),
                            );
                            let resp = ui.interact(
                                row_rect,
                                ui.id().with(("history_row", idx)),
                                egui::Sense::click(),
                            );

                            let is_selected = idx == state.selected;
                            let is_hovered = resp.hovered();

                            // 高亮背景（selected 或 hover）。
                            if is_selected || is_hovered {
                                ui.painter().rect_filled(row_rect, 4.0, pal.bg_highlight);
                            }

                            // 点击选中并接受。
                            if resp.clicked() {
                                state.selected = idx;
                                out.accept = Some(row.text.clone());
                            }
                            // hover 时更新 selected（鼠标悬停即高亮）。
                            if is_hovered {
                                state.selected = idx;
                            }

                            // 内容绘制（badge + 命令文本 + cwd）。
                            let p = ui.painter();

                            // 退出码徽标（exit_code=Some(0) → 绿 ✓；Some(非0) → 红 ✗；None → 无）。
                            let badge_x = row_rect.min.x + 4.0;
                            let badge_y = row_rect.center().y;
                            match row.exit_code {
                                Some(0) => {
                                    p.text(
                                        egui::pos2(badge_x, badge_y),
                                        egui::Align2::LEFT_CENTER,
                                        "✓",
                                        egui::FontId::proportional(11.0),
                                        egui::Color32::from_rgb(0x4a, 0xc2, 0x6c),
                                    );
                                }
                                Some(_) => {
                                    p.text(
                                        egui::pos2(badge_x, badge_y),
                                        egui::Align2::LEFT_CENTER,
                                        "✗",
                                        egui::FontId::proportional(11.0),
                                        egui::Color32::from_rgb(0xcc, 0x4a, 0x4a),
                                    );
                                }
                                None => {}
                            }

                            // cwd 尾目录名（右对齐，fg_dim，截断）。
                            let cwd_name: Option<String> = row.cwd.as_deref().and_then(|c| {
                                std::path::Path::new(c)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .map(|n| n.to_owned())
                            });
                            let cwd_width = if cwd_name.is_some() {
                                CWD_MAX_WIDTH
                            } else {
                                0.0
                            };

                            // 命令文本区域（badge 列之后，cwd 列之前）。
                            let text_left = row_rect.min.x + BADGE_COL;
                            let text_right = row_rect.max.x - cwd_width - 8.0;
                            let text_rect = egui::Rect::from_min_max(
                                egui::pos2(text_left, row_rect.min.y),
                                egui::pos2(text_right, row_rect.max.y),
                            );

                            // 命令文本：用 LayoutJob 实现 match_spans 高亮。
                            let mut job = egui::text::LayoutJob {
                                wrap: egui::text::TextWrapping {
                                    max_width: text_rect.width(),
                                    max_rows: 1,
                                    break_anywhere: true,
                                    ..Default::default()
                                },
                                first_row_min_height: ROW_HEIGHT,
                                ..Default::default()
                            };

                            let text = &row.text;
                            let mut cursor = 0usize;
                            for &(span_start, span_end) in &row.match_spans {
                                // 普通段（span_start 之前）。
                                if cursor < span_start {
                                    job.append(
                                        &text[cursor..span_start],
                                        0.0,
                                        egui::TextFormat {
                                            font_id: egui::FontId::monospace(13.0),
                                            color: pal.fg,
                                            ..Default::default()
                                        },
                                    );
                                }
                                // 高亮段。
                                if span_start < span_end && span_end <= text.len() {
                                    job.append(
                                        &text[span_start..span_end],
                                        0.0,
                                        egui::TextFormat {
                                            font_id: egui::FontId::monospace(13.0),
                                            color: pal.accent,
                                            ..Default::default()
                                        },
                                    );
                                }
                                cursor = span_end;
                            }
                            // 剩余普通段。
                            if cursor < text.len() {
                                job.append(
                                    &text[cursor..],
                                    0.0,
                                    egui::TextFormat {
                                        font_id: egui::FontId::monospace(13.0),
                                        color: pal.fg,
                                        ..Default::default()
                                    },
                                );
                            }
                            // 空文本兜底（不崩溃）。
                            if text.is_empty() {
                                job.append(
                                    "",
                                    0.0,
                                    egui::TextFormat {
                                        font_id: egui::FontId::monospace(13.0),
                                        color: pal.fg,
                                        ..Default::default()
                                    },
                                );
                            }

                            let galley = p.layout_job(job);
                            let text_pos = egui::pos2(
                                text_rect.min.x,
                                text_rect.center().y - galley.size().y / 2.0,
                            );
                            p.galley(text_pos, galley, pal.fg);

                            // cwd 尾目录名（右对齐）。
                            if let Some(name) = cwd_name {
                                let cwd_rect = egui::Rect::from_min_max(
                                    egui::pos2(row_rect.max.x - cwd_width, row_rect.min.y),
                                    egui::pos2(row_rect.max.x - 4.0, row_rect.max.y),
                                );
                                let cwd_galley = p.layout_no_wrap(
                                    name,
                                    egui::FontId::proportional(11.0),
                                    pal.fg_dim,
                                );
                                let cwd_pos = egui::pos2(
                                    cwd_rect.max.x - cwd_galley.size().x.min(cwd_width - 4.0),
                                    cwd_rect.center().y - cwd_galley.size().y / 2.0,
                                );
                                p.galley(cwd_pos, cwd_galley, pal.fg_dim);
                            }

                            // 分配行高让 ScrollArea 正确计算总高度。
                            ui.advance_cursor_after_rect(row_rect);
                        }
                    });
            }

            // —— 底部提示行 ——
            let hint_top = list_rect.max.y + 8.0;
            let hint_rect = egui::Rect::from_min_size(
                egui::pos2(inner.min.x, hint_top),
                egui::vec2(inner.width(), hint_h),
            );
            let mut hint_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(hint_rect)
                    .layout(egui::Layout::top_down(egui::Align::Center)),
            );
            hint_ui.label(
                egui::RichText::new(s.history_search_hint)
                    .size(11.0)
                    .color(pal.fg_dim),
            );
        });

    // backdrop 点击或 Modal should_close（包含 Esc）→ 关闭。
    if modal.should_close() {
        out.closed = true;
    }

    out
}
