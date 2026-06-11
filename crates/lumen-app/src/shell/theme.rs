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
    /// 错误文字色（登录校验失败红字等），与终端主题红同源。
    pub error: egui::Color32,
    /// 信息提示色（toast Info 等），与终端主题青同源。
    pub info: egui::Color32,
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
    error: egui::Color32::from_rgb(0xf7, 0x76, 0x8e),
    info: egui::Color32::from_rgb(0x7d, 0xcf, 0xff),
};

/// 浅色色板（Tokyo Night Light 同源）。
///
/// 色值对齐 folke/tokyonight.nvim 官方 **day** 风格 UI 色
/// （extras/lua/tokyonight_day.lua，2026-06 校对）；逐项标注来源，
/// 无官方对应的两处（filetree_fill / extreme_bg）为手调过渡色。
pub static LIGHT: Palette = Palette {
    light: true,
    // day.bg_dark / bg_sidebar：侧栏比终端底色深一档。
    bg_dark: egui::Color32::from_rgb(0xd0, 0xd5, 0xe3),
    // day.bg：弹层/卡片取主底色（day 的 bg_float 与侧栏同为 #d0d5e3，
    // 弹层叠在侧栏上会糊成一片，取更浅的 bg 保住层级；描边仍有）。
    bg_panel: egui::Color32::from_rgb(0xe1, 0xe2, 0xe7),
    // day.bg_highlight：hover/激活条目。
    bg_highlight: egui::Color32::from_rgb(0xc4, 0xc8, 0xda),
    // day.fg。
    fg: egui::Color32::from_rgb(0x37, 0x60, 0xbf),
    // day.comment：次要文字。
    fg_dim: egui::Color32::from_rgb(0x84, 0x8c, 0xb5),
    // day.blue：强调色（与终端 ANSI 蓝同源）。
    accent: egui::Color32::from_rgb(0x2e, 0x7d, 0xe9),
    // day.bg_visual：选区（与终端选区一致）。
    selection: egui::Color32::from_rgb(0xb7, 0xc1, 0xe3),
    // 手调：bg_dark(#d0d5e3) 与 bg(#e1e2e7) 的中间过渡，区分两栏。
    filetree_fill: egui::Color32::from_rgb(0xd9, 0xdc, 0xe5),
    // 手调：比 bg 再亮一档作输入框底（day 无 extreme 档）。
    extreme_bg: egui::Color32::from_rgb(0xf0, 0xf1, 0xf5),
    // day.warning（= yellow）。
    warn: egui::Color32::from_rgb(0x8c, 0x6c, 0x3e),
    // day.error（= red1，比 red #f52a65 沉稳，浅底文字可读性更好）。
    error: egui::Color32::from_rgb(0xc6, 0x43, 0x43),
    // day.info。
    info: egui::Color32::from_rgb(0x07, 0x87, 0x9d),
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
