//! 登录界面（M3.5 / M5.1）：全屏覆盖层 + 居中卡片（与设置页同用 Modal）。
//!
//! **M5.1 起改为真账户登录**：提交后在后台线程发 REST（[`crate::cloud`]），
//! UI 显示「登录中」spinner、不阻塞帧；结果回传后成功则携带 [`Profile`] 关闭，
//! 失败显示错误。**新邮箱自动注册**（服务端返回 `user_not_found` 时先注册再
//! 登录）。密码仅在提交瞬间过手、绝不落盘。
//!
//! 异步全封装在本模块内：[`show`] 在登录成功的那一帧才返回 `logged_in`，
//! 外层 shell / main 的消费路径与 mock 时代一致、无需改动。

use std::sync::mpsc::{self, Receiver};

use lumen_protocol::{DeviceInfo, LoginRequest};

use crate::cloud::{self, CloudClient, CloudError};
use crate::i18n;
use crate::profile::{LoginError, Profile};

use super::theme::Palette;

/// 居中卡片宽度（逻辑像素）。
const CARD_WIDTH: f32 = 320.0;

/// 后台登录结果：成功 = 档案；失败 = 用户可见错误文案。
type LoginResult = Result<Profile, String>;

/// 登录界面的跨帧状态。
#[derive(Default)]
pub struct LoginUiState {
    /// 是否打开（打开期间终端聚焦布尔置 false，键盘归 egui）。
    pub open: bool,
    /// 邮箱输入缓冲。
    email: String,
    /// 密码输入缓冲（仅内存，关闭即清空，绝不落盘）。
    password: String,
    /// 本地校验失败枚举（F6）。
    error: Option<LoginError>,
    /// 服务端/网络错误文案（与本地校验 `error` 二选一显示）。
    server_error: Option<String>,
    /// 刚打开：本帧把焦点交给邮箱输入框。
    focus_email: bool,
    /// 后台登录进行中（输入禁用 + spinner）。
    submitting: bool,
    /// 后台线程结果接收端。
    rx: Option<Receiver<LoginResult>>,
}

impl LoginUiState {
    /// 打开登录界面（清空上次残留）。
    pub fn open_clean(&mut self) {
        self.reset();
        self.open = true;
        self.focus_email = true;
    }

    /// 清空输入/错误/在途请求（关闭或成功后调用）。
    fn reset(&mut self) {
        self.email.clear();
        self.password.clear();
        self.error = None;
        self.server_error = None;
        self.focus_email = false;
        self.submitting = false;
        self.rx = None;
    }
}

/// 一帧登录界面的产出。
#[derive(Default)]
pub struct LoginOutput {
    /// 覆盖层应关闭（Esc / ✕ / 登录成功）。
    pub closed: bool,
    /// 登录成功的档案（main 写盘并更新全局登录态）。
    pub logged_in: Option<Profile>,
}

