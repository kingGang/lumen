//! 顶栏（M3.5 / M3.8）：左侧激活会话标题（兼拖动区）+ 右侧自绘窗控
//! 三按钮（最小化/最大化还原/关闭）+ 新增窗格 / 复位布局 / 圆形头像
//! 按钮与下拉菜单。
//!
//! M3.8 变更：
//! - 窗口无边框后，窗控三按钮并入顶栏右端（Warp/VSCode 形态）。
//! - 标题文字左侧空白区兼作拖动手柄（`drag_title_bar` 信号）。
//! - 双击空白区 toggle 最大化；右键空白区弹系统窗口菜单。
//! - 新参数 `is_maximized: bool` 控制最大化/还原图标切换。
//!
//! 规格（docs/M3应用外壳设计.md §4）：未登录头像为占位人形图标，已
//! 登录为强调色圆底 + 展示名首字母；点击弹下拉菜单——已登录：展示名
//! （灰字不可点）/ Settings / Keyboard shortcuts / Documentation
//! （灰显占位）/ 分隔线 / Log out；未登录：Log in / Settings /
//! Keyboard shortcuts。UI 只产出动作（[`TopbarOutput`]），登录/登出
//! 与设置页打开/增窗格/窗口操作由上层执行。

use crate::i18n;
use crate::profile::Profile;
use crate::session::MAX_PANES;

use super::theme::Palette;

/// 顶栏高度（逻辑像素）。加入后终端区高度变化走既有的
/// 「矩形变化 → 重建离屏纹理 + 全会话 resize」链路。
pub const HEIGHT: f32 = 34.0;
/// 窗控按钮热区宽度（逻辑像素，参考 Win11 约 46 × 34）。
const WC_BTN_W: f32 = 46.0;
/// 头像直径。
const AVATAR_SIZE: f32 = 24.0;
/// 下拉菜单宽度。
const MENU_WIDTH: f32 = 190.0;

/// 一帧顶栏 UI 的产出。
#[derive(Default)]
pub struct TopbarOutput {
    /// 点击了 Log in（打开登录覆盖层）。
    pub open_login: bool,
    /// 点击了 Settings（打开设置页）。
    pub open_settings: bool,
    /// 点击了 Keyboard shortcuts（打开设置页并定位该分类）。
    pub open_shortcuts: bool,
    /// 点击了 Log out。
    pub log_out: bool,
    /// 点击了「＋」：焦点 tab 内新增窗格（同 Ctrl+Shift+D，F5）。
    pub new_pane: bool,
    /// 点击了「▦」复位按钮（P15）：当前 tab 全部窗格比例恢复均分
    /// （处于最大化态时先退出再复位）。
    pub reset_layout: bool,
    // ── M3.8 窗口控制信号 ──────────────────────────────────────────────
    /// 拖动了顶栏空白区——main 调 window.drag_window()。
    pub drag_title_bar: bool,
    /// 最小化窗口。
    pub minimize_window: bool,
    /// 切换最大化/还原。
    pub toggle_maximize_window: bool,
    /// 关闭窗口——走 CloseRequested 同路径（落盘再退）。
    pub close_window: bool,
    /// 右键空白区弹系统窗口菜单，坐标为 egui 逻辑点。
    pub show_window_menu_at: Option<(f32, f32)>,
    /// 最大化/还原按钮本帧的 egui 逻辑坐标矩形（M3.8 批2 Snap Layouts
    /// 子类化用）：main 换算为屏幕物理像素后写入 snap_layouts 原子。
    /// 按钮不可见（极端情况）时为 None，main 跳过本帧更新。
    pub maximize_btn_rect: Option<egui::Rect>,
}

