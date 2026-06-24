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
//! # 经典模式按钮视觉规格（第十五轮对齐 topbar 按钮语言）
//! - 圆角矩形，圆角 4px（rounding = 4.0）
//! - 关闭态：**常态无边框无底色**（纯文字 fg_dim），hover 才画 bg_highlight 圆角底
//!   （与 topbar 三视图/窗控按钮语言一致：常态无边框 + hover 圆角底）
//! - 开启态（force_fallback=true）：accent 白底 + accent_fg 黑字（醒目反转 CTA，P9 风格）；
//!   无额外描边，圆角 4
//! - hover 文字色：关闭态 hover 时提亮为 fg；开启态始终 accent_fg
//! - 文字字号 11，垂直居中
//!
//! # 垂直居中规格（第十五轮问题2 + 海风哥两轮压线反馈）
//! - 状态栏高度 28px；按钮高度 18px，满高 cell + `from_center_size` 显式垂直居中 → 上下各 5px
//! - 第十五轮曾依赖 horizontal(Align::Center) 自动居中 allocate_exact_size(btn_h)，
//!   但实测未生效（按钮贴顶压上边描边线）；改为显式 from_center_size 强制居中，
//!   并将栏高 24→28px 增大间距（24px 下 3px 仍显挤、贴线）
//!
//! # 设计原则
//! - 状态栏高度常驻（不影响 footer 的 resize 链，两者独立）
//! - 切换按钮与 Ctrl+Shift+E 完全同路径（复用 dispatch ToggleFallback）
//! - 全部文案走 i18n 三语

use super::theme::Palette;
use crate::mode::InputMode;

