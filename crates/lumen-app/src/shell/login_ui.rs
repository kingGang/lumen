//! 登录界面（M3.5）：全屏覆盖层 + 居中卡片（与设置页同用 Modal）。
//!
//! mock 校验在 [`crate::profile::mock_login`]（邮箱含 `@` 且密码非空
//! 即成功，展示名取 `@` 前段），**密码仅用于校验、绝不落盘**。打开
//! 期间键盘归 egui（main 把终端聚焦布尔置 false），Esc / ✕ 关闭，
//! 关闭后焦点与 IME 复位链路与设置页同款。真账号后端 M5 接入。

use crate::profile::{self, Profile};

use super::theme::Palette;

/// 居中卡片宽度（逻辑像素）。
const CARD_WIDTH: f32 = 320.0;

/// 登录界面的跨帧状态。
#[derive(Default)]
pub struct LoginUiState {
    /// 是否打开（打开期间终端聚焦布尔置 false，键盘归 egui）。
    pub open: bool,
    /// 邮箱输入缓冲。
    email: String,
    /// 密码输入缓冲（仅内存，关闭即清空，绝不落盘）。
    password: String,
    /// 校验失败的红字提示。
    error: Option<String>,
    /// 刚打开：本帧把焦点交给邮箱输入框。
    focus_email: bool,
}

impl LoginUiState {
    /// 打开登录界面（清空上次残留的输入与错误）。
    pub fn open_clean(&mut self) {
        self.open = true;
        self.email.clear();
        self.password.clear();
        self.error = None;
        self.focus_email = true;
    }
}

/// 一帧登录界面的产出。
#[derive(Default)]
pub struct LoginOutput {
    /// 覆盖层应关闭（Esc / ✕ / 登录成功）。
    pub closed: bool,
    /// mock 登录成功的档案（main 写盘并更新全局登录态）。
    pub logged_in: Option<Profile>,
}

/// 绘制登录覆盖层。调用方保证 `st.open == true` 时才调用。
pub fn show(ctx: &egui::Context, st: &mut LoginUiState, pal: &Palette) -> LoginOutput {
    let mut out = LoginOutput::default();
    let modal = egui::Modal::new(egui::Id::new("lumen_login_modal"))
        // 半透明 backdrop 压暗下层（终端在其下照常消化与渲染）。
        .backdrop_color(egui::Color32::from_black_alpha(120))
        .frame(
            egui::Frame::new()
                .fill(pal.bg_panel)
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::same(24)),
        )
        .show(ctx, |ui| {
            ui.set_width(CARD_WIDTH);

            // 右上 ✕（独立一行右对齐，不挤压标题居中）。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                let close =
                    egui::Button::new(egui::RichText::new("✕").size(14.0).color(pal.fg_dim))
                        .fill(egui::Color32::TRANSPARENT);
                if ui.add(close).clicked() {
                    out.closed = true;
                }
            });

            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("Lumen")
                        .size(22.0)
                        .strong()
                        .color(pal.fg),
                );
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new("登录（本地模拟，真账号后续版本接入）")
                        .size(11.0)
                        .color(pal.fg_dim),
                );
            });
            ui.add_space(16.0);

            let email_edit = ui.add(
                egui::TextEdit::singleline(&mut st.email)
                    .hint_text("邮箱")
                    .desired_width(f32::INFINITY),
            );
            if st.focus_email {
                email_edit.request_focus();
                st.focus_email = false;
            }
            ui.add_space(8.0);
            let pwd_edit = ui.add(
                egui::TextEdit::singleline(&mut st.password)
                    .password(true)
                    .hint_text("密码")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(14.0);

            // 任一输入框内按 Enter 等同点击登录按钮。
            let submitted = (email_edit.lost_focus() || pwd_edit.lost_focus())
                && ui.input(|i| i.key_pressed(egui::Key::Enter));
            // 主操作按钮 = Warp CTA 形态：accent 实底 + 反相文字
            // （深色主题白底黑字，浅色主题近黑底白字，M3.7b）。
            let login_btn =
                egui::Button::new(egui::RichText::new("登录").size(13.0).color(pal.accent_fg))
                    .fill(pal.accent)
                    .min_size(egui::vec2(ui.available_width(), 32.0));
            if ui.add(login_btn).clicked() || submitted {
                match profile::mock_login(&st.email, &st.password) {
                    Ok(p) => {
                        st.error = None;
                        out.logged_in = Some(p);
                    }
                    Err(msg) => st.error = Some(msg),
                }
            }
            if let Some(err) = &st.error {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(err).size(11.5).color(pal.error));
            }
        });
    // Esc（顶层 modal 时）或 backdrop 点击 → 关闭；登录成功也关闭。
    if modal.should_close() || out.logged_in.is_some() {
        out.closed = true;
    }
    if out.closed {
        // 关闭即清空输入与错误（密码不在内存滞留）。
        st.email.clear();
        st.password.clear();
        st.error = None;
    }
    out
}
