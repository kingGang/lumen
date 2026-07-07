//! 顶栏（M3.5 / M3.8 / 问题1 修复 / R8）：
//! - 左端（LTR）：①侧栏 ②文件树 ③还原窗格 三按钮（紧贴左缘，组前留 10px）
//! - 右端（RTL）：关闭 / 最大化还原 / 最小化 / 头像 / 「＋」
//! - 中间剩余空白：拖动区（drag / 双击 / 右键语义不变）
//! - 标题文字已删除（R8 海风哥点名去掉路径标题）
//!
//! 问题1 修复（RTL 布局 cursor().min bug）：
//! - 原实现用 `allocate_rect(Rect::from_min_size(cursor().min, …))` 手工构造矩形，
//!   RTL 布局下 cursor().min 是剩余区域左上角而非当前放置位置，导致三个窗控
//!   按钮全部叠画在面板左端、顶栏内容消失。
//! - 修复：一律用 `allocate_exact_size(vec2(W, H), Sense::click())` 让布局引擎
//!   自动放置（RTL 下靠右排、左移 cursor），painter 按返回 rect 画图。
//!
//! R8 图标精绘（统一 codicon 族系几何）：
//! - ①侧栏：圆角外框 18×14 + 左 1/3 竖分隔线；可见态左舱填充。
//! - ②文件树：竖干 + 三横枝树形（无外框，横枝长 9/6.5/4 层次分明）。
//! - ③还原窗格：圆角外框 16×14 + 内部十字分隔（田字）。
//!
//! 统一规格：线宽 1.2，热区 28×26，组左缘距 10，组内间距 4。
//!
//! R8.2 变更：图标视觉盒从 ~16×12 扩到 ~18×14；按钮间距 2→4；
//! 树形横枝长度差拉大为 9/6.5/4（高 DPI 下层次可辨）。
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
/// 视图切换按钮热区宽度 × 高度（逻辑像素，R8：28×26）。
const VIEW_BTN_W: f32 = 28.0;
const VIEW_BTN_H: f32 = 26.0;
/// 左端按钮组前缘内边距。
const LEFT_GROUP_MARGIN: f32 = 10.0;
/// 按钮组内间距（R8.2：2→4，增强分组感）。
const VIEW_BTN_GAP: f32 = 4.0;
/// 头像直径。
const AVATAR_SIZE: f32 = 24.0;
/// 下拉菜单宽度（set_min_width 强撑生效后 304 偏宽，海风哥反馈砍半）。
const MENU_WIDTH: f32 = 152.0;

/// 一帧顶栏 UI 的产出。
#[derive(Default)]
pub struct TopbarOutput {
    /// 点击了 Log in（打开登录覆盖层）。
    pub open_login: bool,
    /// 点击了 Settings（打开设置页）。
    pub open_settings: bool,
    /// 点击了 Keyboard shortcuts（打开设置页并定位该分类）。
    pub open_shortcuts: bool,
    /// 头像菜单：点击了「检查更新」（无就绪更新时）→ main 起手动检查。
    pub check_update: bool,
    /// 头像菜单：点击了「更新到 vX」（有就绪更新时）→ main 显示更新弹窗。
    pub open_update: bool,
    /// 头像菜单：点击了「更新日志」→ main 打开 GitHub Releases。
    pub open_whats_new: bool,
    /// 头像菜单：点击了「文档」→ main 打开 GitHub 仓库 README。
    pub open_documentation: bool,
    /// 头像菜单：点击了「反馈」→ main 打开 GitHub Issues。
    pub open_feedback: bool,
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
    // ── 三视图切换信号 ───────────────────────────────────────────────
    /// 切换会话栏显示/隐藏（点击①按钮）。None = 未点击，Some(v) = 新可见值。
    pub toggle_sidebar: Option<bool>,
    /// 切换文件树显示/隐藏（点击②按钮，与 Ctrl+B 同状态源）。None = 未点击，Some(v) = 新可见值。
    pub toggle_filetree: Option<bool>,
    /// 切换本地/远程视图（点击顶栏「本地/远程」tab，M5.2）。
    /// None = 未点击，Some(false) = 本地，Some(true) = 远程。
    pub toggle_view_mode: Option<bool>,
}

