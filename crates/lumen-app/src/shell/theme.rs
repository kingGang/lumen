//! 外壳主题：Warp 风扁平、小圆角、纯黑白灰阶（M3.7b）。
//!
//! M3.4 起支持深/浅双色板（[`Palette`]），与终端区主题联动（设置页
//! Appearance 切换）。M3.7b 黑白化改版（海风哥：「不要蓝色的，要纯
//! 黑白的」）：两套色板全部重做为中性灰阶——深色近黑分层、浅色白灰
//! 分层，强调色从蓝改白/近黑，控件走「平时 → 悬停 → 激活」的亮度
//! 梯度（深色递亮、浅色递深），warn/error 语义色保留。终端区配色
//! （Tokyo Night 的 ANSI 16 色与块状态条）不在此处、保持彩色，见
//! lumen-renderer/src/theme.rs（仅选区底色同向改中性灰）。

use std::sync::Arc;

/// 外壳 UI 色板。所有面板/控件颜色经由它取用（不再用裸常量），
/// 主题切换 = 换一个 `&'static Palette` + [`apply_style`] 重设 egui 样式。
pub struct Palette {
    /// 是否浅色主题（egui `dark_mode` 取反）。
    pub light: bool,
    /// 主底色：顶栏/侧栏/设置页（深=近黑，浅=浅灰）。
    pub bg_dark: egui::Color32,
    /// 弹层/卡片/菜单底色（比主底亮/白一档，分出层级）。
    pub bg_panel: egui::Color32,
    /// 控件平时底色（按钮/下拉/滑轨）。比主底再拉开一档——M3.7b
    /// 核心诉求「按钮要明显」：平时即可见，不再与面板底糊成一片。
    pub btn_bg: egui::Color32,
    /// 悬停高亮底色（hover 档），兼作分隔线/描边灰。
    pub bg_highlight: egui::Color32,
    /// 主文字色（深色板=白，浅色板=近黑）。
    pub fg: egui::Color32,
    /// 次要文字色（中性灰，对主底 ≥4.5:1）。
    pub fg_dim: egui::Color32,
    /// 强调色（焦点窗格边框/未读点/链接/CTA 实底）：深色板=纯白，
    /// 浅色板=近黑——Warp 式主按钮「白底黑字」即 accent + accent_fg。
    pub accent: egui::Color32,
    /// 实底按钮（accent / error 填充）上的文字色，与填充明度相反
    /// （深色板=近黑字配白底/亮红底，浅色板=白字配近黑底/深红底）。
    pub accent_fg: egui::Color32,
    /// 激活/选中底色（列表选中、按下控件、文字选区）：控件梯度的
    /// 最高档（btn_bg → bg_highlight → selection）。
    pub selection: egui::Color32,
    /// 文件树栏底色（与 tab 侧栏差一线，区分两栏）。
    pub filetree_fill: egui::Color32,
    /// 输入框等的极暗/极亮底色。
    pub extreme_bg: egui::Color32,
    /// 警示文字色（语义黄，保留彩色；设置页字体回退提示等）。
    pub warn: egui::Color32,
    /// 错误文字色（语义红，保留彩色；登录校验失败红字等）。
    pub error: egui::Color32,
    /// 信息提示色（toast Info）：M3.7b 起中性白灰/深灰（原青色去彩）。
    pub info: egui::Color32,
}