/// 绘制顶栏（全宽窄条；须先于侧栏加入面板布局才能横贯整窗）。
///
/// # 参数
/// - `title`：激活会话标题，与窗口标题同源（main 的 display_title）。
/// - `pane_count`：激活 tab 当前窗格数（「＋」按钮满额禁用判定）。
/// - `is_maximized`：窗口当前是否最大化（切换最大化/还原图标）。
pub fn show(
    root: &mut egui::Ui,
    title: &str,
    pane_count: usize,
    profile: Option<&Profile>,
    pal: &Palette,
    is_maximized: bool,
) -> TopbarOutput {
    let mut out = TopbarOutput::default();
    egui::Panel::top("lumen_topbar")
        .exact_size(HEIGHT)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(pal.bg_dark)
                .inner_margin(egui::Margin::symmetric(0, 0)),
        )
        .show_inside(root, |ui| {
            // right_to_left 布局：最右端是窗控三按钮，其左是头像/＋/▦，
            // 余下空白是拖动区 + 左侧标题文字。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // ── 窗控三按钮（最右，从右到左：关闭 → 最大化/还原 → 最小化）
                // ──────────────────────────────────────────────────────────────
                let s = i18n::strings();

                // 关闭按钮（悬停红底 #c42b1c 白字，Win11 惯例）
                let close_rect = ui.allocate_rect(
                    egui::Rect::from_min_size(ui.cursor().min, egui::vec2(WC_BTN_W, HEIGHT)),
                    egui::Sense::HOVER,
                );
                // 先 allocate 占位，再 interact 注册语义
                let close_resp = ui.interact(
                    close_rect.rect,
                    ui.id().with("wc_close"),
                    egui::Sense::click(),
                );
                {
                    let painter = ui.painter();
                    let c = close_rect.rect.center();
                    if close_resp.hovered() {
                        // 悬停红底（Win11 关闭按钮惯例色 #c42b1c）
                        painter.rect_filled(
                            close_rect.rect,
                            0.0,
                            egui::Color32::from_rgb(0xc4, 0x2b, 0x1c),
                        );
                    }
                    // ✕ 画线风格（不赌字体覆盖，与窗格标题栏 ✕ 同款）
                    let r = 4.5;
                    let fg = if close_resp.hovered() {
                        egui::Color32::WHITE
                    } else {
                        pal.fg_dim
                    };
                    let stroke = egui::Stroke::new(1.2, fg);
                    painter.line_segment(
                        [egui::pos2(c.x - r, c.y - r), egui::pos2(c.x + r, c.y + r)],
                        stroke,
                    );
                    painter.line_segment(
                        [egui::pos2(c.x - r, c.y + r), egui::pos2(c.x + r, c.y - r)],
                        stroke,
                    );
                }
                if close_resp.on_hover_text(s.wc_close).clicked() {
                    out.close_window = true;
                }

                // 最大化/还原按钮
                let maxrst_rect = ui.allocate_rect(
                    egui::Rect::from_min_size(ui.cursor().min, egui::vec2(WC_BTN_W, HEIGHT)),
                    egui::Sense::HOVER,
                );
                let maxrst_resp = ui.interact(
                    maxrst_rect.rect,
                    ui.id().with("wc_maxrst"),
                    egui::Sense::click(),
                );
                {
                    let painter = ui.painter();
                    let c = maxrst_rect.rect.center();
                    if maxrst_resp.hovered() {
                        painter.rect_filled(maxrst_rect.rect, 0.0, pal.bg_highlight);
                    }
                    let fg = if maxrst_resp.hovered() {
                        pal.fg
                    } else {
                        pal.fg_dim
                    };
                    let stroke = egui::Stroke::new(1.2, fg);
                    if is_maximized {
                        // 还原图标 ⧉：错位双矩形（与窗格最大化态图标同款画法）
                        let r = 3.0;
                        let back = egui::Rect::from_center_size(
                            c + egui::vec2(1.5, -1.5),
                            egui::vec2(2.0 * r, 2.0 * r),
                        );
                        painter.line_segment(
                            [
                                egui::pos2(back.min.x + 2.0, back.min.y),
                                egui::pos2(back.max.x, back.min.y),
                            ],
                            stroke,
                        );
                        painter.line_segment(
                            [
                                egui::pos2(back.max.x, back.min.y),
                                egui::pos2(back.max.x, back.max.y - 2.0),
                            ],
                            stroke,
                        );
                        let front = egui::Rect::from_center_size(
                            c + egui::vec2(-1.0, 1.0),
                            egui::vec2(2.0 * r, 2.0 * r),
                        );
                        painter.rect_stroke(front, 0.0, stroke, egui::StrokeKind::Middle);
                    } else {
                        // 最大化图标 □：单矩形描边
                        let r = 4.5;
                        painter.rect_stroke(
                            egui::Rect::from_center_size(c, egui::vec2(2.0 * r, 2.0 * r)),
                            0.0,
                            stroke,
                            egui::StrokeKind::Middle,
                        );
                    }
                }
                let tip = if is_maximized {
                    s.wc_restore
                } else {
                    s.wc_maximize
                };
                if maxrst_resp.on_hover_text(tip).clicked() {
                    out.toggle_maximize_window = true;
                }
                // M3.8 批2：记录最大化按钮的逻辑矩形，供 main 换算为
                // 屏幕物理像素后写入 snap_layouts 原子（WM_NCHITTEST 命中用）。
                out.maximize_btn_rect = Some(maxrst_rect.rect);

                // 最小化按钮
                let min_rect = ui.allocate_rect(
                    egui::Rect::from_min_size(ui.cursor().min, egui::vec2(WC_BTN_W, HEIGHT)),
                    egui::Sense::HOVER,
                );
                let min_resp = ui.interact(
                    min_rect.rect,
                    ui.id().with("wc_minimize"),
                    egui::Sense::click(),
                );
                {
                    let painter = ui.painter();
                    let c = min_rect.rect.center();
                    if min_resp.hovered() {
                        painter.rect_filled(min_rect.rect, 0.0, pal.bg_highlight);
                    }
                    let fg = if min_resp.hovered() {
                        pal.fg
                    } else {
                        pal.fg_dim
                    };
                    // 「—」横线
                    painter.line_segment(
                        [egui::pos2(c.x - 5.0, c.y), egui::pos2(c.x + 5.0, c.y)],
                        egui::Stroke::new(1.5, fg),
                    );
                }
                if min_resp.on_hover_text(s.wc_minimize).clicked() {
                    out.minimize_window = true;
                }

                // ── 头像（紧贴窗控左侧，加右内边距 10px）──────────────────
                ui.add_space(10.0);
                let resp = avatar_button(ui, profile, pal);
                let _ = egui::Popup::menu(&resp)
                    .align(egui::RectAlign::BOTTOM_END)
                    .width(MENU_WIDTH)
                    .show(|ui| menu_ui(ui, profile, pal, &mut out));
                ui.add_space(6.0);
                // 「＋」新增窗格（F5）：满 MAX_PANES 时禁用 + 悬停提示。
                let plus =
                    egui::Button::new(egui::RichText::new("＋").size(15.0).color(pal.fg_dim))
                        .min_size(egui::vec2(AVATAR_SIZE, AVATAR_SIZE));
                let presp = ui
                    .add_enabled(pane_count < MAX_PANES, plus)
                    .on_hover_text(s.topbar_new_pane_tip)
                    .on_disabled_hover_text(i18n::fmt1(s.topbar_max_panes_fmt, MAX_PANES));
                if presp.clicked() {
                    out.new_pane = true;
                }
                ui.add_space(2.0);
                // 「▦」复位布局（P15）：当前 tab 窗格比例恢复均分；
                // 单窗格无比例可言，禁用态。
                let reset =
                    egui::Button::new(egui::RichText::new("▦").size(13.0).color(pal.fg_dim))
                        .min_size(egui::vec2(AVATAR_SIZE, AVATAR_SIZE));
                let rresp = ui
                    .add_enabled(pane_count > 1, reset)
                    .on_hover_text(s.topbar_reset_tip)
                    .on_disabled_hover_text(s.topbar_reset_disabled_tip);
                if rresp.clicked() {
                    out.reset_layout = true;
                }
                // ── 左侧标题 + 拖动区（right_to_left 余下空间）───────────
                // 用 left_to_right 子布局占满余下空间——仲裁规则：
                //   · drag_started_by(Primary)  → 发 drag_title_bar 信号
                //   · double_clicked()          → toggle 最大化
                //   · secondary_clicked()       → 弹系统窗口菜单
                // 标题文字本身只用 Label（selectable=false，不消费点击）；
                // interact 覆盖整个剩余区，含标题文字上方，egui Label
                // 不拦截拖动，仲裁互不干扰。
                let drag_area = ui.available_rect_before_wrap();
                // 标题文字（左侧 10px 内边距）
                {
                    let text_rect = egui::Rect::from_min_max(
                        egui::pos2(drag_area.min.x + 10.0, drag_area.min.y),
                        drag_area.max,
                    );
                    let mut title_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(text_rect)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    );
                    title_ui.add(
                        egui::Label::new(egui::RichText::new(title).size(12.0).color(pal.fg_dim))
                            .truncate()
                            .selectable(false),
                    );
                }
                // 拖动/双击/右键感知——覆盖整个余下空白区
                let drag_resp = ui.interact(
                    drag_area,
                    ui.id().with("topbar_drag"),
                    egui::Sense::click_and_drag(),
                );
                if drag_resp.drag_started_by(egui::PointerButton::Primary) {
                    out.drag_title_bar = true;
                }
                if drag_resp.double_clicked() {
                    out.toggle_maximize_window = true;
                }
                if drag_resp.secondary_clicked() {
                    if let Some(pos) = drag_resp.interact_pointer_pos() {
                        out.show_window_menu_at = Some((pos.x, pos.y));
                    }
                }
            });
        });
    out
}