/// 顶栏额外状态（打包传入 [`show`]，避免参数列表超过 clippy 7 参数限制）。
pub struct ViewState {
    /// 会话栏（①）当前是否可见。
    pub sidebar_visible: bool,
    /// 文件树（②）当前是否可见（与 Ctrl+B 同状态源）。
    pub filetree_visible: bool,
    /// 头像菜单更新项：Some(版本号) = 有就绪更新（显示「更新到 vX」强调项），
    /// None = 无更新（显示「检查更新」）。
    pub update_version: Option<String>,
    /// 当前视图（M5.2）：false = 本地，true = 远程。
    pub current_view: bool,
    /// 登录态已过期需重新登录（token 过期）：头像叠红色感叹号角标 + 菜单出红字「登录过期」。
    /// main 据 `profile.token_expires_at` 判定（自动续期之外的兜底，如关闭 >7 天再开）。
    pub need_relogin: bool,
}

/// 绘制顶栏（全宽窄条；须先于侧栏加入面板布局才能横贯整窗）。
///
/// # 参数
/// - `title`：激活会话标题（R8 已不显示，仅保留参数兼容，未来可用于 OS 窗口标题）。
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
    let _ = title; // R8：不显示标题，保留参数供上层传 OS 窗口标题用
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
            // 布局策略（R8）：
            //   右端：RTL 布局分配窗控/头像/「＋」各按钮。
            //   左端：在余下空间内用 LTR 子布局放三视图按钮组。
            //   中间：剩余矩形作为拖动区（drag/双击/右键语义不变）。
            //
            // 关键：一律用 allocate_exact_size(vec2(W, H), Sense::click()) 让布局
            // 引擎自动放置——RTL 下布局引擎从右向左分配，自动靠右排列并左移
            // cursor，painter 按返回 rect 画图，不依赖 cursor().min。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let s = i18n::strings();

                // ── 窗控三按钮（最右，从右到左：关闭 → 最大化/还原 → 最小化）
                // ──────────────────────────────────────────────────────────────
                // macOS 用原生装饰（交通灯），此处不画自绘窗控，避免双套按钮；
                // Windows/Linux 无边框，由自绘顶栏承担窗控。
                if !cfg!(target_os = "macos") {

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

                } // end if !macOS（窗控三按钮）

                // ── 头像（紧贴窗控左侧，加右内边距 10px）──────────────────
                ui.add_space(10.0);
                let resp =
                    avatar_button(ui, profile, pal, view.update_version.is_some(), view.need_relogin);
                let update_version = view.update_version.as_deref();
                let need_relogin = view.need_relogin;
                let _ = egui::Popup::menu(&resp)
                    .align(egui::RectAlign::BOTTOM_END)
                    .width(MENU_WIDTH)
                    .show(|ui| menu_ui(ui, profile, pal, update_version, need_relogin, &mut out));
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
                ui.add_space(6.0);

                // ── 左端三视图按钮组（R8：移到最左；LTR 子布局占满余下左端空间）
                // 中间剩余空白区作为拖动区。
                // RTL 布局在此处 cursor 已经是右端按钮左侧；用 available_rect_before_wrap
                // 取整个余下区域，然后在里面画两层：
                //   1. LTR 子 ui 在左端按钮组
                //   2. interact 覆盖整个余下矩形作为拖动区
                let remaining = ui.available_rect_before_wrap();

                // 拖动区（双击/右键/拖动感知）——覆盖余下整个空白，含三视图按钮之间区域
                let drag_resp = ui.interact(
                    remaining,
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

                // LTR 子布局：在余下区域左端放三视图按钮组
                let mut left_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(remaining)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                left_ui.add_space(LEFT_GROUP_MARGIN);

                // ① 显示/隐藏会话栏（codicon layout-sidebar-left 风格）
                {
                    let (sb_rect, sb_resp) = left_ui.allocate_exact_size(
                        egui::vec2(VIEW_BTN_W, VIEW_BTN_H),
                        egui::Sense::click(),
                    );
                    draw_icon_sidebar(&left_ui, sb_rect, view.sidebar_visible, pal);
                    let tip1 = if view.sidebar_visible {
                        s.topbar_sidebar_hide_tip
                    } else {
                        s.topbar_sidebar_show_tip
                    };
                    if sb_resp.on_hover_text(tip1).clicked() {
                        out.toggle_sidebar = Some(!view.sidebar_visible);
                    }
                }

                left_ui.add_space(VIEW_BTN_GAP);

                // ② 显示/隐藏文件树（codicon list-tree 风格）
                {
                    let (ft_rect, ft_resp) = left_ui.allocate_exact_size(
                        egui::vec2(VIEW_BTN_W, VIEW_BTN_H),
                        egui::Sense::click(),
                    );
                    draw_icon_filetree(&left_ui, ft_rect, view.filetree_visible, pal);
                    let tip2 = if view.filetree_visible {
                        s.topbar_filetree_hide_tip
                    } else {
                        s.topbar_filetree_show_tip
                    };
                    if ft_resp.on_hover_text(tip2).clicked() {
                        out.toggle_filetree = Some(!view.filetree_visible);
                    }
                }

                left_ui.add_space(VIEW_BTN_GAP);

                // ③ 还原窗格大小（grid 图标，田字格风格）
                {
                    let enabled = pane_count > 1;
                    let (reset_rect, reset_resp) = left_ui.allocate_exact_size(
                        egui::vec2(VIEW_BTN_W, VIEW_BTN_H),
                        egui::Sense::click(),
                    );
                    draw_icon_grid(&left_ui, reset_rect, enabled, pal);
                    let tip3 = if pane_count > 1 {
                        s.topbar_reset_layout_tip
                    } else {
                        s.topbar_reset_layout_disabled_tip
                    };
                    if reset_resp.on_hover_text(tip3).clicked() && pane_count > 1 {
                        out.reset_layout = true;
                    }
                }

                // ── 本地/远程 tab（M5.2）：与三视图图标拉开间距 ──
                left_ui.add_space(VIEW_BTN_GAP * 3.0);
                if draw_view_tab(&mut left_ui, s.topbar_tab_local, !view.current_view, pal).clicked()
                {
                    out.toggle_view_mode = Some(false);
                }
                left_ui.add_space(VIEW_BTN_GAP);
                if draw_view_tab(&mut left_ui, s.topbar_tab_remote, view.current_view, pal).clicked()
                {
                    out.toggle_view_mode = Some(true);
                }
            });
        });
    out
}