/// 底部状态栏高度（逻辑像素）。
///
/// 28px（第十五轮 24px + 海风哥两轮压线反馈 +4px）：24px 下按钮 18px 居中仅
/// 上下各 3px，视觉仍挤、贴线；加高到 28px 后间距 5px，呼吸感足。
pub const HEIGHT: f32 = 28.0;

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
/// * `transfer` - 控制端活跃文件传输（Some 时中间区改画进度 ↓N↑M + 进度条 + 轮换文件名，
///   替代 cwd；None 时照常显示 cwd）。
/// * `pal` - 当前主题色板。
///
/// # Errors
/// 本函数仅做 egui 绘制，无 IO，不返回 Result。
pub fn show(
    root: &mut egui::Ui,
    mode: InputMode,
    cwd: Option<&std::path::Path>,
    force_fallback: bool,
    transfer: Option<&crate::remote_ws::TransferStatus>,
    pal: &Palette,
) -> StatusBarOutput {
    let mut out = StatusBarOutput::default();
    let s = crate::i18n::strings();

    // panel 内容区矩形（在 horizontal 布局之前捕获）——按钮垂直定位用它的中心。
    // 四轮踩坑后确定：horizontal 闭包内的 ui.max_rect()/available_height() 在本
    // 上下文都不是全栏高，导致按钮被顶在栏顶压线；这个外层 rect 才是可靠的全栏矩形。
    let panel_rect = root.available_rect_before_wrap();

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

        // 中间区统一先占位拿矩形，再按「有活跃传输 → 画进度」/「否则 → 画 cwd」分支。
        let (mid_rect, mid_resp) = ui.allocate_exact_size(
            egui::vec2(cwd_w, ui.available_height()),
            egui::Sense::hover(),
        );
        if let Some(t) = transfer {
            // 活跃传输：方向计数 + 下载进度条 + 轮换在传文件名（替代 cwd）。
            paint_transfer(ui, mid_rect, t, pal);
        } else if let Some(path) = cwd {
            // cwd 显示：尾目录名（截断时可 hover 看全路径）
            let display = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let full_path = path.display().to_string();
            ui.painter().text(
                egui::pos2(mid_rect.min.x, mid_rect.center().y),
                egui::Align2::LEFT_CENTER,
                &display,
                egui::FontId::proportional(11.0),
                pal.fg_dim,
            );
            if display != full_path {
                mid_resp.on_hover_text(full_path);
            }
        }

        // ── 右：经典直通切换按钮（第十五轮：对齐 topbar 按钮语言）────────
        // 按钮语言与 topbar 三视图/窗控按钮统一：
        //   关闭态：常态无边框无底（纯文字 fg_dim）；hover 画 bg_highlight 圆角底 + 文字提亮为 fg
        //   开启态：accent 白底 + accent_fg 黑字（醒目反转 CTA）；无额外描边
        //
        // 垂直居中修复：allocate_exact_size 只分配 btn_h（18px），
        // horizontal(Align::Center) 自动垂直居中 → 上下各 3px，满足与上边框 ≥2px 间距。
        // （旧版分配 ui.available_height()=24px，感知区从栏顶开始，视觉零间距。）

        // 按钮高度（18px）：在 28px 栏内垂直居中，上下各 (HEIGHT-btn_h)/2 = 5px 间距。
        let btn_h = 18.0_f32;
        // 按钮宽度：与上方 btn_w_approx 公式一致（ASCII 7px + CJK 12px，加 12px 内边距）。
        let btn_w = btn_text_w + 12.0;

        // 垂直定位（四轮踩坑后确定方案）：用 horizontal 外捕获的 panel_rect 中心
        // 绝对定位按钮，彻底绕开 egui horizontal 的垂直对齐 / max_rect /
        // available_height（三者实测都把按钮顶在栏顶）。allocate 仅占住水平位置
        // 拿 x；感知区用 ui.interact(btn_rect) 与实际绘制矩形对齐。
        let (cell_rect, _) = ui.allocate_exact_size(egui::vec2(btn_w, btn_h), egui::Sense::hover());
        let btn_rect = egui::Rect::from_center_size(
            egui::pos2(cell_rect.center().x, panel_rect.center().y),
            egui::vec2(btn_w, btn_h),
        );
        let resp = ui
            .interact(
                btn_rect,
                ui.id().with("statusbar_classic_btn"),
                egui::Sense::click(),
            )
            .on_hover_text(s.statusbar_classic_tip);

        if ui.is_rect_visible(btn_rect) {
            let painter = ui.painter();
            let rounding = egui::CornerRadius::same(4);

            // 背景填充：开启态 accent 白底；关闭态 hover 时 bg_highlight；否则透明
            let fill = if force_fallback {
                pal.accent
            } else if resp.hovered() {
                pal.bg_highlight
            } else {
                egui::Color32::TRANSPARENT
            };
            if fill != egui::Color32::TRANSPARENT {
                painter.rect_filled(btn_rect, rounding, fill);
            }

            // 无常态描边（统一 topbar 风格：常态无边框）
            // 开启态白底自带视觉边界，无需额外描边

            // 文字色：开启态 accent_fg（黑字），关闭态 hover 提亮为 fg，常态 fg_dim
            let text_color = if force_fallback {
                pal.accent_fg
            } else if resp.hovered() {
                pal.fg
            } else {
                pal.fg_dim
            };
            // 文字垂直居中于 btn_rect（18px 内）
            painter.text(
                btn_rect.center(),
                egui::Align2::CENTER_CENTER,
                btn_text,
                egui::FontId::proportional(11.0),
                text_color,
            );
        }

        if resp.clicked() {
            out.toggle_fallback = true;
        }

        ui.add_space(8.0);
    });

    out
}

