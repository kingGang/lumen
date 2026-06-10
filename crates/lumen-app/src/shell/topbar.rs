//! 顶栏（M3.5）：左侧激活会话标题 + 右侧圆形头像按钮与下拉菜单。
//!
//! 规格（docs/M3应用外壳设计.md §4 第④行，参考截图
//! docs/截图/用户头像下拉弹窗.png）：未登录头像为占位人形图标，已
//! 登录为强调色圆底 + 展示名首字母；点击弹下拉菜单——已登录：展示名
//! （灰字不可点）/ Settings / Keyboard shortcuts / Documentation
//! （灰显占位）/ 分隔线 / Log out；未登录：Log in / Settings /
//! Keyboard shortcuts。UI 只产出动作（[`TopbarOutput`]），登录/登出
//! 与设置页打开由上层执行。

use crate::profile::Profile;

use super::theme::Palette;

/// 顶栏高度（逻辑像素）。加入后终端区高度变化走既有的
/// 「矩形变化 → 重建离屏纹理 + 全会话 resize」链路。
pub const HEIGHT: f32 = 34.0;
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
}

/// 绘制顶栏（全宽窄条；须先于侧栏加入面板布局才能横贯整窗）。
/// `title` 为激活会话标题，与窗口标题同源（main 的 display_title）。
pub fn show(
    root: &mut egui::Ui,
    title: &str,
    profile: Option<&Profile>,
    pal: &Palette,
) -> TopbarOutput {
    let mut out = TopbarOutput::default();
    egui::Panel::top("lumen_topbar")
        .exact_size(HEIGHT)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(pal.bg_dark)
                .inner_margin(egui::Margin::symmetric(10, 0)),
        )
        .show_inside(root, |ui| {
            // 头像钉在最右（right_to_left），余下空间给标题截断展示。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let resp = avatar_button(ui, profile, pal);
                let _ = egui::Popup::menu(&resp)
                    .align(egui::RectAlign::BOTTOM_END)
                    .width(MENU_WIDTH)
                    .show(|ui| menu_ui(ui, profile, pal, &mut out));
                ui.add_space(4.0);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(title).size(12.0).color(pal.fg_dim),
                        )
                        .truncate(),
                    );
                });
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
            ui.painter().circle_filled(center, radius, pal.accent);
            ui.painter().text(
                center,
                egui::Align2::CENTER_CENTER,
                p.avatar_letter(),
                egui::FontId::proportional(13.0),
                pal.bg_dark,
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
    resp.on_hover_text(profile.map_or_else(|| "未登录".to_owned(), |p| p.email.clone()))
}

/// 下拉菜单内容（参照截图，按登录态二选一）。
fn menu_ui(ui: &mut egui::Ui, profile: Option<&Profile>, pal: &Palette, out: &mut TopbarOutput) {
    match profile {
        Some(p) => {
            // 首行展示名：灰字不可点（参照截图 Jimhy Liu 行）。
            ui.add_enabled(
                false,
                egui::Button::new(
                    egui::RichText::new(&p.display_name).color(pal.fg_dim),
                ),
            );
            if ui.button("Settings").clicked() {
                out.open_settings = true;
                ui.close();
            }
            if ui.button("Keyboard shortcuts").clicked() {
                out.open_shortcuts = true;
                ui.close();
            }
            // Documentation 灰显占位（本期无文档站，§7 不做清单）。
            ui.add_enabled(false, egui::Button::new("Documentation"));
            ui.separator();
            if ui.button("Log out").clicked() {
                out.log_out = true;
                ui.close();
            }
        }
        None => {
            if ui.button("Log in").clicked() {
                out.open_login = true;
                ui.close();
            }
            if ui.button("Settings").clicked() {
                out.open_settings = true;
                ui.close();
            }
            if ui.button("Keyboard shortcuts").clicked() {
                out.open_shortcuts = true;
                ui.close();
            }
        }
    }
}