// ── 图标绘制子函数（R8.2 精绘，统一规格）──────────────────────────────────
// 视觉盒 18×14 逻辑 px 居中于 28×26 热区（R8.2：从 16×12 扩大）；线宽 1.2；
// 颜色常态 fg_dim，hover fg；hover 圆角底 bg_highlight（圆角 4）。

/// ① 侧栏切换图标（codicon layout-sidebar-left 风格）：
/// 圆角外框 18×14（圆角 2.5）+ 距左 1/3 处竖分隔线；
/// 侧栏可见态左舱填充（fg_dim 40% 透明度），隐藏态仅线框。
fn draw_icon_sidebar(ui: &egui::Ui, rect: egui::Rect, visible: bool, pal: &Palette) {
    let painter = ui.painter();
    // 悬停底色
    if ui.rect_contains_pointer(rect) {
        painter.rect_filled(rect, 4.0, pal.bg_highlight);
    }
    let fg = if visible { pal.fg } else { pal.fg_dim };
    let stroke = egui::Stroke::new(1.2, fg);
    let c = rect.center();
    // 外框 18×14，圆角 2.5，像素对齐（R8.2：从 15×12 扩大）
    let bw = 18.0_f32;
    let bh = 14.0_f32;
    let ox = (c.x - bw / 2.0 + 0.5).floor() - 0.5; // round to 0.5
    let oy = (c.y - bh / 2.0 + 0.5).floor() - 0.5;
    let frame = egui::Rect::from_min_size(egui::pos2(ox, oy), egui::vec2(bw, bh));
    painter.rect_stroke(frame, 2.5, stroke, egui::StrokeKind::Middle);
    // 左 1/3 竖分隔线（距左缘约 bw/3）
    let div_x = (ox + bw / 3.0 + 0.5).floor() - 0.5;
    painter.line_segment(
        [
            egui::pos2(div_x, oy + 1.0),
            egui::pos2(div_x, oy + bh - 1.0),
        ],
        stroke,
    );
    // 可见态：左舱填充（fg_dim 40% 透明度的 rect）
    if visible {
        let fill_color = egui::Color32::from_rgba_unmultiplied(
            pal.fg_dim.r(),
            pal.fg_dim.g(),
            pal.fg_dim.b(),
            (pal.fg_dim.a() as f32 * 0.4) as u8,
        );
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(ox + 1.5, oy + 1.5),
                egui::pos2(div_x - 0.5, oy + bh - 1.5),
            ),
            1.5,
            fill_color,
        );
    }
}