/// 在状态栏中间区 `rect` 内画文件传输进度（活跃传输时替代 cwd）：方向计数 `↓N ↑M` + 下载聚合
/// 进度条（`down_total` 已知时）+ 轮换的在传文件名（每 ~2s 换一个）。painter 裁到 `rect`，
/// 长文件名不溢出到右端按钮区。轮换 / 进度刷新靠每帧（传输期数据流持续重绘）+ 兜底 500ms 重绘。
fn paint_transfer(
    ui: &egui::Ui,
    rect: egui::Rect,
    t: &crate::remote_ws::TransferStatus,
    pal: &Palette,
) {
    let painter = ui.painter_at(rect); // 裁到中间区，防长名溢出按钮区。
    let cy = rect.center().y;
    // 方向计数（active()>0 保证至少一向非零）。
    let counts = match (t.downloads, t.uploads) {
        (d, 0) => format!("↓{d}"),
        (0, u) => format!("↑{u}"),
        (d, u) => format!("↓{d} ↑{u}"),
    };
    let count_rect = painter.text(
        egui::pos2(rect.min.x, cy),
        egui::Align2::LEFT_CENTER,
        &counts,
        egui::FontId::proportional(11.0),
        pal.accent,
    );
    let mut x = count_rect.max.x + 8.0;
    // 下载进度条（仅 total 已知时；上传不集中跟踪字节，只计数）。
    let bar_w = 60.0_f32;
    if let Some(ratio) = t.down_ratio() {
        if x + bar_w <= rect.max.x {
            let bar_h = 4.0_f32;
            let cr = egui::CornerRadius::same(2);
            let track =
                egui::Rect::from_min_size(egui::pos2(x, cy - bar_h / 2.0), egui::vec2(bar_w, bar_h));
            painter.rect_filled(track, cr, pal.bg_highlight);
            let fill =
                egui::Rect::from_min_size(track.min, egui::vec2(bar_w * ratio, bar_h));
            painter.rect_filled(fill, cr, pal.accent);
            x += bar_w + 8.0;
        }
    }
    // 轮换在传文件名（每 ~2s 一个；egui 时间驱动）。
    if !t.names.is_empty() && x < rect.max.x {
        let now = ui.input(|i| i.time);
        let idx = ((now / 2.0) as usize) % t.names.len();
        painter.text(
            egui::pos2(x, cy),
            egui::Align2::LEFT_CENTER,
            &t.names[idx],
            egui::FontId::proportional(11.0),
            pal.fg_dim,
        );
    }
    // 传输期保持轮播 / 进度平滑刷新（块间无其它重绘时兜底）。
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(500));
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

        let mut v = ComposerView::compose_empty();
        v.placeholder = Some("占位提示".to_owned());

        // 空行（第一行为空字符串）：应显示 placeholder
        let all_empty = v.lines.iter().all(|l| l.is_empty());
        assert!(all_empty, "空编辑器的 lines 应全为空串");

        // 非空：不显示 placeholder
        v.lines[0] = "ls".to_owned();
        let all_empty = v.lines.iter().all(|l| l.is_empty());
        assert!(!all_empty, "非空编辑器 lines 不应全为空串");
    }

    // ── 按钮垂直居中几何纯函数测试（第十五轮 #2 + 海风哥两轮压线反馈）─────────
    // 钉住「状态栏高 28px / 按钮高 18px → 上下各 5px 间距」的数学约束。
    // 实现用满高 cell + from_center_size 显式居中（非 egui 自动居中，后者实测未生效）；
    // center_y_offset 是该居中的纯数学定义（无 egui 依赖）。

    /// 计算 horizontal(Align::Center) 布局中，在高度为 `bar_h` 的区域里
    /// 分配高度为 `btn_h` 的元素时，元素顶边相对于区域顶边的 y 偏移。
    ///
    /// 这是 egui Align::Center 的数学定义（无 egui 依赖的纯函数）。
    fn center_y_offset(bar_h: f32, btn_h: f32) -> f32 {
        (bar_h - btn_h) / 2.0
    }

    #[test]
    fn 按钮垂直居中_栏高28_按钮高18_偏移5px() {
        let offset = center_y_offset(HEIGHT, 18.0);
        assert_eq!(
            offset, 5.0,
            "状态栏高 {HEIGHT}px / 按钮高 18px 应居中，上下各 5px 间距，实际 {offset}px"
        );
    }

    #[test]
    fn 按钮垂直居中_间距不小于2px() {
        // 约束：无论按钮高度如何，上下间距必须 ≥2px
        let btn_h = 18.0_f32;
        let offset = center_y_offset(HEIGHT, btn_h);
        assert!(
            offset >= 2.0,
            "与上边框间距 {offset}px < 2px（按钮高 {btn_h}px，栏高 {HEIGHT}px）"
        );
        // 下边距等于上边距（对称居中）
        let gap_bottom = HEIGHT - btn_h - offset;
        assert!(
            gap_bottom >= 2.0,
            "与下边距 {gap_bottom}px < 2px（对称居中验证）"
        );
    }

    #[test]
    fn 按钮垂直居中_按钮在栏内不溢出() {
        let btn_h = 18.0_f32;
        let offset = center_y_offset(HEIGHT, btn_h);
        assert!(
            offset >= 0.0 && offset + btn_h <= HEIGHT,
            "按钮（offset={offset}, h={btn_h}）溢出状态栏（高 {HEIGHT}）"
        );
    }
}
