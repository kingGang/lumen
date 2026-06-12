//! 底部状态栏（M4.1 批E，海风哥反馈 #3/#6）——VSCode 式样全宽窄条。
//!
//! # 布局
//! `Panel::bottom` 贴底全宽，在 `CentralPanel` 之前声明（egui 面板布局：
//! bottom > left/right > central，声明顺序决定剩余区域压缩方向）。
//!
//! # 内容（左→右）
//! - 左：输入模式指示（颜色语义：Fallback 用警示微黄）
//! - 中：焦点窗格 cwd（截断显示，hover 全路径）
//! - 右：经典直通切换按钮（真按钮样式，hover tooltip，点击=ToggleFallback 同路径）
//!
//! # 经典模式按钮视觉规格（第十轮问题2）
//! - 圆角矩形，圆角 4px（rounding = 4.0）
//! - 1px `panel_outline` 描边，无填充（关闭态）
//! - 悬停态：`bg_highlight` 填充（与其他控件 hover 一致）
//! - 开启态（force_fallback=true）：`accent` 填充 + `accent_fg` 文字（白底黑字醒目 CTA）
//! - 文字字号 11，垂直居中，按钮高度约 18px（24px 状态栏垂直居中）
//!
//! # 设计原则
//! - 状态栏高度常驻（不影响 footer 的 resize 链，两者独立）
//! - 切换按钮与 Ctrl+Shift+E 完全同路径（复用 dispatch ToggleFallback）
//! - 全部文案走 i18n 三语

use super::theme::Palette;
use crate::mode::InputMode;

/// 底部状态栏高度（逻辑像素）。
pub const HEIGHT: f32 = 24.0;

/// 状态栏 UI 的产出。
#[derive(Default)]
pub struct StatusBarOutput {
    /// 点击了经典直通切换按钮（走 dispatch ToggleFallback 同路径）。
    pub toggle_fallback: bool,
}

