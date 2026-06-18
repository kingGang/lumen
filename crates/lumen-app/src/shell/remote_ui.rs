//! M5.3 终端远程 · part2b：远程控制配对 / 被控 UI。
//!
//! 三个 UI 元件，数据源是 `crate::remote_ws::RemoteWs` 的状态（经 [`super::ShellInput`]
//! 传入），动作经 [`RemoteUiOutput`] 回传 main → 调 `RemoteWs` 方法：
//!
//! - **配对模态**（[`pairing_modal`]，控制端）：发起控制后，输入被控端展示的 9 位
//!   配对码。仿 `login_ui` 的居中 `egui::Modal`。
//! - **顶部横幅**（[`banner`]，被控端 / 控制端）：被控端展示来件控制请求 + 配对码 +
//!   「拒绝」，被控期间醒目红条「正在被远程控制」+「断开」；控制端展示「正在控制 X」
//!   +「断开」。顶部居中 overlay，不挤压三栏布局。

use lumen_protocol::remote::{PairingFailReason, Role};

use crate::i18n;
use crate::remote_ws::{ActiveSession, IncomingControl, PairingPrompt};

use super::theme::Palette;

/// 配对码输入弹窗宽度（逻辑像素）。
const CARD_WIDTH: f32 = 300.0;

/// 配对码位数（服务端固定 9 位）。
const CODE_LEN: usize = 9;

/// 配对 UI 的跨帧状态（输入缓冲 + 焦点；存于 [`super::ShellState`]）。
#[derive(Default)]
pub struct RemoteUiState {
    /// 模态当前是否打开（用于检测首次出现以聚焦/清空）。
    open: bool,
    /// 刚打开：下一帧把焦点交给配对码输入框。
    focus: bool,
    /// 配对码输入缓冲（仅数字、≤9 位）。
    code: String,
}

impl RemoteUiState {
    /// 配对模态消失时复位（清空输入、关闭）。
    pub fn reset(&mut self) {
        self.open = false;
        self.focus = false;
        self.code.clear();
    }

    /// 标记配对模态首次出现：聚焦输入框、清空残留。
    fn ensure_open(&mut self) {
        if !self.open {
            self.open = true;
            self.focus = true;
            self.code.clear();
        }
    }
}

/// 远程 UI 一帧的产出（main 据此调 `RemoteWs`）。
#[derive(Default)]
pub struct RemoteUiOutput {
    /// 控制端提交配对码。
    pub submit_code: Option<String>,
    /// 控制端取消配对（关弹窗）。
    pub cancel_pairing: bool,
    /// 被控端拒绝来件控制请求。
    pub decline: bool,
    /// 任一端结束当前会话。
    pub end_session: bool,
}

/// 绘制控制端配对码输入模态。调用方保证 `prompt` 存在时才调用。
pub fn pairing_modal(
    ctx: &egui::Context,
    st: &mut RemoteUiState,
    prompt: &PairingPrompt,
    pal: &Palette,
) -> RemoteUiOutput {
    st.ensure_open();
    let mut out = RemoteUiOutput::default();
    let s = i18n::strings();

    let modal = egui::Modal::new(egui::Id::new("lumen_pairing_modal"))
        .backdrop_color(egui::Color32::from_black_alpha(120))
        .frame(
            egui::Frame::new()
                .fill(pal.bg_panel)
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::same(24)),
        )
        .show(ctx, |ui| {
            ui.set_width(CARD_WIDTH);
            ui.label(
                egui::RichText::new(s.remote_pairing_title)
                    .size(16.0)
                    .strong()
                    .color(pal.fg),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(i18n::fmt1(s.remote_pairing_prompt_fmt, &prompt.target_name))
                    .size(11.5)
                    .color(pal.fg_dim),
            );
            ui.add_space(14.0);

            let edit = ui.add(
                egui::TextEdit::singleline(&mut st.code)
                    .hint_text(s.remote_pairing_hint)
                    .desired_width(f32::INFINITY)
                    .char_limit(CODE_LEN),
            );
            if st.focus {
                edit.request_focus();
                st.focus = false;
            }
            // 只保留数字，最长 9 位。
            st.code.retain(|c| c.is_ascii_digit());
            st.code.truncate(CODE_LEN);
            let submitted_by_enter =
                edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

            // 上次错误提示（剩余次数）。
            if matches!(prompt.last_error, Some(PairingFailReason::InvalidCode)) {
                if let Some(left) = prompt.attempts_left {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(i18n::fmt1(s.remote_pairing_invalid_fmt, left))
                            .size(11.5)
                            .color(pal.error),
                    );
                }
            }
            ui.add_space(14.0);

            let ready = st.code.len() == CODE_LEN;
            ui.horizontal(|ui| {
                let connect = egui::Button::new(
                    egui::RichText::new(s.remote_pairing_connect)
                        .size(13.0)
                        .color(pal.accent_fg),
                )
                .fill(pal.accent)
                .min_size(egui::vec2(120.0, 30.0));
                if ui.add_enabled(ready, connect).clicked() || (submitted_by_enter && ready) {
                    out.submit_code = Some(st.code.clone());
                    st.code.clear(); // 清空以便被拒后重输
                }
                if ui
                    .button(egui::RichText::new(s.remote_pairing_cancel).size(13.0))
                    .clicked()
                {
                    out.cancel_pairing = true;
                }
            });
        });

    // Esc / backdrop 点击 = 取消配对。
    if modal.should_close() {
        out.cancel_pairing = true;
    }
    out
}

