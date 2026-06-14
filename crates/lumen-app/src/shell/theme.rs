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
/// 主题切换 = 换一个 [`Palette`]（[`shell_palette`] 按主题取手调
/// 静态板或派生板）+ [`apply_style`] 重设 egui 样式。
#[derive(Clone)]
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
    /// 三栏面板外轮廓描边色（P16）：深色板中亮灰（比 bg_highlight 再亮一
    /// 档），在近黑底上清晰可见；浅色板对应中深灰（比 bg_highlight 再深
    /// 一档）。色值独立于分隔线/悬停档，专门服务「面板边界轮廓」语义。
    pub panel_outline: egui::Color32,
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
    // #4a4a4a 面板外轮廓描边（P16b 调暗）：比初版 #626262 更沉，
    // 在近黑底（#161616）上对比约 2.8:1——装饰线不需全 AA，
    // 深色背景下这个亮度已明显可辨而不刺眼（「灰色」目标）。
    // 视觉层次：轮廓(4a) < 分隔线 bg_highlight(38) < 焦点 accent(白)。
    panel_outline: egui::Color32::from_rgb(0x4a, 0x4a, 0x4a),
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
    // #a0a0a0 面板外轮廓描边（P16b 调整）：浅色板方向——比初版 #888888
    // 略亮（更接近背景，更「低调的灰」），在浅灰底（#e6e6e6）上
    // 对比约 2.5:1——装饰线语义，视觉存在而不抢眼。
    panel_outline: egui::Color32::from_rgb(0xa0, 0xa0, 0xa0),
};

/// 取主题对应的外壳色板（P12 外壳联动）。
///
/// Lumen Dark/Light 两个默认主题用 P9 手调的 [`DARK`] / [`LIGHT`]
/// 静态板（保真，不走派生）；其余内置主题从终端配色的 bg/fg/ANSI
/// 按 [`derive_palette`] 的规则自动派生灰阶层次。
pub fn shell_palette(info: &lumen_renderer::themes::ThemeInfo) -> Palette {
    use lumen_renderer::themes::{LUMEN_DARK, LUMEN_LIGHT};
    if info.id == LUMEN_DARK {
        DARK.clone()
    } else if info.id == LUMEN_LIGHT {
        LIGHT.clone()
    } else {
        derive_palette(info.light, &info.theme())
    }
}

/// 从终端主题派生外壳色板（P12）。派生规则：
///
/// - **灰阶阶梯**以终端 bg 为基色向黑/白混合（保留主题底色的色相，
///   如 Nord 蓝灰、Solarized 暖黄）。深色板亮度梯：extreme_bg <
///   bg_dark < filetree_fill < bg_panel < btn_bg < bg_highlight <
///   selection（控件「平时→悬停→激活」递亮）；浅色板方向相反
///   （递暗）。混合比例见各行注释，单测保证单调性。
/// - **文字**跟随终端 fg：向白（深）/黑（浅）推进直到对 btn_bg
///   ≥7:1 且对 selection ≥4.5:1；fg_dim 取 fg 与 bg_dark 的混合再
///   保证对 bg_dark ≥4.5:1（WCAG AA，单测把关下限）。
/// - **accent 保持黑白化原则**（P9）：深色板纯白、浅色板近黑，不随
///   主题彩色化；accent_fg 与之明度相反。
/// - **语义色**取主题 ANSI 黄（warn）/红（error），不足 4.5:1 时向
///   白/黑提对比；info 为中性的 fg 微暗档。
fn derive_palette(light: bool, t: &lumen_renderer::Theme) -> Palette {
    let base = c32(t.background);
    let black = egui::Color32::BLACK;
    let white = egui::Color32::WHITE;
    if light {
        // 浅色：外壳比终端 bg 略暗一线（终端区成为画面最亮处），
        // 控件越深越显眼（与 LIGHT 手调板同方向）。
        let bg_dark = mix(base, black, 0.05);
        let extreme_bg = mix(base, white, 0.60); // 输入框最白
        let bg_panel = mix(base, white, 0.45); // 弹层/卡片浮起
        let filetree_fill = mix(base, white, 0.22); // 与侧栏分一线
        let btn_bg = mix(base, black, 0.12); // 控件平时
        let bg_highlight = mix(base, black, 0.20); // 悬停/分隔线
        let selection = mix(base, black, 0.29); // 激活/选中
        let fg = push_until(c32(t.foreground), black, |c| {
            contrast(c, btn_bg) >= 7.0 && contrast(c, selection) >= 4.5
        });
        let fg_dim = push_until(mix(fg, bg_dark, 0.35), fg, |c| contrast(c, bg_dark) >= 4.6);
        Palette {
            light: true,
            bg_dark,
            bg_panel,
            btn_bg,
            bg_highlight,
            fg,
            fg_dim,
            // 黑白化原则：浅色板近黑强调 + 白字（同 LIGHT 手调板）。
            accent: egui::Color32::from_rgb(0x1f, 0x1f, 0x1f),
            accent_fg: white,
            selection,
            filetree_fill,
            extreme_bg,
            warn: ensure_contrast(c32(t.ansi[3]), bg_dark, 4.5, black),
            error: ensure_contrast(c32(t.ansi[1]), bg_dark, 4.5, black),
            info: ensure_contrast(mix(fg, bg_dark, 0.12), bg_dark, 4.5, black),
            // 浅色派生（P16b）：比 bg_highlight 再深 0.20 档，
            // 与 LIGHT 手调板 #a0a0a0 量级保持一致（低调灰线）。
            panel_outline: mix(bg_dark, black, 0.20),
        }
    } else {
        // 深色：外壳比终端 bg 略暗（终端内容区微浮起），控件递亮。
        let extreme_bg = mix(base, black, 0.50); // 输入框最深（凹陷）
        let bg_dark = mix(base, black, 0.30);
        let filetree_fill = mix(base, black, 0.16);
        let bg_panel = mix(base, white, 0.04); // 弹层/卡片浮起
        let btn_bg = mix(base, white, 0.09); // 控件平时
        let bg_highlight = mix(base, white, 0.17); // 悬停/分隔线
        let selection = mix(base, white, 0.26); // 激活/选中
        let fg = push_until(c32(t.foreground), white, |c| {
            contrast(c, btn_bg) >= 7.0 && contrast(c, selection) >= 4.5
        });
        let fg_dim = push_until(mix(fg, bg_dark, 0.35), fg, |c| contrast(c, bg_dark) >= 4.6);
        Palette {
            light: false,
            bg_dark,
            bg_panel,
            btn_bg,
            bg_highlight,
            fg,
            fg_dim,
            // 黑白化原则：深色板纯白强调 + 近黑字（同 DARK 手调板）。
            accent: white,
            accent_fg: egui::Color32::from_rgb(0x11, 0x11, 0x11),
            selection,
            filetree_fill,
            extreme_bg,
            warn: ensure_contrast(c32(t.ansi[3]), bg_dark, 4.5, white),
            error: ensure_contrast(c32(t.ansi[1]), bg_dark, 4.5, white),
            info: ensure_contrast(mix(fg, bg_dark, 0.12), bg_dark, 4.5, white),
            // 深色派生（P16b）：比 bg_highlight 再亮 0.22 档，
            // 与 DARK 手调板 #4a4a4a 量级保持一致（低调灰线）。
            panel_outline: mix(bg_dark, white, 0.22),
        }
    }
}