/// ② 文件树切换图标（codicon list-tree 风格）：
/// 无外框；左侧竖干线（高 14）+ 向右三条横枝（y 均分，长度 9/6.5/4）。
/// 层次差拉大（9/6.5/4）使高 DPI 下层次可辨；可见态 fg，隐藏态 fg_dim。
fn draw_icon_filetree(ui: &egui::Ui, rect: egui::Rect, visible: bool, pal: &Palette) {
    let painter = ui.painter();
    if ui.rect_contains_pointer(rect) {
        painter.rect_filled(rect, 4.0, pal.bg_highlight);
    }
    let fg = if visible { pal.fg } else { pal.fg_dim };
    let stroke = egui::Stroke::new(1.2, fg);
    let c = rect.center();
    // R8.2：树形高度随视觉盒扩大到 14，竖干偏左 8px
    let tree_h = 14.0_f32;
    let trunk_x = (c.x - 8.0 + 0.5).floor() - 0.5; // 竖干 x，像素对齐
    let top_y = (c.y - tree_h / 2.0 + 0.5).floor() - 0.5;
    let bot_y = top_y + tree_h;
    // 竖干
    painter.line_segment(
        [egui::pos2(trunk_x, top_y), egui::pos2(trunk_x, bot_y)],
        stroke,
    );
    // 三条横枝（y 均分于 top+2/top+7/top+12；R8.2 长度差拉大 9/6.5/4）
    let branches: [(f32, f32); 3] = [(top_y + 2.0, 9.0), (top_y + 7.0, 6.5), (top_y + 12.0, 4.0)];
    for (by, branch_len) in branches {
        let by = (by + 0.5).floor() - 0.5;
        painter.line_segment(
            [
                egui::pos2(trunk_x, by),
                egui::pos2(trunk_x + branch_len, by),
            ],
            stroke,
        );
    }
}

