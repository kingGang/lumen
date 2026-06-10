//! 外壳主题：Warp 风扁平、小圆角、低对比描边。
//!
//! M3.4 起支持深/浅双色板（[`Palette`]），与终端区主题联动（设置页
//! Appearance 切换）：深色对应 Tokyo Night，浅色对应 Tokyo Night
//! Light，色板与 lumen-renderer/src/theme.rs 的终端色同源。

use std::sync::Arc;

/// 外壳 UI 色板。所有面板/控件颜色经由它取用（不再用裸常量），
/// 主题切换 = 换一个 `&'static Palette` + [`apply_style`] 重设 egui 样式。
pub struct Palette {
    /// 是否浅色主题（egui `dark_mode` 取反）。
    pub light: bool,
    /// 侧栏底色（比终端背景更深/浅一档）。
    pub bg_dark: egui::Color32,
    /// 弹层/卡片底色。
    pub bg_panel: egui::Color32,
    /// 高亮底色（hover、激活条目）。
    pub bg_highlight: egui::Color32,
    /// 主文字色。
    pub fg: egui::Color32,
    /// 次要文字色。
    pub fg_dim: egui::Color32,
    /// 强调色。
    pub accent: egui::Color32,
    /// 选区底色（与终端选区一致）。
    pub selection: egui::Color32,
    /// 文件树栏底色（与 tab 侧栏差一线，区分两栏）。
    pub filetree_fill: egui::Color32,
    /// 输入框等的极暗/极亮底色。
    pub extreme_bg: egui::Color32,
    /// 警示文字色（设置页字体回退提示等）。
    pub warn: egui::Color32,
}

/// 深色色板（Tokyo Night 同源）。
pub static DARK: Palette = Palette {
    light: false,
    bg_dark: egui::Color32::from_rgb(0x16, 0x16, 0x1e),
    bg_panel: egui::Color32::from_rgb(0x1f, 0x23, 0x35),
    bg_highlight: egui::Color32::from_rgb(0x29, 0x2e, 0x42),
    fg: egui::Color32::from_rgb(0xc0, 0xca, 0xf5),
    fg_dim: egui::Color32::from_rgb(0x56, 0x5f, 0x89),
    accent: egui::Color32::from_rgb(0x7a, 0xa2, 0xf7),
    selection: egui::Color32::from_rgb(0x2e, 0x3c, 0x64),
    filetree_fill: egui::Color32::from_rgb(0x19, 0x1a, 0x27),
    extreme_bg: egui::Color32::from_rgb(0x10, 0x10, 0x17),
    warn: egui::Color32::from_rgb(0xe0, 0xaf, 0x68),
};

/// 浅色色板（Tokyo Night Light 同源）。
pub static LIGHT: Palette = Palette {
    light: true,
    bg_dark: egui::Color32::from_rgb(0xd4, 0xd6, 0xe1),
    bg_panel: egui::Color32::from_rgb(0xe6, 0xe7, 0xed),
    bg_highlight: egui::Color32::from_rgb(0xc0, 0xc6, 0xda),
    fg: egui::Color32::from_rgb(0x37, 0x60, 0xbf),
    fg_dim: egui::Color32::from_rgb(0x84, 0x8c, 0xb5),
    accent: egui::Color32::from_rgb(0x2e, 0x7d, 0xe9),
    selection: egui::Color32::from_rgb(0xb7, 0xc1, 0xe3),
    filetree_fill: egui::Color32::from_rgb(0xdc, 0xde, 0xe8),
    extreme_bg: egui::Color32::from_rgb(0xf0, 0xf1, 0xf5),
    warn: egui::Color32::from_rgb(0x8c, 0x6c, 0x3e),
};

/// 按明暗取色板。
pub fn palette(light: bool) -> &'static Palette {
    if light {
        &LIGHT
    } else {
        &DARK
    }
}

/// 应用全局 egui 样式（启动与主题切换时调用）。
pub fn apply_style(ctx: &egui::Context, pal: &Palette) {
    let mut style = (*ctx.global_style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.indent = 14.0;

    let v = &mut style.visuals;
    v.dark_mode = !pal.light;
    v.panel_fill = pal.bg_dark;
    v.window_fill = pal.bg_panel;
    v.extreme_bg_color = pal.extreme_bg;
    v.faint_bg_color = pal.bg_highlight;
    v.selection.bg_fill = pal.selection;
    v.selection.stroke = egui::Stroke::new(1.0, pal.fg);
    v.hyperlink_color = pal.accent;
    v.window_corner_radius = egui::CornerRadius::same(10);
    v.window_stroke = egui::Stroke::new(1.0, pal.bg_highlight);

    let corner = egui::CornerRadius::same(6);
    v.widgets.noninteractive.corner_radius = corner;
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, pal.fg);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, pal.bg_highlight);
    v.widgets.inactive.corner_radius = corner;
    v.widgets.inactive.weak_bg_fill = pal.bg_panel;
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, pal.fg);
    v.widgets.hovered.corner_radius = corner;
    v.widgets.hovered.weak_bg_fill = pal.bg_highlight;
    v.widgets.hovered.bg_fill = pal.bg_highlight;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, pal.fg_dim);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.5, pal.fg);
    v.widgets.active.corner_radius = corner;
    v.widgets.active.weak_bg_fill = pal.selection;
    v.widgets.active.bg_fill = pal.selection;
    v.widgets.active.fg_stroke = egui::Stroke::new(1.5, pal.fg);
    v.widgets.open.corner_radius = corner;
    v.widgets.open.weak_bg_fill = pal.bg_highlight;
    v.widgets.open.fg_stroke = egui::Stroke::new(1.0, pal.fg);

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