/// 圆形头像按钮：已登录 = 强调色圆底 + 首字母；未登录 = 占位人形。
fn avatar_button(ui: &mut egui::Ui, profile: Option<&Profile>, pal: &Palette) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(AVATAR_SIZE, AVATAR_SIZE), egui::Sense::click());
    let center = rect.center();
    let radius = AVATAR_SIZE / 2.0;
    match profile {
        Some(p) => {
            // 已登录头像：accent 实底 + 反相首字母（深色主题白底黑字，
            // 浅色主题近黑底白字——M3.7b 去蓝，与 CTA 按钮同形态）。
            ui.painter().circle_filled(center, radius, pal.accent);
            ui.painter().text(
                center,
                egui::Align2::CENTER_CENTER,
                p.avatar_letter(),
                egui::FontId::proportional(13.0),
                pal.accent_fg,
            );
        }
        None => {
            ui.painter().circle_filled(center, radius, pal.bg_highlight);
            ui.painter().text(
                center,
                egui::Align2::CENTER_CENTER,
                "👤",
                egui::FontId::proportional(13.0),
                pal.fg_dim,
            );
        }
    }
    // 悬停反馈：外圈描边（圆形按钮没有 egui 默认的底色 hover 效果）。
    if resp.hovered() {
        ui.painter()
            .circle_stroke(center, radius, egui::Stroke::new(1.5, pal.fg_dim));
    }
    resp.on_hover_text(profile.map_or_else(
        || i18n::strings().topbar_not_logged_in.to_owned(),
        |p| p.email.clone(),
    ))
}