/// ③ 还原窗格图标（田字格风格）：
/// 圆角外框 16×14（圆角 2.5）+ 内部横竖中线十字分隔（2×2 田字）。
/// 不画四个分离小方块（避免显碎）。R8.2：从 14×12 扩大。
fn draw_icon_grid(ui: &egui::Ui, rect: egui::Rect, enabled: bool, pal: &Palette) {
    let painter = ui.painter();
    let hovered = ui.rect_contains_pointer(rect);
    if hovered && enabled {
        painter.rect_filled(rect, 4.0, pal.bg_highlight);
    }
    let fg = if !enabled {
        pal.fg_dim.gamma_multiply(0.4)
    } else if hovered {
        pal.fg
    } else {
        pal.fg_dim
    };
    let stroke = egui::Stroke::new(1.2, fg);
    let c = rect.center();
    // R8.2：从 14×12 扩大到 16×14，比例对齐侧栏/树形框
    let bw = 16.0_f32;
    let bh = 14.0_f32;
    let ox = (c.x - bw / 2.0 + 0.5).floor() - 0.5;
    let oy = (c.y - bh / 2.0 + 0.5).floor() - 0.5;
    let frame = egui::Rect::from_min_size(egui::pos2(ox, oy), egui::vec2(bw, bh));
    // 圆角外框
    painter.rect_stroke(frame, 2.5, stroke, egui::StrokeKind::Middle);
    // 内部横中线
    let mid_y = (oy + bh / 2.0 + 0.5).floor() - 0.5;
    painter.line_segment(
        [
            egui::pos2(ox + 1.5, mid_y),
            egui::pos2(ox + bw - 1.5, mid_y),
        ],
        stroke,
    );
    // 内部竖中线
    let mid_x = (ox + bw / 2.0 + 0.5).floor() - 0.5;
    painter.line_segment(
        [
            egui::pos2(mid_x, oy + 1.5),
            egui::pos2(mid_x, oy + bh - 1.5),
        ],
        stroke,
    );
}

/// 本地/远程 tab 按钮（M5.2）：文字 pill。active = accent 字 + bg_highlight 底；
/// hover = fg 字 + 半透底；常态 = fg_dim 字。返回点击 Response。
fn draw_view_tab(ui: &mut egui::Ui, text: &str, active: bool, pal: &Palette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(40.0, VIEW_BTN_H), egui::Sense::click());
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 4.0, pal.bg_highlight);
    } else if resp.hovered() {
        painter.rect_filled(rect, 4.0, pal.bg_highlight.gamma_multiply(0.5));
    }
    let color = if active {
        pal.accent
    } else if resp.hovered() {
        pal.fg
    } else {
        pal.fg_dim
    };
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(12.5),
        color,
    );
    resp
}

/// 圆形头像按钮：已登录 = 强调色圆底 + 首字母；未登录 = 占位人形。
/// `need_relogin` 为真时右上角叠红色感叹号角标（登录过期，最优先）；否则 `has_update` 为真时叠小红点。
fn avatar_button(
    ui: &mut egui::Ui,
    profile: Option<&Profile>,
    pal: &Palette,
    has_update: bool,
    need_relogin: bool,
) -> egui::Response {
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
    // 右上角角标：登录过期（红圆 + 白「!」，最优先）> 有更新（小红点）。先垫一圈顶栏底色，
    // 确保在 accent 头像底 / 顶栏底上都清晰可辨。
    let red = egui::Color32::from_rgb(0xE5, 0x48, 0x4D);
    let badge = egui::pos2(center.x + radius * 0.66, center.y - radius * 0.66);
    if need_relogin {
        let r = 5.0;
        ui.painter().circle_filled(badge, r + 1.2, pal.bg_dark);
        ui.painter().circle_filled(badge, r, red);
        ui.painter().text(
            badge,
            egui::Align2::CENTER_CENTER,
            "!",
            egui::FontId::proportional(8.5),
            egui::Color32::WHITE,
        );
    } else if has_update {
        let dot_r = 3.5;
        ui.painter().circle_filled(badge, dot_r + 1.2, pal.bg_dark);
        ui.painter().circle_filled(badge, dot_r, red);
    }
    // 悬停提示：过期时提示重新登录。
    let hover = if need_relogin {
        i18n::strings().topbar_session_expired.to_owned()
    } else {
        profile.map_or_else(
            || i18n::strings().topbar_not_logged_in.to_owned(),
            |p| p.email.clone(),
        )
    };
    resp.on_hover_text(hover)
}

