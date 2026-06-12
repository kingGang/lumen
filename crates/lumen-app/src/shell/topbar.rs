//! 顶栏（M3.5 / M3.8 / 问题1 修复）：左侧激活会话标题（兼拖动区）、
//! 右侧自绘窗控三按钮（最小化/最大化还原/关闭）、三视图切换按钮组、
//! 「＋」新增窗格与圆形头像按钮下拉菜单。
//!
//! 问题1 修复（RTL 布局 cursor().min bug）：
//! - 原实现用 `allocate_rect(Rect::from_min_size(cursor().min, …))` 手工构造矩形，
//!   RTL 布局下 cursor().min 是剩余区域左上角而非当前放置位置，导致三个窗控
//!   按钮全部叠画在面板左端、顶栏内容消失。
//! - 修复：一律用 `allocate_exact_size(vec2(W, H), Sense::click())` 让布局引擎
//!   自动放置（RTL 下靠右排、左移 cursor），painter 按返回 rect 画图。
//!
//! 问题7（三视图切换按钮）：
//! - 在「＋」左侧新增三按钮组：①显示/隐藏会话栏 ②显示/隐藏文件树（复用 Ctrl+B）
//!   ③还原窗格大小（原顶栏「▦」功能迁入）。
//! - 原「▦」按钮删除（功能已迁到③）。
//! - 图标 painter 画线风格（项目惯例）。
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
/// 视图切换按钮尺寸（逻辑像素，问题7）。
const VIEW_BTN: f32 = 26.0;
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
    /// 点击了③还原窗格大小按钮（原「▦」功能，问题7迁入）。
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
    // ── 问题7：三视图切换信号 ─────────────────────────────────────────
    /// 切换会话栏显示/隐藏（点击①按钮）。None = 未点击，Some(v) = 新可见值。
    pub toggle_sidebar: Option<bool>,
    /// 切换文件树显示/隐藏（点击②按钮，与 Ctrl+B 同状态源）。None = 未点击，Some(v) = 新可见值。
    pub toggle_filetree: Option<bool>,
}

/// 顶栏三视图切换按钮的可见态（问题7）：将两个 bool 打包传入 [`show`]，
/// 避免参数列表超过 clippy 7 参数限制。
pub struct ViewState {
    /// 会话栏（①）当前是否可见。
    pub sidebar_visible: bool,
    /// 文件树（②）当前是否可见（与 Ctrl+B 同状态源）。
    pub filetree_visible: bool,
}