/// 渲染器 Rgb → egui Color32。
pub fn c32(c: lumen_renderer::Rgb) -> egui::Color32 {
    egui::Color32::from_rgb(c.0, c.1, c.2)
}

/// sRGB 分量线性插值：`t=0` 取 `a`，`t=1` 取 `b`。
fn mix(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let f = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    egui::Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

/// WCAG 相对亮度（sRGB → 线性加权）。
fn rel_lum(c: egui::Color32) -> f32 {
    fn lin(v: u8) -> f32 {
        let s = f32::from(v) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// WCAG 对比度（≥1，越大对比越强）。
fn contrast(a: egui::Color32, b: egui::Color32) -> f32 {
    let (x, y) = (rel_lum(a), rel_lum(b));
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

/// 从 `from` 向 `toward` 按 1/40 步长推进，返回第一个满足 `ok` 的
/// 颜色；走满全程仍不满足（极端防御，内置主题不会触发）返回
/// `toward` 本身。
fn push_until(
    from: egui::Color32,
    toward: egui::Color32,
    ok: impl Fn(egui::Color32) -> bool,
) -> egui::Color32 {
    for i in 0..=40 {
        let cand = mix(from, toward, i as f32 / 40.0);
        if ok(cand) {
            return cand;
        }
    }
    toward
}

/// `c` 对底色 `bg` 的对比不足 `target` 时向 `toward` 推进补足。
fn ensure_contrast(
    c: egui::Color32,
    bg: egui::Color32,
    target: f32,
    toward: egui::Color32,
) -> egui::Color32 {
    push_until(c, toward, |x| contrast(x, bg) >= target)
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
    // 弹窗/菜单/窗口去半透明投影（海风哥 2026-06-14：弹窗背景不要半透明）。
    // egui 默认 window/popup_shadow 是 from_black_alpha(96) 的柔和黑投影，把弹窗
    // 周围终端内容压暗、显半透明观感；改无投影，靠上面 window_stroke 实色边框
    // 区隔（弹窗 body 本就是 window_fill=bg_panel 实色，无 alpha）。
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;

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

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_renderer::themes::{BUILTIN, LUMEN_DARK, LUMEN_LIGHT};

    /// 派生主题迭代器（排除走手调板的 Lumen 双主题）。
    fn derived() -> impl Iterator<Item = &'static lumen_renderer::themes::ThemeInfo> {
        BUILTIN
            .iter()
            .filter(|i| i.id != LUMEN_DARK && i.id != LUMEN_LIGHT)
    }

    #[test]
    fn 派生_深色亮度阶梯单调递增() {
        for info in derived().filter(|i| !i.light) {
            let p = shell_palette(info);
            let steps = [
                ("extreme_bg", p.extreme_bg),
                ("bg_dark", p.bg_dark),
                ("filetree_fill", p.filetree_fill),
                ("bg_panel", p.bg_panel),
                ("btn_bg", p.btn_bg),
                ("bg_highlight", p.bg_highlight),
                ("selection", p.selection),
            ];
            for w in steps.windows(2) {
                assert!(
                    rel_lum(w[0].1) < rel_lum(w[1].1),
                    "{}: {} 应暗于 {}",
                    info.id,
                    w[0].0,
                    w[1].0
                );
            }
        }
    }

    #[test]
    fn 派生_浅色亮度阶梯单调递减() {
        for info in derived().filter(|i| i.light) {
            let p = shell_palette(info);
            let steps = [
                ("extreme_bg", p.extreme_bg),
                ("bg_panel", p.bg_panel),
                ("filetree_fill", p.filetree_fill),
                ("bg_dark", p.bg_dark),
                ("btn_bg", p.btn_bg),
                ("bg_highlight", p.bg_highlight),
                ("selection", p.selection),
            ];
            for w in steps.windows(2) {
                assert!(
                    rel_lum(w[0].1) > rel_lum(w[1].1),
                    "{}: {} 应亮于 {}",
                    info.id,
                    w[0].0,
                    w[1].0
                );
            }
        }
    }

    #[test]
    fn 派生_对比度下限() {
        for info in derived() {
            let p = shell_palette(info);
            let cases = [
                // 主文字对主底/控件底/选中底（核心可读性）。
                ("fg vs bg_dark", contrast(p.fg, p.bg_dark), 7.0),
                ("fg vs btn_bg", contrast(p.fg, p.btn_bg), 7.0),
                ("fg vs selection", contrast(p.fg, p.selection), 4.5),
                // 次要文字与语义色（WCAG AA 下限）。
                ("fg_dim vs bg_dark", contrast(p.fg_dim, p.bg_dark), 4.5),
                ("warn vs bg_dark", contrast(p.warn, p.bg_dark), 4.5),
                ("error vs bg_dark", contrast(p.error, p.bg_dark), 4.5),
                ("info vs bg_dark", contrast(p.info, p.bg_dark), 4.5),
                // 实底按钮上的文字。
                ("accent_fg vs accent", contrast(p.accent_fg, p.accent), 4.5),
            ];
            for (name, got, min) in cases {
                assert!(got >= min, "{}: {name} 对比 {got:.2} < {min}", info.id);
            }
        }
    }

    #[test]
    fn lumen双主题走手调板不派生() {
        // P9/P16 手调值保真：与静态板逐项一致（抽查关键字段）。
        let d = shell_palette(lumen_renderer::themes::find_or_default(LUMEN_DARK));
        assert_eq!(d.bg_dark, DARK.bg_dark);
        assert_eq!(d.accent, DARK.accent);
        assert_eq!(d.selection, DARK.selection);
        assert_eq!(d.panel_outline, DARK.panel_outline);
        assert!(!d.light);
        let l = shell_palette(lumen_renderer::themes::find_or_default(LUMEN_LIGHT));
        assert_eq!(l.bg_dark, LIGHT.bg_dark);
        assert_eq!(l.accent, LIGHT.accent);
        assert_eq!(l.selection, LIGHT.selection);
        assert_eq!(l.panel_outline, LIGHT.panel_outline);
        assert!(l.light);
    }

    #[test]
    fn 派生_accent保持黑白化() {
        // P9 黑白化原则不随主题彩色化：深=纯白 / 浅=近黑。
        for info in derived() {
            let p = shell_palette(info);
            if p.light {
                assert_eq!(p.accent, egui::Color32::from_rgb(0x1f, 0x1f, 0x1f));
                assert_eq!(p.accent_fg, egui::Color32::WHITE);
            } else {
                assert_eq!(p.accent, egui::Color32::WHITE);
                assert_eq!(p.accent_fg, egui::Color32::from_rgb(0x11, 0x11, 0x11));
            }
        }
    }

    #[test]
    fn 工具函数_mix与contrast() {
        let black = egui::Color32::BLACK;
        let white = egui::Color32::WHITE;
        assert_eq!(mix(black, white, 0.0), black);
        assert_eq!(mix(black, white, 1.0), white);
        // 黑白对比 = WCAG 满格 21:1。
        assert!((contrast(black, white) - 21.0).abs() < 0.01);
        assert!((contrast(white, white) - 1.0).abs() < 0.01);
        // push_until：永不满足时回退 toward。
        assert_eq!(push_until(black, white, |_| false), white);
        assert_eq!(push_until(black, white, |c| c == black), black);
    }
}