/// 深色色板（纯黑白灰阶，Warp 深色观感）。
///
/// 对比度按 WCAG 相对亮度粗算并逐项标注（核心控件均 ≥4.5:1）。
pub static DARK: Palette = Palette {
    light: false,
    // #161616 主底：顶栏/侧栏/设置页近黑。
    bg_dark: egui::Color32::from_rgb(0x16, 0x16, 0x16),
    // #232323 弹层/卡片/菜单底。
    bg_panel: egui::Color32::from_rgb(0x23, 0x23, 0x23),
    // #2a2a2a 控件平时底（对 #161616 拉开一档，按钮平时即可见）。
    btn_bg: egui::Color32::from_rgb(0x2a, 0x2a, 0x2a),
    // #383838 悬停提亮档 / 分隔线、描边灰。
    bg_highlight: egui::Color32::from_rgb(0x38, 0x38, 0x38),
    // #ececec 主文字（白）：对 #161616 约 15:1、对 #2a2a2a 约 12:1。
    fg: egui::Color32::from_rgb(0xec, 0xec, 0xec),
    // #9e9e9e 次要文字：对 #161616 约 6.8:1、对 #2a2a2a 约 5.4:1。
    fg_dim: egui::Color32::from_rgb(0x9e, 0x9e, 0x9e),
    // #ffffff 纯白强调：焦点窗格边框/未读点/链接/CTA 白底。
    accent: egui::Color32::WHITE,
    // #111111 白底 CTA 上的黑字（约 18:1）；error 红底上约 7:1。
    accent_fg: egui::Color32::from_rgb(0x11, 0x11, 0x11),
    // #4a4a4a 激活/选中/文字选区（梯度 2a→38→4a 递亮）；fg 对其约 7.9:1。
    selection: egui::Color32::from_rgb(0x4a, 0x4a, 0x4a),
    // #1b1b1b 文件树栏（与侧栏 #161616 分一线）。
    filetree_fill: egui::Color32::from_rgb(0x1b, 0x1b, 0x1b),
    // #0e0e0e 输入框底（比主底更深，凹陷感）。
    extreme_bg: egui::Color32::from_rgb(0x0e, 0x0e, 0x0e),
    // #e0af68 语义黄保留（Tokyo Night 同源）。
    warn: egui::Color32::from_rgb(0xe0, 0xaf, 0x68),
    // #f7768e 语义红保留（Tokyo Night 同源）。
    error: egui::Color32::from_rgb(0xf7, 0x76, 0x8e),
    // #d6d6d6 toast Info 中性白灰（原青 #7dcfff 去彩）。
    info: egui::Color32::from_rgb(0xd6, 0xd6, 0xd6),
};