/// 绘制顶部远程状态横幅（被控来件 / 被控中 / 控制中）。无相关态时不画。
pub fn banner(
    ctx: &egui::Context,
    incoming: Option<&IncomingControl>,
    session: Option<&ActiveSession>,
    pal: &Palette,
) -> RemoteUiOutput {
    let mut out = RemoteUiOutput::default();
    if incoming.is_none() && session.is_none() {
        return out;
    }
    let s = i18n::strings();

    egui::Area::new(egui::Id::new("lumen_remote_banner"))
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 42.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            // 横幅描边色按语义：来件/被控 = 警示，控制中 = 强调。
            let accent = if incoming.is_some() {
                pal.warn
            } else if session.is_some_and(|x| x.role == Role::Controlled) {
                pal.error
            } else {
                pal.accent
            };
            egui::Frame::new()
                .fill(pal.bg_panel)
                .stroke(egui::Stroke::new(1.5, accent))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if let Some(inc) = incoming {
                            ui.label(
                                egui::RichText::new(i18n::fmt1(
                                    s.remote_incoming_fmt,
                                    &inc.controller_name,
                                ))
                                .size(13.0)
                                .color(pal.fg),
                            );
                            ui.add_space(10.0);
                            ui.label(
                                egui::RichText::new(format!("{}: ", s.remote_incoming_code))
                                    .size(12.0)
                                    .color(pal.fg_dim),
                            );
                            ui.label(
                                egui::RichText::new(group_code(&inc.pairing_code))
                                    .size(16.0)
                                    .strong()
                                    .monospace()
                                    .color(accent),
                            );
                            ui.add_space(10.0);
                            if ui.button(s.remote_decline).clicked() {
                                out.decline = true;
                            }
                        } else if let Some(sess) = session {
                            let (text, color) = match sess.role {
                                Role::Controlled => (
                                    i18n::fmt1(s.remote_being_controlled_fmt, &sess.peer_name),
                                    pal.error,
                                ),
                                Role::Controller => (
                                    i18n::fmt1(s.remote_controlling_fmt, &sess.peer_name),
                                    pal.fg,
                                ),
                            };
                            ui.label(egui::RichText::new(text).size(13.0).strong().color(color));
                            ui.add_space(12.0);
                            if ui.button(s.remote_disconnect).clicked() {
                                out.end_session = true;
                            }
                        }
                    });
                });
        });
    out
}

/// 在终端工作区 `rect` 上绘制控制端**远程镜像**（part3a 纯文本视图：等宽行 +
/// 光标块；part3b 升级为 wgpu 上色渲染）。用 Middle 层 painter 叠在本地窗格之上、
/// 模态/toast 之下；display-only（输入捕获是 part4）。
pub fn paint_mirror(
    ui: &egui::Ui,
    rect: egui::Rect,
    view: &crate::remote_mirror::MirrorView,
    pal: &Palette,
) {
    let painter = ui
        .ctx()
        .layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("lumen_remote_mirror"),
        ))
        .with_clip_rect(rect);
    painter.rect_filled(rect, 0.0, pal.bg_dark);

    let font = egui::FontId::monospace(13.0);
    // 量一个等宽字符的尺寸（避免 egui Fonts 的 &mut 取用限制）。
    let sample = painter.layout_no_wrap("M".to_owned(), font.clone(), pal.fg);
    let char_w = sample.size().x.max(1.0);
    let line_h = sample.size().y.max(1.0);
    let pad = 4.0;
    // 显示区能容纳的行/列上限：被控端可能比控制端窗口宽/高，超出的不绘制
    // （主动限制——否则 egui 仍会对整行布局再被 clip 裁掉，纯浪费 CPU）。
    let max_rows = (((rect.height() - 2.0 * pad) / line_h).floor()).max(0.0) as usize;
    let max_cols = (((rect.width() - 2.0 * pad) / char_w).floor()).max(0.0) as usize;

    for (i, line) in view.lines.iter().take(max_rows).enumerate() {
        if line.is_empty() {
            continue;
        }
        // 按可见列数截断，避免把超宽整行丢给 painter 布局。
        let shown: String = line.chars().take(max_cols).collect();
        if shown.is_empty() {
            continue;
        }
        let pos = egui::pos2(rect.left() + pad, rect.top() + pad + i as f32 * line_h);
        painter.text(pos, egui::Align2::LEFT_TOP, shown, font.clone(), pal.fg);
    }

    // 光标块（半透明 accent）：行、列都在可见范围内才画。
    let (cr, cc) = view.cursor;
    if cr < max_rows && cc < max_cols {
        let cx = rect.left() + pad + cc as f32 * char_w;
        let cy = rect.top() + pad + cr as f32 * line_h;
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(cx, cy),
                egui::vec2(char_w.max(2.0), line_h),
            ),
            0.0,
            pal.accent.gamma_multiply(0.5),
        );
    }
}

/// 把 9 位配对码按「3 3 3」分组便于读出（如 `123456789` → `123 456 789`）。
fn group_code(code: &str) -> String {
    code.chars()
        .enumerate()
        .flat_map(|(i, c)| {
            if i > 0 && i % 3 == 0 {
                vec![' ', c]
            } else {
                vec![c]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 配对码分组() {
        assert_eq!(group_code("123456789"), "123 456 789");
        assert_eq!(group_code("12"), "12");
        assert_eq!(group_code(""), "");
    }

    #[test]
    fn 状态复位() {
        let mut st = RemoteUiState::default();
        st.ensure_open();
        st.code.push_str("123");
        assert!(st.open);
        st.reset();
        assert!(!st.open && st.code.is_empty());
    }
}