/// 绘制登录覆盖层。调用方保证 `st.open == true` 时才调用。
pub fn show(ctx: &egui::Context, st: &mut LoginUiState, pal: &Palette) -> LoginOutput {
    // —— 1. 先收后台登录结果（借用先于可变操作，避免借用冲突）——
    let polled = if st.submitting {
        st.rx.as_ref().map(Receiver::try_recv)
    } else {
        None
    };
    if let Some(res) = polled {
        match res {
            Ok(Ok(profile)) => {
                // 成功：重置并携带档案关闭。
                st.reset();
                return LoginOutput {
                    closed: true,
                    logged_in: Some(profile),
                };
            }
            Ok(Err(msg)) => {
                st.submitting = false;
                st.rx = None;
                st.server_error = Some(msg);
            }
            Err(mpsc::TryRecvError::Empty) => {
                // 仍在进行：保持重绘以便下一帧继续轮询。
                ctx.request_repaint();
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                st.submitting = false;
                st.rx = None;
                st.server_error = Some("登录线程异常退出".to_string());
            }
        }
    }

    let mut out = LoginOutput::default();
    let modal = egui::Modal::new(egui::Id::new("lumen_login_modal"))
        .backdrop_color(egui::Color32::from_black_alpha(120))
        .frame(
            egui::Frame::new()
                .fill(pal.bg_panel)
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::same(24)),
        )
        .show(ctx, |ui| {
            ui.set_width(CARD_WIDTH);

            // 右上 ✕。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                let close =
                    egui::Button::new(egui::RichText::new("✕").size(14.0).color(pal.fg_dim))
                        .fill(egui::Color32::TRANSPARENT);
                if ui.add(close).clicked() {
                    out.closed = true;
                }
            });

            let s = i18n::strings();
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("Lumen").size(22.0).strong().color(pal.fg));
                ui.add_space(2.0);
                ui.label(egui::RichText::new(s.login_subtitle).size(11.0).color(pal.fg_dim));
            });
            ui.add_space(16.0);

            let enabled = !st.submitting;
            let email_edit = ui.add_enabled(
                enabled,
                egui::TextEdit::singleline(&mut st.email)
                    .hint_text(s.login_email_hint)
                    .desired_width(f32::INFINITY),
            );
            if st.focus_email {
                email_edit.request_focus();
                st.focus_email = false;
            }
            ui.add_space(8.0);
            let pwd_edit = ui.add_enabled(
                enabled,
                egui::TextEdit::singleline(&mut st.password)
                    .password(true)
                    .hint_text(s.login_password_hint)
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(14.0);

            // 任一输入框内回车等同点击登录。
            let submitted_by_enter = enabled
                && (email_edit.lost_focus() || pwd_edit.lost_focus())
                && ui.input(|i| i.key_pressed(egui::Key::Enter));

            let login_btn = egui::Button::new(
                egui::RichText::new(s.login_btn).size(13.0).color(pal.accent_fg),
            )
            .fill(pal.accent)
            .min_size(egui::vec2(ui.available_width(), 32.0));
            let clicked = ui.add_enabled(enabled, login_btn).clicked();

            if st.submitting {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new("登录中…").size(11.5).color(pal.fg_dim),
                    );
                });
            }

            if (clicked || submitted_by_enter) && !st.submitting {
                start_login(ctx, st);
            }

            // 错误显示（服务端错误优先于本地校验错误）。
            let err_text = st
                .server_error
                .clone()
                .or_else(|| st.error.map(|e| e.message().to_string()));
            if let Some(e) = err_text {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(e).size(11.5).color(pal.error));
            }
        });

    // Esc / backdrop 点击 → 关闭（提交中也允许关闭：丢弃 rx，后台结果被忽略）。
    if modal.should_close() {
        out.closed = true;
    }
    if out.closed {
        st.reset();
    }
    out
}

/// 本地校验通过后启动后台登录线程。
fn start_login(ctx: &egui::Context, st: &mut LoginUiState) {
    let email = st.email.trim().to_string();
    let password = st.password.clone();
    // 本地校验：邮箱含 `@`（前后段非空）、密码非空。
    let valid_email = email
        .split_once('@')
        .is_some_and(|(u, h)| !u.is_empty() && !h.is_empty());
    if !valid_email {
        st.error = Some(LoginError::InvalidEmail);
        st.server_error = None;
        return;
    }
    if password.is_empty() {
        st.error = Some(LoginError::EmptyPassword);
        st.server_error = None;
        return;
    }
    st.error = None;
    st.server_error = None;

    let (tx, rx) = mpsc::channel();
    st.rx = Some(rx);
    st.submitting = true;
    let ctx2 = ctx.clone();
    std::thread::spawn(move || {
        let result = do_login(&email, &password);
        // 发送失败（UI 已关闭丢弃 rx）忽略即可。
        let _ = tx.send(result);
        ctx2.request_repaint();
    });
}

/// 后台执行：新邮箱自动注册，然后登录；构造 [`Profile`]。
fn do_login(email: &str, password: &str) -> LoginResult {
    let client = CloudClient::new(cloud::server_url());
    let device = DeviceInfo {
        device_id: cloud::load_device_id(),
        name: cloud::device_name(),
        os: std::env::consts::OS.to_string(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let req = LoginRequest {
        email: email.to_string(),
        password: password.to_string(),
        device,
    };
    let resp = match client.login(&req) {
        Ok(r) => r,
        // 新邮箱：服务端无此账户 → 自动注册后再登录。
        Err(CloudError::Api { code, .. }) if code == "user_not_found" => {
            // 新邮箱：自动注册后再登录。若注册撞上 email_taken（并发/竞态），
            // 直接转登录，让最终错误回落到真正成因（如密码错误）。
            match client.register(email, password) {
                Ok(_) => {}
                Err(CloudError::Api { code, .. }) if code == "email_taken" => {}
                Err(e) => return Err(e.user_message()),
            }
            client.login(&req).map_err(|e| e.user_message())?
        }
        Err(e) => return Err(e.user_message()),
    };
    cloud::save_device_id(&resp.device_id);
    Ok(Profile::from_auth(resp))
}