/// 下拉菜单内容（参照截图，按登录态二选一）。
fn menu_ui(ui: &mut egui::Ui, profile: Option<&Profile>, pal: &Palette, out: &mut TopbarOutput) {
    let s = i18n::strings();
    match profile {
        Some(p) => {
            // 首行展示名：灰字不可点（参照截图 Jimhy Liu 行）。
            ui.add_enabled(
                false,
                egui::Button::new(egui::RichText::new(&p.display_name).color(pal.fg_dim)),
            );
            if ui.button(s.menu_settings).clicked() {
                out.open_settings = true;
                ui.close();
            }
            if ui.button(s.menu_keyboard_shortcuts).clicked() {
                out.open_shortcuts = true;
                ui.close();
            }
            // Documentation 灰显占位（本期无文档站，§7 不做清单）。
            ui.add_enabled(false, egui::Button::new(s.menu_documentation));
            ui.separator();
            if ui.button(s.menu_log_out).clicked() {
                out.log_out = true;
                ui.close();
            }
        }
        None => {
            if ui.button(s.menu_log_in).clicked() {
                out.open_login = true;
                ui.close();
            }
            if ui.button(s.menu_settings).clicked() {
                out.open_settings = true;
                ui.close();
            }
            if ui.button(s.menu_keyboard_shortcuts).clicked() {
                out.open_shortcuts = true;
                ui.close();
            }
        }
    }
}