/// 头像下拉菜单（对齐 Warp 分组样式）：
/// 用户名 ┊ 更新组（更新到 vX / 检查更新 + 更新日志）┊ 设置组（设置 +
/// 键盘快捷键）┊ 资源组（文档 + 反馈）┊ 账号组（登录 / 退出登录）。
fn menu_ui(
    ui: &mut egui::Ui,
    profile: Option<&Profile>,
    pal: &Palette,
    update_version: Option<&str>,
    need_relogin: bool,
    out: &mut TopbarOutput,
) {
    let s = i18n::strings();
    // 强制菜单内容区最小宽度：`Popup::menu().width()` 仅作建议，内容窄时
    // 菜单仍按内容收窄（海风哥反馈菜单太窄即此因），这里硬撑到 MENU_WIDTH。
    ui.set_min_width(MENU_WIDTH);
    // 顶部：已登录展示名（灰字不可点）+ 分隔线。
    if let Some(p) = profile {
        ui.add_enabled(
            false,
            egui::Button::new(egui::RichText::new(&p.display_name).color(pal.fg_dim)),
        );
        ui.separator();
    }
    // 更新组：有就绪更新 →「更新到 vX」(强调色，打开更新弹窗)；否则
    // →「检查更新」(手动检查)。再加「更新日志」。
    if let Some(ver) = update_version {
        if ui
            .button(egui::RichText::new(i18n::fmt1(s.menu_update_to_fmt, ver)).color(pal.accent))
            .clicked()
        {
            out.open_update = true;
            ui.close();
        }
    } else if ui.button(s.menu_check_update).clicked() {
        out.check_update = true;
        ui.close();
    }
    if ui.button(s.menu_whats_new).clicked() {
        out.open_whats_new = true;
        ui.close();
    }
    ui.separator();
    // 设置组。
    if ui.button(s.menu_settings).clicked() {
        out.open_settings = true;
        ui.close();
    }
    if ui.button(s.menu_keyboard_shortcuts).clicked() {
        out.open_shortcuts = true;
        ui.close();
    }
    ui.separator();
    // 资源组：文档 / 反馈（打开 GitHub）。
    if ui.button(s.menu_documentation).clicked() {
        out.open_documentation = true;
        ui.close();
    }
    if ui.button(s.menu_feedback).clicked() {
        out.open_feedback = true;
        ui.close();
    }
    ui.separator();
    // 账号组：登录过期 → 红字「登录过期」（点此重登）置顶；再「登录 / 退出登录」。
    if need_relogin
        && profile.is_some()
        && ui
            .button(egui::RichText::new(s.menu_session_expired).color(pal.error))
            .clicked()
    {
        out.open_login = true;
        ui.close();
    }
    if profile.is_some() {
        if ui.button(s.menu_log_out).clicked() {
            out.log_out = true;
            ui.close();
        }
    } else if ui.button(s.menu_log_in).clicked() {
        out.open_login = true;
        ui.close();
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
                    update_version: None,
                    current_view: false,
                    need_relogin: false,
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
                    update_version: None,
                    current_view: false,
                    need_relogin: false,
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

    /// R8 新增：左端三视图按钮组哨兵——egui 跑一帧，左端区域（x<200）
    /// 应存在线段图元（三视图图标的竖干/框线/横枝）。
    #[test]
    fn 顶栏_左端视图按钮图元存在() {
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
                "标题",
                1,
                None,
                &pal,
                false,
                ViewState {
                    sidebar_visible: true,
                    filetree_visible: false,
                    update_version: None,
                    current_view: false,
                    need_relogin: false,
                },
            );
        });
        fn count_left_segments(shapes: &[egui::epaint::ClippedShape]) -> usize {
            let mut n = 0;
            for cs in shapes {
                n += walk_left(&cs.shape);
            }
            n
        }
        fn walk_left(s: &egui::epaint::Shape) -> usize {
            use egui::epaint::Shape;
            match s {
                Shape::LineSegment { points, .. } => {
                    // 左端按钮组：x < 200（三按钮组约在 10..130 范围内）
                    usize::from(points[0].x < 200.0 || points[1].x < 200.0)
                }
                Shape::Vec(v) => v.iter().map(walk_left).sum(),
                _ => 0,
            }
        }
        let segs = count_left_segments(&full.shapes);
        // 三视图图标含多条线段（①侧栏分隔线 + ②树形竖干+横枝 + ③田字中线）
        assert!(segs >= 4, "左端线段图元过少：{segs} 条——视图按钮没画到左端");
    }
}