/// 绘制底部状态栏。
///
/// # Arguments
/// * `root` - egui 根 Ui（Panel::bottom 注册后的 show_inside 上下文）。
/// * `mode` - 当前有效输入模式（每帧推导，不缓存）。
/// * `cwd` - 焦点窗格的当前工作目录（None 时中央区域空白）。
/// * `force_fallback` - 经典直通模式开关（决定右端按钮显示状态）。
/// * `pal` - 当前主题色板。
///
/// # Errors
/// 本函数仅做 egui 绘制，无 IO，不返回 Result。
pub fn show(
    root: &mut egui::Ui,
    mode: InputMode,
    cwd: Option<&std::path::Path>,
    force_fallback: bool,
    pal: &Palette,
) -> StatusBarOutput {
    let mut out = StatusBarOutput::default();
    let s = crate::i18n::strings();

    // 顶边 1px 描边（与全 app 面板描边一致）
    {
        use egui::emath::GuiRounding as _;
        let ppp = root.pixels_per_point();
        let r = root.available_rect_before_wrap().round_to_pixels(ppp);
        let hw = 0.5 / ppp;
        root.painter().line_segment(
            [
                egui::pos2(r.min.x, r.min.y + hw),
                egui::pos2(r.max.x, r.min.y + hw),
            ],
            egui::Stroke::new(1.0 / ppp, pal.panel_outline),
        );
    }

    // 水平三段布局：左（模式指示）| 中（cwd）| 右（按钮）
    root.horizontal(|ui| {
        ui.add_space(8.0);

        // ── 左：输入模式指示 ────────────────────────────────────────
        let (mode_text, mode_color) = mode_label(mode, force_fallback, s, pal);
        ui.add(
            egui::Label::new(egui::RichText::new(mode_text).size(11.0).color(mode_color))
                .selectable(false),
        );

        ui.add_space(12.0);

        // ── 中：cwd（截断显示，hover 全路径）──────────────────────
        // 先算右端按钮的宽度，留出空间给 cwd 截断
        let btn_text = if force_fallback {
            s.statusbar_classic_on
        } else {
            s.statusbar_classic_off
        };
        // cwd 区域：占满剩余宽度（留右端按钮宽度 + 右边距）
        // 用 with_layout 左右分段
        let available_w = ui.available_width();
        // 按钮文字宽度估算：ASCII 字符约 7px，CJK 全角字符约 12px（11pt 比例字体实测）。
        // 与下方 allocate_exact_size 的 btn_size.x 公式保持一致。
        let btn_text_w: f32 = btn_text
            .chars()
            .map(|c| if c.is_ascii() { 7.0_f32 } else { 12.0_f32 })
            .sum();
        let btn_w_approx = btn_text_w + 12.0 + 8.0;
        let cwd_w = (available_w - btn_w_approx - 8.0).max(10.0);

        if let Some(path) = cwd {
            // cwd 显示：尾目录名（截断时可 hover 看全路径）
            let display = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let full_path = path.display().to_string();

            let (_, cwd_resp) = ui.allocate_exact_size(
                egui::vec2(cwd_w, ui.available_height()),
                egui::Sense::hover(),
            );
            // 在分配的矩形内绘制截断文本
            ui.painter().text(
                egui::pos2(cwd_resp.rect.min.x, cwd_resp.rect.center().y),
                egui::Align2::LEFT_CENTER,
                &display,
                egui::FontId::proportional(11.0),
                pal.fg_dim,
            );
            if display != full_path {
                cwd_resp.on_hover_text(full_path);
            }
        } else {
            // 无 cwd：占位留空（保持布局稳定）
            ui.allocate_exact_size(
                egui::vec2(cwd_w, ui.available_height()),
                egui::Sense::hover(),
            );
        }

        // ── 右：经典直通切换按钮（真按钮样式，第十轮问题2）──────────
        // 开启态（force_fallback=true）：accent 白底 + accent_fg 黑字（醒目 CTA）。
        // 关闭态：线框灰字（panel_outline 1px 描边 + fg_dim 文字，低调）。
        // 悬停：bg_highlight 填充（统一 hover 档）。
        // 圆角 4px，高度约 18px（24px 状态栏垂直居中）。
        let btn_fill = if force_fallback {
            pal.accent
        } else {
            egui::Color32::TRANSPARENT
        };
        let btn_text_color = if force_fallback {
            pal.accent_fg
        } else {
            pal.fg_dim
        };

        // 按钮内边距（水平 6px，垂直 1px），使整体高约 18px
        let btn_h = 18.0_f32;
        let pad_v = ((ui.available_height() - btn_h) / 2.0).max(0.0);
        ui.add_space(0.0); // flush 布局
                           // 按钮宽度：与上方 btn_w_approx 公式一致（ASCII 7px + CJK 12px，加 12px 内边距）。
        let btn_size = egui::vec2(btn_text_w + 12.0, btn_h);

        // 使用 allocate_exact_size 手工绘制，以获得完整视觉控制
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(btn_size.x, ui.available_height()),
            egui::Sense::click(),
        );
        let resp = resp.on_hover_text(s.statusbar_classic_tip);

        // 垂直居中的按钮矩形
        let btn_rect = egui::Rect::from_min_size(
            egui::pos2(rect.min.x, rect.center().y - btn_h / 2.0),
            egui::vec2(rect.width(), btn_h),
        );

        if ui.is_rect_visible(btn_rect) {
            let painter = ui.painter();
            let rounding = egui::CornerRadius::same(4);

            // 背景填充（悬停 or 开启态）
            let fill = if force_fallback {
                btn_fill
            } else if resp.hovered() {
                pal.bg_highlight
            } else {
                egui::Color32::TRANSPARENT
            };
            if fill != egui::Color32::TRANSPARENT {
                painter.rect_filled(btn_rect, rounding, fill);
            }

            // 1px 描边（关闭态始终显示；开启态白底自带视觉边界，但仍留描边保持形状）
            let stroke_color = if force_fallback {
                // 开启态：accent 描边（白底）
                pal.accent
            } else {
                pal.panel_outline
            };
            painter.rect_stroke(
                btn_rect,
                rounding,
                egui::Stroke::new(1.0, stroke_color),
                egui::StrokeKind::Inside,
            );

            // 文字居中
            painter.text(
                btn_rect.center(),
                egui::Align2::CENTER_CENTER,
                btn_text,
                egui::FontId::proportional(11.0),
                btn_text_color,
            );
        }

        if resp.clicked() {
            out.toggle_fallback = true;
        }

        let _ = pad_v; // 已由 allocate_exact_size 覆盖居中逻辑

        ui.add_space(8.0);
    });

    out
}