/// 浅色色板（深色板的白底黑字反转：近黑强调、控件「越深越明显」，
/// 梯度方向与深色板相反）。
pub static LIGHT: Palette = Palette {
    light: true,
    // #e6e6e6 主底：顶栏/侧栏/设置页浅灰。
    bg_dark: egui::Color32::from_rgb(0xe6, 0xe6, 0xe6),
    // #f5f5f5 弹层/卡片（比主底更白，浮起层级）。
    bg_panel: egui::Color32::from_rgb(0xf5, 0xf5, 0xf5),
    // #d9d9d9 控件平时底（对 #e6e6e6 拉开一档）。
    btn_bg: egui::Color32::from_rgb(0xd9, 0xd9, 0xd9),
    // #cccccc 悬停加深档 / 分隔线、描边灰。
    bg_highlight: egui::Color32::from_rgb(0xcc, 0xcc, 0xcc),
    // #1a1a1a 主文字（近黑）：对 #e6e6e6 约 13.9:1。
    fg: egui::Color32::from_rgb(0x1a, 0x1a, 0x1a),
    // #5a5a5a 次要文字：对 #e6e6e6 约 5.5:1、对 #f5f5f5 约 6.3:1。
    fg_dim: egui::Color32::from_rgb(0x5a, 0x5a, 0x5a),
    // #1f1f1f 近黑强调：CTA 深底白字（= 深色板白底黑字的反转）。
    accent: egui::Color32::from_rgb(0x1f, 0x1f, 0x1f),
    // #ffffff 深底 CTA 上的白字（约 16.5:1）；error 红底 #c64343 上约 4.9:1。
    accent_fg: egui::Color32::WHITE,
    // #bdbdbd 激活/选中/文字选区（梯度 d9→cc→bd 递深）；fg 对其约 9.3:1。
    selection: egui::Color32::from_rgb(0xbd, 0xbd, 0xbd),
    // #eeeeee 文件树栏（与侧栏 #e6e6e6 分一线）。
    filetree_fill: egui::Color32::from_rgb(0xee, 0xee, 0xee),
    // #fcfcfc 输入框底（比卡片再白一档）。
    extreme_bg: egui::Color32::from_rgb(0xfc, 0xfc, 0xfc),
    // #8c6c3e 语义黄保留（Tokyo Night day 同源，暗化保浅底可读）。
    warn: egui::Color32::from_rgb(0x8c, 0x6c, 0x3e),
    // #c64343 语义红保留（Tokyo Night day red1）。
    error: egui::Color32::from_rgb(0xc6, 0x43, 0x43),
    // #3c3c3c toast Info 中性深灰（原青 #07879d 去彩）。
    info: egui::Color32::from_rgb(0x3c, 0x3c, 0x3c),
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
///
/// M3.7b：visuals 从对应明暗的 egui 基准重建后逐项覆盖（不再克隆
/// 旧 visuals，避免深↔浅切换残留另一套派生色），egui 自带的几处
/// 蓝色默认值（hyperlink / 列表选中 / 文字选区 / 输入光标）全部
/// 改为本色板的中性灰阶。
pub fn apply_style(ctx: &egui::Context, pal: &Palette) {
    let mut style = (*ctx.global_style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.indent = 14.0;
    style.visuals = if pal.light {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };

    let v = &mut style.visuals;
    v.panel_fill = pal.bg_dark;
    v.window_fill = pal.bg_panel;
    v.extreme_bg_color = pal.extreme_bg;
    v.faint_bg_color = pal.bg_highlight;
    // 列表选中/文字选区底：中性灰（egui 默认为蓝）。
    v.selection.bg_fill = pal.selection;
    // 输入框聚焦边框（TextEdit has_focus 取 selection.stroke）：主文字色。
    v.selection.stroke = egui::Stroke::new(1.0, pal.fg);
    // 链接色随强调色走白/近黑（egui 默认为蓝）。
    v.hyperlink_color = pal.accent;
    // 文本输入光标：egui 默认浅蓝（深）/深蓝（浅），改主文字色。
    v.text_cursor.stroke = egui::Stroke::new(2.0, pal.fg);
    // egui 内部警示/错误文字与本色板语义色对齐。
    v.warn_fg_color = pal.warn;
    v.error_fg_color = pal.error;
    v.window_corner_radius = egui::CornerRadius::same(10);
    v.window_stroke = egui::Stroke::new(1.0, pal.bg_highlight);

    // 控件状态梯度（核心诉求「按钮要明显」）：平时 btn_bg → 悬停
    // bg_highlight → 按下/选中 selection，深色逐级递亮、浅色递深。
    let corner = egui::CornerRadius::same(6);
    v.widgets.noninteractive.corner_radius = corner;
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, pal.fg);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, pal.bg_highlight);
    v.widgets.inactive.corner_radius = corner;
    v.widgets.inactive.weak_bg_fill = pal.btn_bg;
    // bg_fill 同步走灰阶（滑块轨道/勾选框底等非弱填充控件）。
    v.widgets.inactive.bg_fill = pal.btn_bg;
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, pal.fg);
    v.widgets.hovered.corner_radius = corner;
    v.widgets.hovered.weak_bg_fill = pal.bg_highlight;
    v.widgets.hovered.bg_fill = pal.bg_highlight;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, pal.fg_dim);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.5, pal.fg);
    v.widgets.active.corner_radius = corner;
    v.widgets.active.weak_bg_fill = pal.selection;
    v.widgets.active.bg_fill = pal.selection;
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, pal.fg_dim);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.5, pal.fg);
    v.widgets.open.corner_radius = corner;
    v.widgets.open.weak_bg_fill = pal.bg_highlight;
    v.widgets.open.bg_fill = pal.bg_panel;
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