/// 绘制顶栏（全宽窄条；须先于侧栏加入面板布局才能横贯整窗）。
///
/// # 参数
/// - `title`：激活会话标题，与窗口标题同源（main 的 display_title）。
/// - `pane_count`：激活 tab 当前窗格数（「＋」按钮满额禁用判定）。
/// - `is_maximized`：窗口当前是否最大化（切换最大化/还原图标）。
/// - `view`：三视图切换按钮的当前可见态（①会话栏 / ②文件树）。
pub fn show(
    root: &mut egui::Ui,
    title: &str,
    pane_count: usize,
    profile: Option<&Profile>,
    pal: &Palette,
    is_maximized: bool,
    view: ViewState,
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
            // right_to_left 布局：最右端是窗控三按钮，其左是头像/三视图按钮/＋，
            // 余下空白是拖动区 + 左侧标题文字。
            //
            // 关键：一律用 allocate_exact_size(vec2(W, H), Sense::click()) 让布局
            // 引擎自动放置——RTL 下布局引擎从右向左分配，自动靠右排列并左移
            // cursor，painter 按返回 rect 画图，不依赖 cursor().min。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let s = i18n::strings();

                // ── 窗控三按钮（最右，从右到左：关闭 → 最大化/还原 → 最小化）
                // ──────────────────────────────────────────────────────────────

                // 关闭按钮（悬停红底 #c42b1c 白字，Win11 惯例）
                let (close_rect, close_resp) =
                    ui.allocate_exact_size(egui::vec2(WC_BTN_W, HEIGHT), egui::Sense::click());
                {
                    let painter = ui.painter();
                    let c = close_rect.center();
                    if close_resp.hovered() {
                        // 悬停红底（Win11 关闭按钮惯例色 #c42b1c）
                        painter.rect_filled(
                            close_rect,
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
                let (maxrst_rect, maxrst_resp) =
                    ui.allocate_exact_size(egui::vec2(WC_BTN_W, HEIGHT), egui::Sense::click());
                {
                    let painter = ui.painter();
                    let c = maxrst_rect.center();
                    if maxrst_resp.hovered() {
                        painter.rect_filled(maxrst_rect, 0.0, pal.bg_highlight);
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
                out.maximize_btn_rect = Some(maxrst_rect);

                // 最小化按钮
                let (min_rect, min_resp) =
                    ui.allocate_exact_size(egui::vec2(WC_BTN_W, HEIGHT), egui::Sense::click());
                {
                    let painter = ui.painter();
                    let c = min_rect.center();
                    if min_resp.hovered() {
                        painter.rect_filled(min_rect, 0.0, pal.bg_highlight);
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
                ui.add_space(4.0);

                // ── 三视图切换按钮组（问题7，「＋」左侧）─────────────────
                // 按钮顺序（RTL：从右向左）：③还原 → ②文件树 → ①会话栏
                // 画线风格（项目惯例），激活态 pal.fg，隐藏态 pal.fg_dim。

                // ③ 还原窗格大小（原「▦」功能，问题7迁入）
                // 单窗格时禁用（无多窗格比例可还原）。
                let (reset_rect, reset_resp) =
                    ui.allocate_exact_size(egui::vec2(VIEW_BTN, VIEW_BTN), egui::Sense::click());
                {
                    // 禁用态：pane_count <= 1
                    let enabled = pane_count > 1;
                    let fg = if !enabled {
                        pal.fg_dim.gamma_multiply(0.4)
                    } else if reset_resp.hovered() {
                        pal.fg
                    } else {
                        pal.fg_dim
                    };
                    if reset_resp.hovered() && enabled {
                        ui.painter().rect_filled(reset_rect, 3.0, pal.bg_highlight);
                    }
                    // 四格 ▦ 图标（沿用既有画法）
                    let c = reset_rect.center();
                    let s2 = 3.5;
                    let gap = 1.5;
                    let stroke = egui::Stroke::new(1.1, fg);
                    for dx in [-1.0f32, 1.0] {
                        for dy in [-1.0f32, 1.0] {
                            let ox = c.x + dx * (s2 + gap / 2.0);
                            let oy = c.y + dy * (s2 + gap / 2.0);
                            ui.painter().rect_stroke(
                                egui::Rect::from_center_size(
                                    egui::pos2(ox, oy),
                                    egui::vec2(s2 * 2.0, s2 * 2.0),
                                ),
                                0.0,
                                stroke,
                                egui::StrokeKind::Middle,
                            );
                        }
                    }
                }
                let tip3 = if pane_count > 1 {
                    s.topbar_reset_layout_tip
                } else {
                    s.topbar_reset_layout_disabled_tip
                };
                if reset_resp.on_hover_text(tip3).clicked() && pane_count > 1 {
                    out.reset_layout = true;
                }

                ui.add_space(2.0);

                // ② 显示/隐藏文件树（Ctrl+B 同状态源，复用 filetree.visible）
                let (ft_rect, ft_resp) =
                    ui.allocate_exact_size(egui::vec2(VIEW_BTN, VIEW_BTN), egui::Sense::click());
                {
                    // 激活态（文件树可见）用 fg，隐藏态用 fg_dim
                    let fg = if view.filetree_visible {
                        pal.fg
                    } else {
                        pal.fg_dim
                    };
                    if ft_resp.hovered() {
                        ui.painter().rect_filled(ft_rect, 3.0, pal.bg_highlight);
                    }
                    // 图标②：方框+内部两短横线（列表意象）
                    let c = ft_rect.center();
                    let r = 4.5;
                    let stroke = egui::Stroke::new(1.1, fg);
                    // 外框
                    ui.painter().rect_stroke(
                        egui::Rect::from_center_size(c, egui::vec2(r * 2.0, r * 2.0)),
                        0.0,
                        stroke,
                        egui::StrokeKind::Middle,
                    );
                    // 内部两短横线
                    let line_x0 = c.x - r * 0.55;
                    let line_x1 = c.x + r * 0.65;
                    let stroke_inner = egui::Stroke::new(1.0, fg);
                    ui.painter().line_segment(
                        [
                            egui::pos2(line_x0, c.y - r * 0.35),
                            egui::pos2(line_x1, c.y - r * 0.35),
                        ],
                        stroke_inner,
                    );
                    ui.painter().line_segment(
                        [
                            egui::pos2(line_x0, c.y + r * 0.35),
                            egui::pos2(line_x1, c.y + r * 0.35),
                        ],
                        stroke_inner,
                    );
                }
                let tip2 = if view.filetree_visible {
                    s.topbar_filetree_hide_tip
                } else {
                    s.topbar_filetree_show_tip
                };
                if ft_resp.on_hover_text(tip2).clicked() {
                    out.toggle_filetree = Some(!view.filetree_visible);
                }

                ui.add_space(2.0);

                // ① 显示/隐藏会话栏
                let (sb_rect, sb_resp) =
                    ui.allocate_exact_size(egui::vec2(VIEW_BTN, VIEW_BTN), egui::Sense::click());
                {
                    // 激活态（侧栏可见）用 fg，隐藏态用 fg_dim
                    let fg = if view.sidebar_visible {
                        pal.fg
                    } else {
                        pal.fg_dim
                    };
                    if sb_resp.hovered() {
                        ui.painter().rect_filled(sb_rect, 3.0, pal.bg_highlight);
                    }
                    // 图标①：方框+左竖实条
                    let c = sb_rect.center();
                    let r = 4.5;
                    let stroke = egui::Stroke::new(1.1, fg);
                    // 外框
                    ui.painter().rect_stroke(
                        egui::Rect::from_center_size(c, egui::vec2(r * 2.0, r * 2.0)),
                        0.0,
                        stroke,
                        egui::StrokeKind::Middle,
                    );
                    // 左侧实条（竖线）
                    let bar_x = c.x - r * 0.5;
                    let stroke_bar = egui::Stroke::new(2.5, fg);
                    ui.painter().line_segment(
                        [
                            egui::pos2(bar_x, c.y - r * 0.7),
                            egui::pos2(bar_x, c.y + r * 0.7),
                        ],
                        stroke_bar,
                    );
                }
                let tip1 = if view.sidebar_visible {
                    s.topbar_sidebar_hide_tip
                } else {
                    s.topbar_sidebar_show_tip
                };
                if sb_resp.on_hover_text(tip1).clicked() {
                    out.toggle_sidebar = Some(!view.sidebar_visible);
                }

                ui.add_space(4.0);

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

#[cfg(test)]
mod topbar_layout_tests {
    use super::*;
    use crate::shell::theme;

    /// RTL 布局哨兵（fb2610a 修复回归防线）：无头 egui 跑一帧顶栏布局，
    /// 断言窗控按钮分配在面板右端——cursor().min 类手工矩形 bug 重现时此测试失败。
    #[test]
    fn 顶栏_最大化按钮分配在右端() {
        let ctx = egui::Context::default();
        let info = lumen_renderer::themes::find_or_default("lumen-dark");
        let pal = theme::shell_palette(info);
        let mut got: Option<egui::Rect> = None;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1200.0, 700.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            let tb = show(
                ui,
                "诊断标题",
                1,
                None,
                &pal,
                false,
                ViewState {
                    sidebar_visible: true,
                    filetree_visible: true,
                },
            );
            got = Some(tb.maximize_btn_rect.unwrap_or(egui::Rect::NOTHING));
        });
        let r = got.expect("应跑过一帧");
        // 关闭按钮占最右 46px，最大化按钮应在其左：x ∈ [1200-92, 1200-46]
        assert!(
            r.max.x > 1100.0 && r.min.x > 1050.0,
            "最大化按钮不在右端：{r:?}"
        );
        assert!(r.height() > 0.0, "按钮矩形退化：{r:?}");
    }

    /// 绘制哨兵：一帧的绘制图元里右端区域（x>1050）应存在窗控按钮的
    /// 线段图元（✕/—/□ 画线）——按钮绘制被条件分支意外跳过时此测试失败。
    #[test]
    fn 顶栏_右端绘制图元存在() {
        let ctx = egui::Context::default();
        let info = lumen_renderer::themes::find_or_default("lumen-dark");
        let pal = theme::shell_palette(info);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1200.0, 700.0),
            )),
            ..Default::default()
        };
        let full = ctx.run_ui(input, |ui| {
            let _ = show(
                ui,
                "诊断标题",
                1,
                None,
                &pal,
                false,
                ViewState {
                    sidebar_visible: true,
                    filetree_visible: true,
                },
            );
        });
        fn count_right_segments(shapes: &[egui::epaint::ClippedShape]) -> usize {
            let mut n = 0;
            for cs in shapes {
                n += walk(&cs.shape);
            }
            n
        }
        fn walk(s: &egui::epaint::Shape) -> usize {
            use egui::epaint::Shape;
            match s {
                Shape::LineSegment { points, .. } => {
                    usize::from(points[0].x > 1050.0 || points[1].x > 1050.0)
                }
                Shape::Vec(v) => v.iter().map(walk).sum(),
                _ => 0,
            }
        }
        let segs = count_right_segments(&full.shapes);
        // 窗控三按钮至少 4 条线段（✕ 两条 + — 一条 + □ 矩形另算）
        assert!(segs >= 3, "右端线段图元过少：{segs} 条——按钮没画");
    }
}
