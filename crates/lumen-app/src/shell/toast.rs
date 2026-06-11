//! 系统提示框（toast，需求池 F2）：右下角浮层堆叠的轻量通知。
//!
//! 用法（批次B 文件树操作反馈等场景直接消费）：
//! 1. 任意拿得到 [`super::ShellState`] 的地方调 [`ToastState::push`]
//!    推入提示（egui 帧内 push 的当帧即可见）；
//! 2. [`show`] 已挂在 `shell::show` 末尾每帧调用，新场景无需额外接线；
//! 3. 在 egui 帧外（main.rs 的事件处理里）push 后需 `request_redraw`，
//!    否则提示要等下一个无关事件触发重绘才显示。
//!
//! 展示规则：锚定窗口右下角，多条自下而上堆叠（最新的贴近角落）；
//! 按分级自动消失（Info 3s / Warn 5s / Error 8s）；显示期间用
//! `request_repaint_after` 安排到期重绘（与 main.rs 的 egui 重绘计划
//! 合流），全部消失后不再请求——不引入空转。

use std::time::{Duration, Instant};

use super::theme;

/// 同屏最多保留的提示条数（超出丢最旧，防事故场景刷屏占满屏幕）。
const MAX_TOASTS: usize = 6;
/// 浮层距窗口右下角的边距（逻辑像素）。
const MARGIN: f32 = 12.0;
/// 单条提示的最大宽度（逻辑像素，超长文本自动换行）。
const MAX_WIDTH: f32 = 320.0;

/// 提示分级（决定配色与展示时长）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    /// 一般信息（中性白灰/深灰，3 秒；M3.7b 黑白化前为青色）。
    Info,
    /// 警告（黄，5 秒）。
    Warn,
    /// 错误（红，8 秒）。
    Error,
}

impl ToastKind {
    /// 展示时长（到点自动消失）。
    fn duration(self) -> Duration {
        match self {
            Self::Info => Duration::from_secs(3),
            Self::Warn => Duration::from_secs(5),
            Self::Error => Duration::from_secs(8),
        }
    }

    /// 分级配色（图标与描边用；Info 为中性灰阶、Warn/Error 保留
    /// 语义黄/红，见 shell/theme.rs）。
    fn color(self, pal: &theme::Palette) -> egui::Color32 {
        match self {
            Self::Info => pal.info,
            Self::Warn => pal.warn,
            Self::Error => pal.error,
        }
    }

    /// 分级图标字符。
    fn icon(self) -> &'static str {
        match self {
            Self::Info => "ℹ",
            Self::Warn => "⚠",
            Self::Error => "✕",
        }
    }
}

/// 一条在显示的提示。
struct Toast {
    kind: ToastKind,
    text: String,
    /// 到期时刻（[`show`] 每帧按它清理过期条目）。
    expires_at: Instant,
}

/// toast 队列（挂在 [`super::ShellState`] 上跨帧保留）。
#[derive(Default)]
pub struct ToastState {
    /// 存活的提示，按推入顺序（旧在前）。
    toasts: Vec<Toast>,
}

impl ToastState {
    /// 推入一条提示，按分级自动消失（Info 3s / Warn 5s / Error 8s）。
    ///
    /// 超出 [`MAX_TOASTS`] 时丢弃最旧的一条。在 egui 帧外（事件处理
    /// 中）调用后记得 `request_redraw`，见模块文档。
    pub fn push(&mut self, kind: ToastKind, text: impl Into<String>) {
        if self.toasts.len() >= MAX_TOASTS {
            self.toasts.remove(0);
        }
        self.toasts.push(Toast {
            kind,
            text: text.into(),
            expires_at: Instant::now() + kind.duration(),
        });
    }
}

/// 绘制提示浮层（`shell::show` 每帧末尾调用 = 叠在一切覆盖层之上）。
///
/// 有存活提示时以 `request_repaint_after` 安排最近一条到期时的重绘；
/// 浮层不可交互（`interactable(false)`），不影响终端区鼠标路由
/// （main.rs `mouse_in_term` 的 layer 命中只看可交互层）。
pub fn show(ctx: &egui::Context, st: &mut ToastState, pal: &theme::Palette) {
    let now = Instant::now();
    st.toasts.retain(|t| now < t.expires_at);
    let Some(next_expire) = st.toasts.iter().map(|t| t.expires_at).min() else {
        return; // 无提示：不画，也不再安排重绘（空闲零开销）
    };
    ctx.request_repaint_after(next_expire - now);

    egui::Area::new(egui::Id::new("lumen_toast_layer"))
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-MARGIN, -MARGIN))
        .order(egui::Order::Foreground)
        .interactable(false)
        .show(ctx, |ui| {
            // 自下而上堆叠：最新的一条贴近角落，旧的被顶上去。
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
                for t in st.toasts.iter().rev() {
                    let color = t.kind.color(pal);
                    egui::Frame::new()
                        .fill(pal.bg_panel)
                        .stroke(egui::Stroke::new(1.0, color))
                        .corner_radius(egui::CornerRadius::same(6))
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.set_max_width(MAX_WIDTH);
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(t.kind.icon()).color(color));
                                ui.label(egui::RichText::new(&t.text).color(pal.fg));
                            });
                        });
                }
            });
        });
}