/// 按模式返回 (显示文字, 颜色)。
///
/// Fallback 态用警示微黄提示非默认态；其余用 fg_dim。
fn mode_label<'s>(
    mode: InputMode,
    force_fallback: bool,
    s: &'s crate::i18n::Strings,
    pal: &Palette,
) -> (&'s str, egui::Color32) {
    // force_fallback=true 时 mode 已被 effective_mode 强制为 Fallback，
    // 此处直接按 mode 分支即可；但用 force_fallback 额外加警示色。
    let _ = force_fallback; // force_fallback 的颜色已在调用处通过 mode=Fallback 体现
    match mode {
        InputMode::Compose => (s.statusbar_mode_compose, pal.fg_dim),
        InputMode::Running => (s.statusbar_mode_running, pal.fg_dim),
        InputMode::AltScreen => (s.statusbar_mode_altscreen, pal.fg_dim),
        // Fallback（含 force_fallback 覆盖）：警示微黄
        InputMode::Fallback => (
            s.statusbar_mode_fallback,
            egui::Color32::from_rgb(220, 180, 60),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 简单验证 mode_label 返回非空文字（编译期 i18n 完备性已保证三语覆盖）。
    // 此处仅钉住「每个模式都有输出文字」的行为契约。

    fn dummy_pal() -> Palette {
        // 用深色主题色板（测试无需真实渲染）
        use crate::shell::theme;
        let info = crate::settings::theme_info("dark");
        theme::shell_palette(info)
    }

    #[test]
    fn 状态栏模式文字_compose_非空() {
        use crate::i18n;
        let s = i18n::strings();
        let pal = dummy_pal();
        let (text, _) = mode_label(InputMode::Compose, false, s, &pal);
        assert!(!text.is_empty(), "Compose 模式文字不应为空");
    }

    #[test]
    fn 状态栏模式文字_running_非空() {
        use crate::i18n;
        let s = i18n::strings();
        let pal = dummy_pal();
        let (text, _) = mode_label(InputMode::Running, false, s, &pal);
        assert!(!text.is_empty(), "Running 模式文字不应为空");
    }

    #[test]
    fn 状态栏模式文字_altscreen_非空() {
        use crate::i18n;
        let s = i18n::strings();
        let pal = dummy_pal();
        let (text, _) = mode_label(InputMode::AltScreen, false, s, &pal);
        assert!(!text.is_empty(), "AltScreen 模式文字不应为空");
    }

    #[test]
    fn 状态栏模式文字_fallback_用警示色() {
        use crate::i18n;
        let s = i18n::strings();
        let pal = dummy_pal();
        let (text, color) = mode_label(InputMode::Fallback, true, s, &pal);
        assert!(!text.is_empty(), "Fallback 模式文字不应为空");
        // Fallback 用警示微黄，不等于 fg_dim
        assert_ne!(color, pal.fg_dim, "Fallback 模式应用警示色而非 fg_dim");
    }

    #[test]
    fn 占位提示条件_空缓冲显示_非空不显示() {
        // 此测试钉住「lines 全空 → 应显示 placeholder」的数据契约
        // （renderer 侧的渲染逻辑按此判断，app 侧组装时设 placeholder）
        use lumen_renderer::composer_view::ComposerView;

        let mut v = ComposerView::compose_empty("Compose");
        v.placeholder = Some("占位提示".to_owned());

        // 空行（第一行为空字符串）：应显示 placeholder
        let all_empty = v.lines.iter().all(|l| l.is_empty());
        assert!(all_empty, "空编辑器的 lines 应全为空串");

        // 非空：不显示 placeholder
        v.lines[0] = "ls".to_owned();
        let all_empty = v.lines.iter().all(|l| l.is_empty());
        assert!(!all_empty, "非空编辑器 lines 不应全为空串");
    }
}
