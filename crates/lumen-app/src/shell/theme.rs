//! 外壳主题：Warp 风深色（扁平、小圆角、低对比描边），
//! 色板与终端区 Tokyo Night 主题同源（见 lumen-renderer/src/theme.rs）。

use std::sync::Arc;

/// 侧栏底色（比终端背景更深一档）。
pub const BG_DARK: egui::Color32 = egui::Color32::from_rgb(0x16, 0x16, 0x1e);
/// 弹层/卡片底色。
pub const BG_PANEL: egui::Color32 = egui::Color32::from_rgb(0x1f, 0x23, 0x35);
/// 高亮底色（hover、激活条目）。
pub const BG_HIGHLIGHT: egui::Color32 = egui::Color32::from_rgb(0x29, 0x2e, 0x42);
/// 主文字色。
pub const FG: egui::Color32 = egui::Color32::from_rgb(0xc0, 0xca, 0xf5);
/// 次要文字色（注释灰蓝）。
pub const FG_DIM: egui::Color32 = egui::Color32::from_rgb(0x56, 0x5f, 0x89);
/// 强调色（蓝）。
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x7a, 0xa2, 0xf7);
/// 选区底色（与终端选区一致）。
pub const SELECTION: egui::Color32 = egui::Color32::from_rgb(0x2e, 0x3c, 0x64);

/// 应用全局 egui 样式。
pub fn apply_style(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.indent = 14.0;

    let v = &mut style.visuals;
    v.dark_mode = true;
    v.panel_fill = BG_DARK;
    v.window_fill = BG_PANEL;
    v.extreme_bg_color = egui::Color32::from_rgb(0x10, 0x10, 0x17);
    v.faint_bg_color = BG_HIGHLIGHT;
    v.selection.bg_fill = SELECTION;
    v.selection.stroke = egui::Stroke::new(1.0, FG);
    v.hyperlink_color = ACCENT;
    v.window_corner_radius = egui::CornerRadius::same(10);
    v.window_stroke = egui::Stroke::new(1.0, BG_HIGHLIGHT);

    let corner = egui::CornerRadius::same(6);
    v.widgets.noninteractive.corner_radius = corner;
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, FG);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, BG_HIGHLIGHT);
    v.widgets.inactive.corner_radius = corner;
    v.widgets.inactive.weak_bg_fill = BG_PANEL;
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, FG);
    v.widgets.hovered.corner_radius = corner;
    v.widgets.hovered.weak_bg_fill = BG_HIGHLIGHT;
    v.widgets.hovered.bg_fill = BG_HIGHLIGHT;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, FG_DIM);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.5, FG);
    v.widgets.active.corner_radius = corner;
    v.widgets.active.weak_bg_fill = SELECTION;
    v.widgets.active.bg_fill = SELECTION;
    v.widgets.active.fg_stroke = egui::Stroke::new(1.5, FG);
    v.widgets.open.corner_radius = corner;
    v.widgets.open.weak_bg_fill = BG_HIGHLIGHT;
    v.widgets.open.fg_stroke = egui::Stroke::new(1.0, FG);

    ctx.set_global_style(style);
}

/// 运行时加载系统中文字体并插入 fallback 列表。
///
/// 优先微软雅黑（msyh.ttc，按 index 0 取字面），缺失则降级黑体；
/// 都没有时仅记录警告（外壳界面中文会缺字，终端区不受影响——
/// 终端文字走 lumen-renderer 的 glyphon 管线，与 egui 字体无关）。
/// 字体约 19MB，运行时读取而非编进二进制。
pub fn install_cjk_fonts(ctx: &egui::Context) {
    const CANDIDATES: [(&str, u32); 2] = [
        ("C:/Windows/Fonts/msyh.ttc", 0),
        ("C:/Windows/Fonts/simhei.ttf", 0),
    ];
    let mut fonts = egui::FontDefinitions::default();
    for (path, index) in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut data = egui::FontData::from_owned(bytes);
        data.index = index;
        fonts.font_data.insert("cjk".to_owned(), Arc::new(data));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&family) {
                list.push("cjk".to_owned());
            }
        }
        ctx.set_fonts(fonts);
        log::info!("外壳中文字体: {path}");
        return;
    }
    log::warn!("未找到系统中文字体（msyh.ttc / simhei.ttf），外壳界面中文将无法显示");
}
